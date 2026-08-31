//! Whole-route CBOR (RFC 8949) decoder and semantic document construction.
//!
//! The decoder is a straight-line, frame-driven state machine over the borrowed source: each loop head admits a bounded
//! work quantum against the request's [`jqf_resource::WorkMeter`] and replenishes the cooperative budget when it is
//! exhausted. It validates the complete generic-data-model grammar — well-formedness (RFC 8949 §5.6.2), text UTF-8
//! (§3.2.4), map-key uniqueness per §5.6.1, and the recognized-tag content laws — and builds the format-neutral
//! semantic `Document` in one pass.
//!
//! Tag chains are represented with the jqf-data tag-layer law: an uninterpreted tag (any tag outside the recognized
//! 0–5) wraps its payload through a kindless [`AccountedSemanticNode::Unrepresentable`] node carrying
//! `Tagged("cbor:tag:<n>")` and one `cbor.tag-payload@1` occurrence, so a chain materializes as nested `Value::Tagged`.
//! Recognized tags 0–5 parse their payload into a bounded [`crate::equality::KeyValue`] first (the same structural
//! parser map keys use), validate the raw-shape law, and project when the value is exactly representable in core.
//!
//! Object projection is available only for maps whose keys are unique direct UTF-8 text strings; any other map is
//! non-projectable and fails the whole-document route (which promises a semantic root) with `UnrepresentableSemantic`.
//! The failure is immediate, at the first non-text key before the rest of that map is decoded; the located routes close
//! the gap by retaining the topology.

use alloc::vec::Vec;
use jqf_codec_core::{
    AccessInput, AccessOutcome, AccessResult, AccessSession, CodecError, CodecFailureKind, CodecRunContext,
    DocumentProduct,
};
use jqf_data::{
    AccountedDocumentBuilder, AccountedDocumentFinalizer, AccountedIntrinsicTag, AccountedOccurrenceKey,
    AccountedSemanticNode, AuthoritativeEmptyFamilies, BuilderCoverage, ContainerSpanKind, DataError,
    DiagnosticCoverage, Document, DocumentCapabilityFamily, DocumentCapacity, DocumentFinalizationPoll,
    DocumentSchemaRecipe, DocumentSourceBindingPoll, DocumentSourceBindingStage, LazySpanMaterializer, LocalOwnerRef,
    NodeId, PreparedDocumentSchema, ValueKind,
};
use jqf_resource::{DepthGuard, OwnedDepthGuard, ResourceContext, WorkAdmission};
use jqf_source::ResolvedSource;

use crate::big::Big;
use crate::datetime::{Rfc3339Error, epoch_float_to_offset, epoch_integer_to_offset, parse_rfc3339};
use crate::equality::{KeySet, KeyValue};
use crate::read::{self, Arg, Major, ReadError};

const ITEM_ROLE: &str = "cbor.array-item@1";
const MEMBER_ROLE: &str = "cbor.object-member@1";
const TAG_PAYLOAD_ROLE: &str = "cbor.tag-payload@1";

/// The cooperative work quantum a span decode admits between polls.
const COOPERATIVE_CREDITS: u32 = 4_096;

pub(crate) const SCALAR_KIND: &str = "cbor.scalar@1";
const ARRAY_KIND: &str = "cbor.array@1";
const OBJECT_KIND: &str = "cbor.object@1";
const TAG_KIND: &str = "cbor.tag@1";

/// Creates a fresh request-accounted CBOR builder and its prepared schema. The specialized routes share this entry
/// point: every located/stand-in/run document is a fresh build from the validated whole document.
pub(crate) fn fresh_builder(
    _resources: &ResourceContext<'_>,
) -> Result<(AccountedDocumentBuilder<'static>, jqf_data::PreparedDocumentSchema), CodecError> {
    let recipe = cbor_schema_recipe().map_err(map_data)?;
    let (builder, schema) =
        AccountedDocumentBuilder::try_new_prepared_with_coverage(&recipe, BuilderCoverage::minimal_semantic())
            .map_err(map_data)?;
    let mut builder = builder;
    builder.set_authoritative_empty_families(AuthoritativeEmptyFamilies::from_family(
        DocumentCapabilityFamily::Attributes,
    ));
    builder.set_diagnostic_coverage(DiagnosticCoverage::NotRequested);
    Ok((builder, schema))
}

fn cbor_schema_recipe() -> Result<DocumentSchemaRecipe<'static>, DataError> {
    DocumentSchemaRecipe::try_new(
        "cbor",
        Some(crate::CBOR_GENERIC_DIALECT_ID),
        &[SCALAR_KIND, ARRAY_KIND, OBJECT_KIND, TAG_KIND],
        &[ITEM_ROLE, MEMBER_ROLE, TAG_PAYLOAD_ROLE],
        &[],
        &[],
    )
}

pub(crate) fn map_data(error: DataError) -> CodecError {
    jqf_codec_core::map_data(error, "CBOR builder rejected document construction")
}

pub(crate) fn data_contract() -> CodecError {
    jqf_codec_core::data_contract("CBOR authoritative document construction")
}

pub(crate) use jqf_codec_core::{PRUNE_ALL, PruneLookup};

/// The CBOR access session: one source, one document.
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent session facts (published, source_bound, sealed, the adjacent opt-in) that must survive poll boundaries; a bitflag would hide which fact each poll sets"
)]
pub(crate) struct CborParseState {
    phase: Phase,
    decoder: Option<Decoder>,
    builder: Option<AccountedDocumentBuilder<'static>>,
    root: Option<NodeId>,
    finalizer: Option<AccountedDocumentFinalizer<'static>>,
    published: bool,
    product: Option<DocumentProduct<'static>>,
    /// The in-flight cooperative source seal, when the decoder deferred a root container span (the source-retention
    /// mode).
    binding_stage: Option<jqf_data::DocumentSourceBindingStage>,
    /// Whether the decoder committed span-backed nodes over a sealed source: the published product must then attach the
    /// session's borrowed source so the spans resolve bytes.
    source_bound: bool,
    /// Whether a cooperative source seal was already bound for this document (the retain route's root-span commit, or
    /// the eager route's bind-after-spans seal). Gates `begin_finalize`'s seal-on-spans arm.
    sealed: bool,
    /// The adjacent-value opt-in: decode ONE item and stop at its end, reporting the real consumed offset, instead of
    /// rejecting every byte after the item as trailing content. The whole-document path keeps today's trailing-byte
    /// rejection.
    adjacent: bool,
    /// The end of the decoded top-level item, captured when the decoder finishes. The consumed-offset receipt in
    /// adjacent mode: the drive advances by exactly the item's bytes, never by the whole source.
    item_end: u64,
    /// The complete input's byte length, for the consumed-offset progress receipt (the whole-document route consumes
    /// the whole input).
    source_len: u64,
    /// Builder coverage the next decode (and a recycled reopen) uses.
    coverage: BuilderCoverage,
    /// Whether the whole document is deferred to one source span.
    retain_source: bool,
    /// Empty-path count: the root container is built and member payloads stay unbuilt after every byte is validated.
    count_only: bool,
    /// Kept-subtree prune on the eager path. `None` when lazy or when the hint keeps everything.
    prune: Option<PruneLookup>,
}

enum Phase {
    Parse,
    Seal,
    Finalize,
    Publish,
}

impl CborParseState {
    pub(crate) fn try_new(
        _source: ResolvedSource<'_>,
        coverage: BuilderCoverage,
        lazy_frontier: Option<usize>,
        allow_adjacent_values: bool,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Self, CodecError> {
        // A request that names a container frontier at ALL defers the WHOLE document to one root span (v1: the frontier
        // is either "everything" or nothing). The source-retention paths (the cbor.source@1 output profile) request
        // exactly this.
        let retain_source = lazy_frontier.is_some();
        let decoder = Decoder::try_new(coverage, retain_source, allow_adjacent_values, None, false, resources)?;
        Ok(Self {
            phase: Phase::Parse,
            decoder: Some(decoder),
            builder: None,
            root: None,
            finalizer: None,
            published: false,
            product: None,
            binding_stage: None,
            source_bound: false,
            sealed: false,
            adjacent: allow_adjacent_values,
            item_end: 0,
            source_len: 0,
            coverage,
            retain_source,
            count_only: false,
            prune: None,
        })
    }

    /// Arms empty-path count: validate every byte, keep the root container, defer member payloads as child spans.
    pub(crate) fn enable_count_only(&mut self) {
        if self.retain_source {
            return;
        }
        self.count_only = true;
        self.prune = None;
        if let Some(decoder) = &mut self.decoder {
            decoder.enable_count_only();
        }
    }

    /// Re-arms prune from a recycled requirement. A source-retaining or count-only session drops the hint.
    pub(crate) fn set_prune(&mut self, prune: Option<PruneLookup>) {
        self.prune = if self.retain_source || self.count_only {
            None
        } else {
            prune
        };
        if let Some(decoder) = &mut self.decoder {
            decoder.prune.clone_from(&self.prune);
        }
    }

    /// Whether this session defers the whole document to one source span (the retention profile `try_new` derived from
    /// the requirement's frontier hint). A recycled reopen must match it, or the recycled session serves the demand
    /// under a different retention profile than a fresh open would.
    pub(crate) fn retain_source(&self) -> bool {
        self.retain_source
    }

    /// Whether this session records only the root container for an empty-path count. A recycled reopen must match it.
    pub(crate) fn count_only(&self) -> bool {
        self.count_only
    }

    /// Reinitializes this state for one more adjacent document, leaving exactly the state a fresh [`Self::try_new`]
    /// would have produced.
    pub(crate) fn try_reset(&mut self, resources: &mut ResourceContext<'_>) -> Result<(), CodecError> {
        self.decoder = Some(Decoder::try_new(
            self.coverage,
            self.retain_source,
            self.adjacent,
            self.prune.clone(),
            self.count_only,
            resources,
        )?);
        self.phase = Phase::Parse;
        self.builder = None;
        self.root = None;
        self.finalizer = None;
        self.published = false;
        self.product = None;
        self.binding_stage = None;
        self.source_bound = false;
        self.sealed = false;
        self.item_end = 0;
        self.source_len = 0;
        Ok(())
    }
}

impl AccessSession for CborParseState {
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
    fn decode<'source>(
        &mut self,
        input: AccessInput<'_, 'source>,
        context: &mut CodecRunContext<'_, '_>,
    ) -> Result<AccessResult<'source>, CodecError> {
        let source = match input {
            AccessInput::Source(source) => source,
            AccessInput::Document(_) => {
                return Err(CodecError::new(CodecFailureKind::ProviderRouteMismatch));
            }
        };
        self.source_len = u64::try_from(source.bytes().len()).unwrap_or(u64::MAX);
        if self.published {
            return Err(data_contract());
        }
        loop {
            match self.phase {
                Phase::Parse => {
                    let decoder = self.decoder.as_mut().ok_or_else(data_contract)?;
                    match decoder.poll(source, context.resources())? {
                        DecodePoll::Pending => context.replenish_work()?,
                        DecodePoll::Ready => {
                            let decoder = self.decoder.take().ok_or_else(data_contract)?;
                            self.source_bound = decoder.source_bound();
                            // Capture the item end BEFORE the decoder is consumed: the adjacent-mode consumed-offset
                            // receipt advances by exactly the item's bytes.
                            self.item_end = u64::try_from(decoder.pos).unwrap_or(u64::MAX);
                            let (builder, root) = decoder.finish(context.resources())?;
                            self.builder = Some(builder);
                            self.root = Some(root);
                            self.begin_finalize(context.resources())?;
                        }
                        DecodePoll::SealRequired => {
                            // The root container span needs the sealed source before it may be committed: start the
                            // cooperative seal (hash off — every consumer of this binding reads through
                            // metadata-checked access, never re-verifying the digest).
                            self.binding_stage =
                                Some(jqf_data::DocumentSourceBindingStage::new(source).map_err(map_data)?);
                            self.phase = Phase::Seal;
                        }
                    }
                }
                Phase::Seal => {
                    // The eager route's seal-on-spans arm enters this phase WITHOUT the stage (begin_finalize has no
                    // source): start the cooperative seal here, once.
                    if self.binding_stage.is_none() {
                        self.binding_stage = Some(jqf_data::DocumentSourceBindingStage::new(source).map_err(map_data)?);
                    }
                    let stage = self.binding_stage.as_mut().ok_or_else(data_contract)?;
                    // SAFETY: codec-core retains one immutable source
                    // authority for the complete access session and passes
                    // that exact authority each poll; the stage was
                    // constructed over the same segment and re-verifies
                    // identity on every call.
                    match unsafe { stage.poll(source, context.resources()) }.map_err(map_data)? {
                        jqf_data::DocumentSourceBindingPoll::Pending => {
                            context.replenish_work()?;
                        }
                        jqf_data::DocumentSourceBindingPoll::Ready(binding) => {
                            self.binding_stage = None;
                            self.sealed = true;
                            if let Some(decoder) = self.decoder.as_mut() {
                                decoder.commit_pending_root_span(binding, context.resources())?;
                                self.phase = Phase::Parse;
                            } else {
                                // The eager route: the whole document is built and only the seal is missing — bind
                                // the builder's source and finalize.
                                let root = self.root.take().ok_or_else(data_contract)?;
                                let mut builder = self.builder.take().ok_or_else(data_contract)?;
                                builder.bind_source(binding).map_err(map_data)?;
                                self.finalizer =
                                    Some(builder.begin_finish(root, context.resources()).map_err(map_data)?);
                                self.phase = Phase::Finalize;
                            }
                        }
                    }
                }
                Phase::Finalize => {
                    let finalizer = self.finalizer.as_mut().ok_or_else(data_contract)?;
                    let DocumentFinalizationPoll::Ready(document) =
                        finalizer.poll(context.resources()).map_err(map_data)?
                    else {
                        context.replenish_work()?;
                        continue;
                    };
                    self.finalizer = None;
                    self.product = Some(DocumentProduct::try_new(document, context.resources())?);
                    self.phase = Phase::Publish;
                }
                Phase::Publish => {
                    let product = self.product.take().ok_or_else(data_contract)?;
                    let product = if self.source_bound {
                        // SAFETY: codec-core owns this exact immutable source for
                        // the whole access session; the decoder sealed against it
                        // and every committed span names it.
                        unsafe { product.attach_borrowed_source_from_access_session(source, context.resources())? }
                    } else {
                        product
                    };
                    self.published = true;
                    let outcome = AccessOutcome::FullDocument(product);
                    // The consumed-offset receipt: the whole-document route consumes the whole input; the
                    // adjacent-value opt-in decodes ONE item and reports the item's real end, so the drive advances by
                    // exactly the item and decodes the remainder as the next item.
                    let consumed = if self.adjacent { self.item_end } else { self.source_len };
                    return Ok(AccessResult::from_outcome_with_consumed_offset(outcome, consumed));
                }
            }
        }
    }
}

impl CborParseState {
    fn begin_finalize(&mut self, resources: &mut ResourceContext<'_>) -> Result<(), CodecError> {
        // The eager route's per-item authored spans were committed against a builder with no seal yet: bind the exact
        // immutable source cooperatively BEFORE the document may finish (the `record_authored_span` seal discipline).
        if self.source_bound && !self.sealed {
            self.phase = Phase::Seal;
            return Ok(());
        }
        let root = self.root.take().ok_or_else(data_contract)?;
        let builder = self.builder.take().ok_or_else(data_contract)?;
        self.finalizer = Some(builder.begin_finish(root, resources).map_err(map_data)?);
        self.phase = Phase::Finalize;
        Ok(())
    }
}

/// Drives one validated CBOR item from a byte span into a FRESH document, returning the built product. The span must be
/// a complete single item (the walker's located value). This is the "second read is budgeted" law: the walk already
/// validated the whole input, so this read materializes only the located subtree, bounding retention to it rather than
/// to the input.
pub(crate) fn decode_span(
    source: ResolvedSource<'_>,
    start: usize,
    end: usize,
    prune: Option<&PruneLookup>,
    resources: &mut ResourceContext<'_>,
) -> Result<DocumentProduct<'static>, CodecError> {
    let document = decode_span_document(source, start, end, prune, resources)?;
    DocumentProduct::try_new(document, resources)
}

/// Publishes the last-wins located container as a lazy span root. The walk already
/// proved `[start, end)` one complete item and counted its children; count does not
/// re-decode the hit.
#[allow(
    unsafe_code,
    reason = "span admission and source attach are unsafe by jqf-data; the walk proved this range on the session-owned source"
)]
pub(crate) fn publish_located_skeleton<'source>(
    source: ResolvedSource<'source>,
    start: usize,
    end: usize,
    container: ContainerSpanKind,
    child_count: Option<u64>,
    context: &mut CodecRunContext<'_, '_>,
) -> Result<DocumentProduct<'source>, CodecError> {
    let bytes = &source.bytes()[start..end];
    let base = source
        .base_offset()
        .saturating_add(u64::try_from(start).unwrap_or(u64::MAX));
    let sub = ResolvedSource::new(source.source(), source.label(), bytes, base);
    let (mut builder, schema) = fresh_builder(context.resources())?;
    builder.bind_span_materializer(&crate::lazy::CBOR_SPAN_MATERIALIZER as &dyn LazySpanMaterializer);
    let mut stage = DocumentSourceBindingStage::new(sub).map_err(map_data)?;
    let binding = loop {
        // SAFETY: codec-core holds this session's source unchanged; `sub` is
        // the last-wins range the walk recorded on that authority.
        match unsafe { stage.poll(sub, context.resources()) }.map_err(map_data)? {
            DocumentSourceBindingPoll::Pending => context.replenish_work()?,
            DocumentSourceBindingPoll::Ready(binding) => break binding,
        }
    };
    builder.bind_source(binding).map_err(map_data)?;
    let span = jqf_source::Span::from_usize(0, bytes.len());
    let slot = match container {
        ContainerSpanKind::Array => 1,
        ContainerSpanKind::Object => 2,
    };
    let kind = schema.node_kind(slot).ok_or_else(data_contract)?;
    // SAFETY: the walk proved `[start, end)` one complete container of this
    // session's immutable source.
    let root =
        unsafe { builder.add_prepared_bound_container_span_node(&schema, kind, span, container, context.resources()) }
            .map_err(map_data)?;
    builder
        .set_container_span_counts(root, child_count, None)
        .map_err(map_data)?;
    let document = builder.finish(root, context.resources()).map_err(map_data)?;
    let document =
        unsafe { document.with_borrowed_source_from_bound_authority(sub, context.resources()) }.map_err(map_data)?;
    DocumentProduct::try_new(document, context.resources())
}

/// Kind-only document from the located item's first payload byte (or recognized tag). The walk already validated every
/// unread byte.
pub(crate) fn kind_only_span(
    source: ResolvedSource<'_>,
    start: usize,
    resources: &mut ResourceContext<'_>,
) -> Result<DocumentProduct<'static>, CodecError> {
    if recognized_tag_may_project(source, start)? {
        let end = crate::walk::skip_item(source, start, resources)?;
        return decode_span(source, start, end, None, resources);
    }
    let kind = classify_kind(source, start)?;
    let (mut builder, _) = fresh_builder(resources)?;
    let root = add_kind_only_node(&mut builder, kind, resources)?;
    let document = builder.finish(root, resources).map_err(map_data)?;
    DocumentProduct::try_new(document, resources)
}

fn recognized_tag_may_project(source: ResolvedSource<'_>, mut pos: usize) -> Result<bool, CodecError> {
    loop {
        let (head, next) = read::head(source.bytes(), pos).map_err(|error| read_error(source, pos, error))?;
        match head.major {
            Major::Tag => {
                let Arg::UInt(tag) = head.arg else {
                    return Err(read_error(source, pos, ReadError::Eof));
                };
                if tag <= 5 {
                    return Ok(true);
                }
                pos = next;
            }
            _ => return Ok(false),
        }
    }
}

fn classify_kind(source: ResolvedSource<'_>, mut pos: usize) -> Result<ValueKind, CodecError> {
    loop {
        let (head, next) = read::head(source.bytes(), pos).map_err(|error| read_error(source, pos, error))?;
        match head.major {
            Major::Tag => {
                let Arg::UInt(tag) = head.arg else {
                    return Err(read_error(source, pos, ReadError::Eof));
                };
                if tag <= 5 {
                    return Ok(if tag <= 1 {
                        ValueKind::OffsetDateTime
                    } else {
                        ValueKind::Number
                    });
                }
                pos = next;
            }
            Major::Array => return Ok(ValueKind::Array),
            Major::Map => return Ok(ValueKind::Object),
            Major::Text => return Ok(ValueKind::String),
            Major::Bytes => return Ok(ValueKind::Bytes),
            Major::UInt | Major::NegInt => return Ok(ValueKind::Number),
            Major::Simple => {
                return Ok(match head.arg {
                    Arg::UInt(20 | 21) => ValueKind::Bool,
                    Arg::UInt(22 | 23) => ValueKind::Null,
                    _ => ValueKind::Number,
                });
            }
        }
    }
}

fn add_kind_only_node(
    builder: &mut AccountedDocumentBuilder<'static>,
    kind: ValueKind,
    resources: &mut ResourceContext<'_>,
) -> Result<NodeId, CodecError> {
    match kind {
        ValueKind::Null => builder.add_node(SCALAR_KIND, AccountedSemanticNode::Null, None, resources),
        ValueKind::Bool => builder.add_node(SCALAR_KIND, AccountedSemanticNode::Bool(false), None, resources),
        ValueKind::Number => builder.add_node(SCALAR_KIND, AccountedSemanticNode::Integer("0"), None, resources),
        ValueKind::String => builder.add_node(SCALAR_KIND, AccountedSemanticNode::String(""), None, resources),
        ValueKind::Bytes => builder.add_node(SCALAR_KIND, AccountedSemanticNode::Bytes(&[]), None, resources),
        ValueKind::Array => builder.add_node(
            ARRAY_KIND,
            AccountedSemanticNode::Array { item_role: ITEM_ROLE },
            None,
            resources,
        ),
        ValueKind::Object => builder.add_node(
            OBJECT_KIND,
            AccountedSemanticNode::Object {
                member_role: MEMBER_ROLE,
            },
            None,
            resources,
        ),
        ValueKind::OffsetDateTime => {
            let datetime = parse_rfc3339("1970-01-01T00:00:00Z").map_err(|_| data_contract())?;
            builder.add_node(
                SCALAR_KIND,
                AccountedSemanticNode::OffsetDateTime(&datetime),
                None,
                resources,
            )
        }
        _ => return Err(data_contract()),
    }
    .map_err(map_data)
}

/// As [`decode_span`], but returns the built `Document` directly (no `DocumentProduct` wrapper): the lazy
/// container-span materializer decodes one span into a document only to materialize its root value, then drops the
/// document.
pub(crate) fn decode_span_document(
    source: ResolvedSource<'_>,
    start: usize,
    end: usize,
    prune: Option<&PruneLookup>,
    resources: &mut ResourceContext<'_>,
) -> Result<Document<'static>, CodecError> {
    let span_source = span_source(source, start, end);
    let (builder, _schema) = fresh_builder(resources)?;
    let mut builder = Some(builder);
    let root = decode_value_into(&mut builder, span_source, prune.cloned(), resources)?;
    let builder = builder.take().ok_or_else(data_contract)?;
    builder.finish(root, resources).map_err(map_data)
}

/// The source restricted to one validated item's byte span, preserving the original source identity and absolute base
/// offset for diagnostics.
fn span_source(source: ResolvedSource<'_>, start: usize, end: usize) -> ResolvedSource<'_> {
    let base = source
        .base_offset()
        .saturating_add(u64::try_from(start).unwrap_or(u64::MAX));
    ResolvedSource::new(source.source(), source.label(), &source.bytes()[start..end], base)
}

/// Decodes ONE complete validated item from a span into `builder`, returning its root node. The builder slot is reused
/// across calls, so a run accumulates many elements without a per-element parse state.
fn decode_value_into(
    builder_slot: &mut Option<AccountedDocumentBuilder<'static>>,
    source: ResolvedSource<'_>,
    prune: Option<PruneLookup>,
    resources: &mut ResourceContext<'_>,
) -> Result<NodeId, CodecError> {
    let builder = builder_slot.take().ok_or_else(data_contract)?;
    let mut decoder = Decoder::try_new_from_builder(builder, prune);
    loop {
        match decoder.poll(source, resources)? {
            DecodePoll::Pending => {
                resources
                    .try_begin_next_cooperative_entry(COOPERATIVE_CREDITS)
                    .map_err(CodecError::from)?;
            }
            DecodePoll::Ready => {
                let (builder, root) = decoder.finish(resources)?;
                *builder_slot = Some(builder);
                return Ok(root);
            }
            DecodePoll::SealRequired => {
                // The span re-decode path constructs its decoder with `retain_source` off, so the source-retention seal
                // can never be requested here.
                return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                    contract: "source-retention seal on the span re-decode path",
                }));
            }
        }
    }
}

enum DecodePoll {
    Pending,
    Ready,
    /// The decode deferred the whole container root to one span and needs the session to seal the source cooperatively
    /// before the span node may be committed (the source-retention mode's accounting law).
    SealRequired,
}

/// The frame-driven whole-route decoder.
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent decoder facts (retain_source, done, capacity_scanned,
              record_spans, spans_committed, the adjacent opt-in) that must
              survive poll boundaries; a bitflag would hide which fact each
              poll sets"
)]
struct Decoder {
    pos: usize,
    /// The open-container stack. Accounted under `Working`, exactly as json holds the identical concept
    /// (`json/src/parse.rs`): one frame per open container, so its length follows the input's nesting.
    frames: Vec<Frame>,
    /// Tags read but not yet applied to a value. Accounted under `Working`: a tag chain is one entry per leading tag
    /// head, so an input of nothing but tag bytes would otherwise grow an untracked vector.
    pending_tags: Vec<u64>,
    produced: Option<NodeId>,
    root: Option<NodeId>,
    done: bool,
    builder: AccountedDocumentBuilder<'static>,
    /// Whether the whole document is deferred to one source span (the source-retention mode). When set, the decoder
    /// validates the input via the byte walk and commits the root as a `ContainerSpan`, publishing a document whose
    /// nodes answer `node_source_span` for the source@1 encoder.
    retain_source: bool,
    /// The adjacent-value opt-in: the root completes at the item's end (`pos`) instead of asserting the item consumes
    /// the whole input, so the session reports the real consumed offset and the drive decodes the remainder as the next
    /// item.
    adjacent: bool,
    /// The pending root container span, awaiting the session's cooperative seal before the span node is committed
    /// (source-retention mode).
    pending_root_span: Option<(jqf_source::Span, jqf_data::ContainerSpanKind)>,
    /// The prepared schema, kept for the prepared-bound span commit.
    schema: Option<PreparedDocumentSchema>,
    /// Whether the one-shot arena pre-scan has run.
    capacity_scanned: bool,
    /// Whether per-item authored spans are recorded (the eager whole-document decode). The re-decode paths —
    /// element/span re-decode and the encoder's reparse — build documents that outlive the source borrow and must not
    /// name spans of it.
    record_spans: bool,
    /// The first byte of the current logical item: the first tag head of a tag chain, or the value head itself. Set by
    /// [`Decoder::step`] before each value head read; taken by the owning head arm once the item's extent is known.
    item_start: Option<usize>,
    /// One pending tag head's offset, parallel to `pending_tags` (a tag layer's span runs from its own head through the
    /// value's end).
    pending_tag_starts: alloc::vec::Vec<usize>,
    /// Authored spans stashed in node-creation order, flushed to the builder at [`Decoder::finish`]: the builder's
    /// `record_authored_span` demands strictly increasing node order, which a container's span — known only at CLOSE,
    /// after its children's nodes exist — cannot satisfy in place. The placeholder entries (`None`) sit at their
    /// container's creation position and are filled at close, keeping the flush ordered.
    item_spans: alloc::vec::Vec<Option<(jqf_data::NodeId, jqf_source::Span)>>,
    /// Whether any per-item span was stashed: the session must then seal the exact immutable source authority before
    /// the document may finish.
    spans_committed: bool,
    /// Kept-subtree prune. `None` keeps every member.
    prune: Option<PruneLookup>,
    /// Empty-path count: root children that are containers are committed as spans, not built trees.
    count_only: bool,
}

struct Frame {
    node: NodeId,
    kind: FrameKind,
    /// Tags pending when this container opened: they wrap the WHOLE container when it closes, never its items.
    tags: Vec<u64>,
    /// The tags' head offsets, parallel to `tags`.
    tag_starts: Vec<usize>,
    /// The item's first byte (its header, or its tag chain's first head).
    start: usize,
    /// The creation-order slot of this container's span placeholder, when spans are recorded (filled at close).
    span_slot: Option<usize>,
    /// Prune-tree node governing this container's children (`PRUNE_ALL` keeps every child).
    prune: u32,
    _depth: OwnedDepthGuard,
}

enum FrameKind {
    Array {
        remaining: u64,
        indefinite: bool,
    },
    Map {
        remaining_pairs: u64,
        indefinite: bool,
        phase: MapPhase,
        keys: KeySet,
        pending_key_text: Option<PendingKey>,
    },
}

enum MapPhase {
    ExpectKey,
    ExpectValue,
}

/// A map key's member text, either a source range (definite UTF-8 text) or an owned copy (indefinite text chunks).
enum PendingKey {
    Range(core::ops::Range<usize>),
    Owned(alloc::string::String),
}

impl Decoder {
    fn try_new(
        coverage: BuilderCoverage,
        retain_source: bool,
        adjacent: bool,
        prune: Option<PruneLookup>,
        count_only: bool,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<Self, CodecError> {
        // A FRESH builder from the validated recipe (not the shared prototype builder): the dynamic
        // `add_node`/`add_occurrence` path this decoder uses is refused by the shared-schema prototype builder.
        let recipe = cbor_schema_recipe().map_err(map_data)?;
        let (builder, schema) =
            AccountedDocumentBuilder::try_new_prepared_with_coverage(&recipe, coverage).map_err(map_data)?;
        let mut builder = builder;
        builder.set_authoritative_empty_families(AuthoritativeEmptyFamilies::from_family(
            DocumentCapabilityFamily::Attributes,
        ));
        builder.set_diagnostic_coverage(DiagnosticCoverage::NotRequested);
        if retain_source || count_only {
            builder.bind_span_materializer(&crate::lazy::CBOR_SPAN_MATERIALIZER);
        }
        Ok(Self {
            pos: 0,
            frames: Vec::new(),
            pending_tags: Vec::new(),
            produced: None,
            root: None,
            done: false,
            builder,
            retain_source,
            adjacent,
            pending_root_span: None,
            schema: Some(schema),
            capacity_scanned: false,
            record_spans: !retain_source,
            item_start: None,
            pending_tag_starts: Vec::new(),
            item_spans: Vec::new(),
            spans_committed: false,
            prune,
            count_only,
        })
    }

    fn enable_count_only(&mut self) {
        self.count_only = true;
        self.prune = None;
        self.builder
            .bind_span_materializer(&crate::lazy::CBOR_SPAN_MATERIALIZER);
    }

    fn finish(
        mut self,
        resources: &ResourceContext<'_>,
    ) -> Result<(AccountedDocumentBuilder<'static>, NodeId), CodecError> {
        // Flush the stashed authored spans in node-creation order — the
        // `record_authored_span` order law, satisfied by construction.
        // SAFETY: every span was produced by this decoder's own byte walk
        // over the exact immutable source authority the session seals before
        // publication, and the bytes each names re-resolve to the node's
        // stored semantic under this dialect — the `record_authored_span`
        // contract, in binary clothes (the span names the item's exact
        // header-through-payload bytes, never a copy of its semantic).
        for (node, span) in core::mem::take(&mut self.item_spans).into_iter().flatten() {
            unsafe { self.builder.record_authored_span(node, span, resources) }.map_err(map_data)?;
        }
        let root = self.root.ok_or_else(data_contract)?;
        Ok((self.builder, root))
    }

    /// Whether the decoder sealed a source and committed span-backed nodes over it (the source-retention mode), or
    /// stashed per-item authored spans that require the same seal.
    fn source_bound(&self) -> bool {
        (self.retain_source && self.done) || (self.record_spans && self.spans_committed)
    }

    /// Stashes one item's authored span: `start` is the item's first byte (a tag chain's first head) and `end` is just
    /// past its last byte. Every CBOR item's extent is exact from its header (RFC 8949 §3), so the bounds are
    /// unambiguous at the point of the node's production.
    fn bind_span(&mut self, node: jqf_data::NodeId, start: usize, end: usize) -> Result<(), CodecError> {
        if !self.record_spans {
            return Ok(());
        }
        let span = jqf_source::Span::try_new(
            u32::try_from(start).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?,
            u32::try_from(end).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?,
        )
        .ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?;
        self.item_spans.push(Some((node, span)));
        self.spans_committed = true;
        Ok(())
    }

    /// [`Self::bind_span`] for a just-produced item: the span's start is the current logical item's first byte.
    fn bind_item_span(&mut self, node: jqf_data::NodeId, end: usize) -> Result<(), CodecError> {
        let Some(start) = self.item_start.take() else {
            return Ok(());
        };
        self.bind_span(node, start, end)
    }

    /// Reserves the creation-order slot for an OPEN container's span (a placeholder filled at close, when the extent is
    /// known). Returns the slot the frame carries.
    fn reserve_container_span(&mut self) -> Option<usize> {
        if !self.record_spans {
            return None;
        }
        let slot = self.item_spans.len();
        self.item_spans.push(None);
        Some(slot)
    }

    /// Fills the closing container's reserved span slot: the container's authored extent runs from its first byte
    /// through the current position (just past its last item, or past the BREAK byte of an indefinite container).
    fn fill_container_span(&mut self, frame: &Frame) -> Result<(), CodecError> {
        if let Some(slot) = frame.span_slot {
            let span = jqf_source::Span::try_new(
                u32::try_from(frame.start).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?,
                u32::try_from(self.pos).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?,
            )
            .ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?;
            self.item_spans[slot] = Some((frame.node, span));
            self.spans_committed = true;
        }
        Ok(())
    }

    /// Records a just-produced scalar item's authored span and advances past it: the span runs from the item's first
    /// byte through `end`, the offset just past its last byte.
    fn finish_value(&mut self, node: jqf_data::NodeId, end: usize) -> Result<(), CodecError> {
        self.bind_item_span(node, end)?;
        self.pos = end;
        self.produced = Some(node);
        Ok(())
    }

    /// Constructs a decoder over a builder the CALLER prepared (the span re-decode path). The builder is reused across
    /// elements, so its node and occurrence ledger accumulates rather than being replaced. The re-decode is always
    /// single-item over a validated span, never adjacent.
    fn try_new_from_builder(builder: AccountedDocumentBuilder<'static>, prune: Option<PruneLookup>) -> Self {
        Self {
            pos: 0,
            frames: Vec::new(),
            pending_tags: Vec::new(),
            produced: None,
            root: None,
            done: false,
            builder,
            retain_source: false,
            adjacent: false,
            pending_root_span: None,
            schema: None,
            capacity_scanned: false,
            record_spans: false,
            item_start: None,
            pending_tag_starts: Vec::new(),
            item_spans: Vec::new(),
            spans_committed: false,
            prune,
            count_only: false,
        }
    }

    fn poll(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<DecodePoll, CodecError> {
        loop {
            if self.done {
                if self.pending_root_span.is_some() {
                    return Ok(DecodePoll::SealRequired);
                }
                return Ok(DecodePoll::Ready);
            }
            self.scan_capacity(source.bytes(), resources);
            if resources.admit_work_transition()? == WorkAdmission::Pending {
                return Ok(DecodePoll::Pending);
            }
            self.step(source, resources)?;
        }
    }

    /// One-shot exact arena reservation. CBOR is the one codec whose final node/occurrence totals are knowable from the
    /// input — every container carries its definite count (RFC 8949 §3.2.5). A cheap structural pre-scan of the wire
    /// sums those counts and reserves the EXACT total once, so the builder's arenas never amortized-double.
    ///
    /// The reserve is one shot per decode, NEVER per container: a per-container exact reserve reallocates the arena to
    /// exactly `len + count` at every open past the last reserve, holding old and new buffers live at once (measured
    /// regression: catalog RSS 149 -> 211 MiB). The pre-scan prices the whole document once; it is the json codec's
    /// source-length estimate made exact by the lengths already in the input. It is a hint, never an admission
    /// requirement — a denied reservation rolls back and the builder grows amortized.
    fn scan_capacity(&mut self, bytes: &[u8], resources: &ResourceContext<'_>) {
        if self.capacity_scanned {
            return;
        }
        self.capacity_scanned = true;
        // Source-retention builds ONE span-backed node for a container root (a scalar root is a one-node document
        // either way), so pricing every node and occurrence in the input would reserve — and charge the ledger for
        // — millions of slots nothing will ever build. Empty-path count keeps the root plus one span per child.
        if self.retain_source || self.count_only || self.prune.is_some() {
            return;
        }
        if bytes.is_empty() {
            return;
        }
        let mut nodes = 1usize; // the root
        let mut occurrences = 0usize;
        // The full census: the reservation is exact, so the builder never reallocs and the peak carries one arena, not
        // old-plus-new during a doubling. A prefix-bounded census with amortized growth for the rest traded ~20 ms of
        // wall on a 10 MB document for +48 MiB of peak on every whole-document decode (the last doubling holds both
        // buffers), which the RSS gate's cbor lanes breached at 1.37x — the census walk is O(value bytes), linear
        // across an adjacent-value stream's recycled reopens too, so the exact reserve is bought once per byte.
        count_nodes(bytes, 0, &mut nodes, &mut occurrences);
        if nodes <= 1 {
            return;
        }
        let _ = self.builder.try_reserve(
            DocumentCapacity {
                nodes,
                occurrences,
                ..DocumentCapacity::default()
            },
            resources,
        );
    }

    fn step(&mut self, source: ResolvedSource<'_>, resources: &ResourceContext<'_>) -> Result<(), CodecError> {
        let bytes = source.bytes();
        if let Some(node) = self.produced.take() {
            return self.deliver(node, source, resources);
        }
        // Source-retention mode defers the WHOLE container root to one span: the byte walk validates the complete
        // input, then the root is committed as a span-backed container.
        if self.retain_source && self.frames.is_empty() && self.pos == 0 {
            return self.read_source_root(source, resources);
        }
        // A BREAK byte (0xff) closes the innermost INDEFINITE container before any key or value head is read. RFC 8949
        // §5.6: a BREAK in a map is legal only in key position — a dangling key (ExpectValue) is not well-formed and
        // must not drop the pending key.
        if bytes.get(self.pos) == Some(&0xff) && self.top_frame_indefinite() {
            if self.top_frame_map_expects_value() {
                return Err(crate::error::invalid(
                    source,
                    self.pos,
                    "break-in-value-position",
                    "a BREAK byte is legal only where a map pair starts",
                ));
            }
            return self.close_break(source, resources);
        }
        // A map frame waiting for its next key parses the key structurally (never as a document node) before any value
        // head is read.
        let expecting_key = self.frames.as_slice().last().is_some_and(|frame| {
            matches!(
                frame.kind,
                FrameKind::Map {
                    phase: MapPhase::ExpectKey,
                    ..
                }
            )
        });
        if expecting_key {
            return self.read_map_key(source, resources);
        }
        if self.omitted_map_value(source) {
            return self.skip_omitted_map_value(source, resources);
        }
        // Every value head read starts a new logical item: capture its first byte NOW so a leading tag chain's span
        // covers the chain from its first head through the value's end.
        if self.item_start.is_none() {
            self.item_start = Some(self.pos);
        }
        self.read_value_head(source, resources)
    }

    fn omitted_map_value(&self, source: ResolvedSource<'_>) -> bool {
        let Some(lookup) = self.prune.as_ref() else {
            return false;
        };
        let Some(frame) = self.frames.as_slice().last() else {
            return false;
        };
        if frame.prune == PRUNE_ALL {
            return false;
        }
        let FrameKind::Map {
            phase: MapPhase::ExpectValue,
            pending_key_text,
            ..
        } = &frame.kind
        else {
            return false;
        };
        let Some(pending) = pending_key_text else {
            return false;
        };
        let key = match pending {
            PendingKey::Range(range) => source.bytes().get(range.start..range.end),
            PendingKey::Owned(text) => Some(text.as_bytes()),
        };
        let Some(key) = key else {
            return false;
        };
        lookup.member_prune(frame.prune, key).is_none()
    }

    fn skip_omitted_map_value(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        self.item_start = None;
        self.pos = crate::walk::skip_item(source, self.pos, resources)?;
        let Some(frame) = self.frames.as_mut_slice().last_mut() else {
            return Err(data_contract());
        };
        let FrameKind::Map {
            remaining_pairs,
            indefinite,
            phase,
            pending_key_text,
            ..
        } = &mut frame.kind
        else {
            return Err(data_contract());
        };
        if !matches!(phase, MapPhase::ExpectValue) {
            return Err(data_contract());
        }
        pending_key_text.take();
        *remaining_pairs = remaining_pairs.saturating_sub(1);
        if !*indefinite && *remaining_pairs == 0 {
            let frame = self.frames.pop().ok_or_else(data_contract)?;
            self.restore_container_tags(&frame);
            self.fill_container_span(&frame)?;
            self.produced = Some(frame.node);
        } else {
            *phase = MapPhase::ExpectKey;
        }
        Ok(())
    }

    fn value_prune(&self, source: ResolvedSource<'_>) -> u32 {
        let Some(lookup) = self.prune.as_ref() else {
            return PRUNE_ALL;
        };
        let Some(frame) = self.frames.as_slice().last() else {
            return jqf_codec_core::PruneTree::ROOT;
        };
        if frame.prune == PRUNE_ALL {
            return PRUNE_ALL;
        }
        match &frame.kind {
            FrameKind::Array { .. } => lookup.element_prune(frame.prune),
            FrameKind::Map {
                phase: MapPhase::ExpectValue,
                pending_key_text,
                ..
            } => {
                let Some(pending) = pending_key_text else {
                    return PRUNE_ALL;
                };
                let key = match pending {
                    PendingKey::Range(range) => source.bytes().get(range.start..range.end),
                    PendingKey::Owned(text) => Some(text.as_bytes()),
                };
                let Some(key) = key else {
                    return PRUNE_ALL;
                };
                lookup.member_prune(frame.prune, key).unwrap_or(PRUNE_ALL)
            }
            FrameKind::Map { .. } => PRUNE_ALL,
        }
    }

    /// Whether the top frame is an indefinite-length container (BREAK closes it, never an item count).
    fn top_frame_indefinite(&self) -> bool {
        self.frames.as_slice().last().is_some_and(|frame| match &frame.kind {
            FrameKind::Array { indefinite, .. } | FrameKind::Map { indefinite, .. } => *indefinite,
        })
    }

    /// Whether the top frame is a map waiting for the value of a key already read — a BREAK here would drop that key.
    fn top_frame_map_expects_value(&self) -> bool {
        self.frames.as_slice().last().is_some_and(|frame| {
            matches!(
                frame.kind,
                FrameKind::Map {
                    phase: MapPhase::ExpectValue,
                    ..
                }
            )
        })
    }

    /// The source-retention root read: the byte walk validates the complete input, and a CONTAINER root is committed as
    /// one span-backed node (the `cbor.source@1` encoder reuses its exact bytes). A scalar root has no span to reuse
    /// — the decoder falls through to the ordinary eager read.
    fn read_source_root(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        let bytes = source.bytes();
        // The byte walk validates the WHOLE input to the generic dialect's exact strictness (validate-everything-first)
        // and locates the root's span. With an empty step list the root is the target. The walk takes this decoder's
        // own adjacent opt-in: a source-retention session opened for adjacent values must stop at the first item, never
        // reject the remainder as trailing content.
        let located = crate::walk::locate(source, &[], self.adjacent, resources)?;
        let crate::walk::Located::Value { start, end, .. } = located.0 else {
            return Err(data_contract());
        };
        let (head, _next) = read::head(bytes, start).map_err(|error| read_error(source, start, error))?;
        let container = match head.major {
            Major::Array => Some(jqf_data::ContainerSpanKind::Array),
            Major::Map => Some(jqf_data::ContainerSpanKind::Object),
            _ => None,
        };
        let Some(container) = container else {
            // A scalar root is not span-backed: defer to the eager decode.
            self.retain_source = false;
            return self.step(source, resources);
        };
        let span = jqf_source::Span::try_new(
            u32::try_from(start).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?,
            u32::try_from(end).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?,
        )
        .ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?;
        // The span node needs the sealed source BEFORE it is committed, but the seal is a linear pass: stash the span
        // and let the session seal cooperatively across polls (the stage polls in `DecodePoll:: SealRequired`), then
        // commit through [`Decoder::commit_pending_root_span`].
        self.pending_root_span = Some((span, container));
        self.pos = end;
        self.done = true;
        Ok(())
    }

    /// Commits the whole-document container root as a source span, binding the exact immutable source authority the
    /// session sealed cooperatively.
    fn commit_pending_root_span(
        &mut self,
        binding: jqf_data::DocumentSourceBinding,
        resources: &ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        let (span, container) = self.pending_root_span.take().ok_or_else(data_contract)?;
        self.builder.bind_source(binding).map_err(map_data)?;
        let schema = self.schema.as_ref().ok_or_else(data_contract)?;
        let slot = match container {
            jqf_data::ContainerSpanKind::Array => 1,
            jqf_data::ContainerSpanKind::Object => 2,
        };
        let kind = schema.node_kind(slot).ok_or_else(data_contract)?;
        // SAFETY: the byte walk just validated [start, end) as one complete
        // item of this exact immutable source authority, which codec-core
        // holds unchanged through publication and which the session sealed.
        let node = unsafe {
            self.builder
                .add_prepared_bound_container_span_node(schema, kind, span, container, resources)
        }
        .map_err(map_data)?;
        self.root = Some(node);
        Ok(())
    }

    /// Closes the innermost indefinite container at the BREAK byte.
    fn close_break(&mut self, source: ResolvedSource<'_>, _resources: &ResourceContext<'_>) -> Result<(), CodecError> {
        let bytes = source.bytes();
        debug_assert_eq!(bytes.get(self.pos), Some(&0xff));
        // A tag read after the container opened still owes its CONTENT item; the BREAK is not a data item (RFC 8949
        // §3.4.4), so a pending tag at the close is a malformed document. Rejecting here — rather than wrapping the
        // empty container in `cbor:tag:N` at deliver — is what keeps the `debug_assert!(tag > 5)` invariant true AND
        // closes the plain-release silent-accept divergence.
        if !self.pending_tags.is_empty() {
            return Err(crate::error::invalid(
                source,
                self.pos,
                "tag-without-content",
                "a tag must receive its content item before the container closes",
            ));
        }
        self.pos += 1;
        let frame = self.frames.pop().ok_or_else(data_contract)?;
        self.restore_container_tags(&frame);
        self.fill_container_span(&frame)?;
        self.produced = Some(frame.node);
        Ok(())
    }

    /// Restores the tags a closing container held (and their head offsets) so the produced node's delivery applies them
    /// (innermost first).
    fn restore_container_tags(&mut self, frame: &Frame) {
        self.pending_tags.extend_from_slice(&frame.tags);
        self.pending_tag_starts.extend_from_slice(&frame.tag_starts);
    }

    /// Parses one map key structurally, records its §5.6.1 identity, and arms the frame to receive the value. A
    /// non-text key makes the map non-projectable, which fails the whole-document route immediately.
    fn read_map_key(&mut self, source: ResolvedSource<'_>, resources: &ResourceContext<'_>) -> Result<(), CodecError> {
        let bytes = source.bytes();
        // Text-key fast path: a definite UTF-8 text key is the only key the whole-document route accepts, so keep it as
        // a source range and never copy the bytes into a KeyValue or a String.
        let (head, next) = read::head(bytes, self.pos).map_err(|error| read_error(source, self.pos, error))?;
        if let (Major::Text, Arg::UInt(length)) = (head.major, head.arg) {
            let end = checked_slice_end(source, next, length)?;
            let payload = bytes
                .get(next..end)
                .ok_or_else(|| read_error(source, next, ReadError::Eof))?;
            core::str::from_utf8(payload)
                .map_err(|_| crate::error::invalid(source, next, "invalid-utf8", "text string is not valid UTF-8"))?;
            let frame = self.frames.as_mut_slice().last_mut().ok_or_else(data_contract)?;
            let FrameKind::Map {
                keys,
                pending_key_text,
                phase,
                ..
            } = &mut frame.kind
            else {
                return Err(data_contract());
            };
            if !keys.try_insert_text(bytes, next..end) {
                return Err(crate::error::invalid(
                    source,
                    next,
                    "duplicate-key",
                    "a map's keys must be unique (RFC 8949 §5.6.1)",
                ));
            }
            *pending_key_text = Some(PendingKey::Range(next..end));
            *phase = MapPhase::ExpectValue;
            self.pos = end;
            return Ok(());
        }
        let key_start = self.pos;
        let key = self.parse_key_value(source, resources)?;
        let frame = self.frames.as_mut_slice().last_mut().ok_or_else(data_contract)?;
        let FrameKind::Map {
            keys,
            pending_key_text,
            phase,
            ..
        } = &mut frame.kind
        else {
            return Err(data_contract());
        };
        // A non-projectable map cannot satisfy the whole route's semantic-root promise: fail immediately rather than
        // decode the remainder of the map. Duplicate non-text keys still fire the uniqueness refusal first.
        let text = match &key {
            KeyValue::Text(text) => Some(alloc::string::String::from(
                core::str::from_utf8(text).map_err(|_| data_contract())?,
            )),
            _ => None,
        };
        if !keys.try_insert(bytes, key) {
            return Err(crate::error::invalid(
                source,
                key_start,
                "duplicate-key",
                "a map's keys must be unique (RFC 8949 §5.6.1)",
            ));
        }
        let Some(text) = text else {
            return Err(unrepresentable());
        };
        *pending_key_text = Some(PendingKey::Owned(text));
        *phase = MapPhase::ExpectValue;
        Ok(())
    }

    /// Feeds a completed value node to its parent (or completes the root), wrapping any pending uninterpreted tags into
    /// tag-layer nodes first.
    fn deliver(
        &mut self,
        mut node: NodeId,
        source: ResolvedSource<'_>,
        resources: &ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        let bytes = source.bytes();
        // Wrap pending tags, innermost first. Recognized tags are consumed at head-read time, so only uninterpreted
        // tags can reach this point; the guard below is an invariant, never a live branch.
        while let Some(tag) = self.pending_tags.pop() {
            debug_assert!(tag > 5, "recognized tags are consumed at head-read time");
            let start = self.pending_tag_starts.pop().ok_or_else(data_contract)?;
            node = self.wrap_layer(node, tag, resources)?;
            // The tag layer's authored extent is its OWN head through the value's end: a tagged item is the tag plus
            // the value it wraps.
            self.bind_span(node, start, self.pos)?;
        }
        let mut finish = false;
        let Some(frame) = self.frames.as_mut_slice().last_mut() else {
            // The root value: exactly one top-level item is legal on the single-document path; the adjacent-value
            // opt-in stops at the item's end and the remainder is the next item.
            if !self.adjacent && self.pos != bytes.len() {
                return Err(crate::error::invalid(
                    source,
                    self.pos,
                    "trailing-content",
                    "trailing content after the top-level item",
                ));
            }
            self.root = Some(node);
            self.done = true;
            return Ok(());
        };
        let owner = frame.node;
        match &mut frame.kind {
            FrameKind::Array { remaining, indefinite } => {
                self.builder
                    .add_occurrence(LocalOwnerRef::Node(owner), ITEM_ROLE, None, node, resources)
                    .map_err(map_data)?;
                *remaining -= 1;
                // A definite array closes at its count; an indefinite one closes at the BREAK byte.
                if !*indefinite && *remaining == 0 {
                    finish = true;
                }
            }
            FrameKind::Map {
                remaining_pairs,
                indefinite,
                phase,
                pending_key_text,
                ..
            } => {
                if !matches!(phase, MapPhase::ExpectValue) {
                    return Err(data_contract());
                }
                let pending = pending_key_text.take().ok_or_else(data_contract)?;
                let owned_text;
                let key_text = match pending {
                    PendingKey::Range(range) => core::str::from_utf8(bytes.get(range).ok_or_else(data_contract)?)
                        .map_err(|_| data_contract())?,
                    PendingKey::Owned(text) => {
                        owned_text = text;
                        owned_text.as_str()
                    }
                };
                self.builder
                    .add_occurrence(
                        LocalOwnerRef::Node(owner),
                        MEMBER_ROLE,
                        Some(AccountedOccurrenceKey::Text(key_text)),
                        node,
                        resources,
                    )
                    .map_err(map_data)?;
                *remaining_pairs -= 1;
                if !*indefinite && *remaining_pairs == 0 {
                    finish = true;
                } else {
                    *phase = MapPhase::ExpectKey;
                }
            }
        }
        if finish {
            let frame = self.frames.pop().ok_or_else(data_contract)?;
            debug_assert_eq!(frame.node, owner);
            // Tags that were pending when this container opened wrap the whole container: restore them so the produced
            // node's delivery applies them (innermost first).
            self.restore_container_tags(&frame);
            self.fill_container_span(&frame)?;
            self.produced = Some(owner);
        }
        Ok(())
    }

    /// Reads the next value head and dispatches: recognized-tag payloads, scalar items, or container opens.
    fn read_value_head(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        let bytes = source.bytes();
        // A recognized tag (0-5) as the innermost pending tag parses its payload structurally (raw-shape law first),
        // then projects or retains.
        if self.pending_tags.as_slice().last().is_some_and(|&tag| tag <= 5) {
            let payload = self.parse_key_value(source, resources)?;
            let tag = self.pending_tags.pop().ok_or_else(data_contract)?;
            // The tag's head offset rides the same stack; it has no node of its own, so it is discarded — the item
            // span below covers the WHOLE chain from its first head.
            self.pending_tag_starts.pop();
            let node = self.project_or_retain(tag, payload, resources)?;
            // `parse_key_value` advanced past the payload: the item's extent is its first tag head through the
            // payload's end.
            self.bind_item_span(node, self.pos)?;
            self.produced = Some(node);
            return Ok(());
        }
        let (head, next) = read::head(bytes, self.pos).map_err(|error| read_error(source, self.pos, error))?;
        match (head.major, head.arg) {
            (Major::Tag, Arg::UInt(tag)) => {
                self.pending_tags.push(tag);
                self.pending_tag_starts.push(self.pos);
                self.pos = next;
                Ok(())
            }
            (Major::UInt, Arg::UInt(value)) => {
                // Non-allocating machine render for the i64 range (the common case); the u64 tail past i64::MAX keeps
                // the format! fallback. The builder copies the spelling into its tracked storage during add_node, so
                // the inline borrow lives only for that call.
                let integer = i64::try_from(value).ok().map(jqf_data::Integer::from_i64);
                let text: alloc::borrow::Cow<'_, str> = match &integer {
                    Some(integer) => alloc::borrow::Cow::Borrowed(integer.as_str()),
                    None => alloc::borrow::Cow::Owned(alloc::format!("{value}")),
                };
                let node = self
                    .builder
                    .add_node(
                        SCALAR_KIND,
                        AccountedSemanticNode::Integer(text.as_ref()),
                        None,
                        resources,
                    )
                    .map_err(map_data)?;
                self.finish_value(node, next)?;
                Ok(())
            }
            (Major::NegInt, Arg::UInt(value)) => {
                // The same machine render for the common i64 range; `-1 - value` fits i64 exactly when `value` does.
                let integer = i64::try_from(value)
                    .ok()
                    .map(|value| jqf_data::Integer::from_i64(-1 - value));
                let text: alloc::borrow::Cow<'_, str> = match &integer {
                    Some(integer) => alloc::borrow::Cow::Borrowed(integer.as_str()),
                    None => alloc::borrow::Cow::Owned(neg_int_text(value)),
                };
                let node = self
                    .builder
                    .add_node(
                        SCALAR_KIND,
                        AccountedSemanticNode::Integer(text.as_ref()),
                        None,
                        resources,
                    )
                    .map_err(map_data)?;
                self.finish_value(node, next)?;
                Ok(())
            }
            (Major::Bytes, arg) => self.read_bytes(arg, next, source, resources),
            (Major::Text, arg) => self.read_text(arg, next, source, resources),
            (Major::Array, Arg::UInt(count)) => {
                let start = self.item_start.take().unwrap_or(self.pos);
                self.open_array(count, false, next, start, source, resources)
            }
            (Major::Array, Arg::Indef) => {
                let start = self.item_start.take().unwrap_or(self.pos);
                self.open_array(u64::MAX, true, next, start, source, resources)
            }
            (Major::Map, Arg::UInt(count)) => {
                let start = self.item_start.take().unwrap_or(self.pos);
                self.open_map(count, false, next, start, source, resources)
            }
            (Major::Map, Arg::Indef) => {
                let start = self.item_start.take().unwrap_or(self.pos);
                self.open_map(u64::MAX, true, next, start, source, resources)
            }
            (Major::Simple, Arg::UInt(kind)) => {
                let kind = u8::try_from(kind).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?;
                // The float rows fold their payload bytes into the head's end, so the payload begins `width` bytes
                // before `next`.
                let payload_start = next.saturating_sub(float_width(kind));
                let node = self.simple_value(kind, payload_start, source, resources)?;
                self.finish_value(node, next)?;
                Ok(())
            }
            _ => Err(crate::error::invalid(
                source,
                self.pos,
                "invalid-argument",
                "reserved or invalid additional-information argument",
            )),
        }
    }

    fn read_bytes(
        &mut self,
        arg: Arg,
        next: usize,
        source: ResolvedSource<'_>,
        resources: &ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        let bytes = source.bytes();
        // A definite-length payload is the source slice itself — borrow it into the builder (which copies
        // synchronously) instead of materializing an intermediate `Vec` copy of the same bytes.
        let (payload, end) = match arg {
            Arg::UInt(length) => {
                let end = checked_end_with_diagnostic(source, next, length)?;
                let payload = bytes
                    .get(next..end)
                    .ok_or_else(|| read_error(source, next, ReadError::Eof))?;
                (alloc::borrow::Cow::Borrowed(payload), end)
            }
            Arg::Indef => {
                self.pos = next;
                let (chunks, end) = self.read_string_chunks(source, false)?;
                (alloc::borrow::Cow::Owned(concat_chunks(&chunks)), end)
            }
        };
        let node = self
            .builder
            .add_node(
                SCALAR_KIND,
                AccountedSemanticNode::Bytes(payload.as_ref()),
                None,
                resources,
            )
            .map_err(map_data)?;
        self.finish_value(node, end)?;
        Ok(())
    }

    fn read_text(
        &mut self,
        arg: Arg,
        next: usize,
        source: ResolvedSource<'_>,
        resources: &ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        let bytes = source.bytes();
        // As [`Decoder::read_bytes`], the definite-length payload is the validated source slice, borrowed not copied.
        let (payload, end) = match arg {
            Arg::UInt(length) => {
                let end = checked_end_with_diagnostic(source, next, length)?;
                let payload = bytes
                    .get(next..end)
                    .ok_or_else(|| read_error(source, next, ReadError::Eof))?;
                (alloc::borrow::Cow::Borrowed(payload), end)
            }
            Arg::Indef => {
                self.pos = next;
                let (chunks, end) = self.read_string_chunks(source, true)?;
                (alloc::borrow::Cow::Owned(concat_chunks(&chunks)), end)
            }
        };
        let text = core::str::from_utf8(payload.as_ref())
            .map_err(|_| crate::error::invalid(source, next, "invalid-utf8", "text string is not valid UTF-8"))?;
        let node = self
            .builder
            .add_node(SCALAR_KIND, AccountedSemanticNode::String(text), None, resources)
            .map_err(map_data)?;
        self.finish_value(node, end)?;
        Ok(())
    }

    /// Reads an indefinite-length string's chunks through BREAK, optionally validating each chunk's UTF-8 (text) as it
    /// lands. See the free [`read_string_chunks`] for the borrowing law.
    fn read_string_chunks<'a>(
        &mut self,
        source: ResolvedSource<'a>,
        text: bool,
    ) -> Result<(alloc::vec::Vec<&'a [u8]>, usize), CodecError> {
        let (chunks, end) = read_string_chunks(source, self.pos, text)?;
        self.pos = end;
        Ok((chunks, end))
    }

    /// Validates one complete child item and commits it as a deferred container span — the empty-path count skeleton.
    #[allow(
        unsafe_code,
        reason = "the bound-span contract is unsafe by jqf-data's design; skip_item just proved these bytes one complete item of the session-owned source"
    )]
    fn defer_container_span(
        &mut self,
        start: usize,
        container: ContainerSpanKind,
        source: ResolvedSource<'_>,
        resources: &ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        let end = crate::walk::skip_item(source, start, resources)?;
        self.pending_tags.clear();
        self.pending_tag_starts.clear();
        self.item_start = None;
        let span = jqf_source::Span::try_new(
            u32::try_from(start).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?,
            u32::try_from(end).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?,
        )
        .ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?;
        let schema = self.schema.as_ref().ok_or_else(data_contract)?;
        let slot = match container {
            ContainerSpanKind::Array => 1,
            ContainerSpanKind::Object => 2,
        };
        let kind = schema.node_kind(slot).ok_or_else(data_contract)?;
        // SAFETY: skip_item validated [start, end) as one complete item of this session's immutable source authority.
        let node = unsafe {
            self.builder
                .add_prepared_bound_container_span_node(schema, kind, span, container, resources)
        }
        .map_err(map_data)?;
        self.pos = end;
        self.spans_committed = true;
        self.produced = Some(node);
        Ok(())
    }

    fn open_array(
        &mut self,
        count: u64,
        indefinite: bool,
        next: usize,
        start: usize,
        source: ResolvedSource<'_>,
        resources: &ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        if self.count_only && !self.frames.is_empty() {
            return self.defer_container_span(start, ContainerSpanKind::Array, source, resources);
        }
        self.pos = next;
        let node = self
            .builder
            .add_node(
                ARRAY_KIND,
                AccountedSemanticNode::Array { item_role: ITEM_ROLE },
                None,
                resources,
            )
            .map_err(map_data)?;
        if count == 0 {
            // An empty container completes immediately; its pending tags apply through the ordinary delivery path. Its
            // authored extent starts at the item's first byte — any leading tag heads included (the same `item_start`
            // law every value's span follows).
            self.bind_span(node, start, next)?;
            self.produced = Some(node);
            return Ok(());
        }
        let prune = self.value_prune(source);
        let depth = resources.enter_nesting_owned().map_err(CodecError::from)?;
        // Tags pending when the container opened wrap the WHOLE container at close, never its items.
        let (tags, tag_starts) = self.take_pending_tags();
        let span_slot = self.reserve_container_span();
        self.push_frame(Frame {
            node,
            kind: FrameKind::Array {
                remaining: count,
                indefinite,
            },
            tags,
            tag_starts,
            start,
            span_slot,
            prune,
            _depth: depth,
        });
        Ok(())
    }

    fn open_map(
        &mut self,
        count: u64,
        indefinite: bool,
        next: usize,
        start: usize,
        source: ResolvedSource<'_>,
        resources: &ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        if self.count_only && !self.frames.is_empty() {
            return self.defer_container_span(start, ContainerSpanKind::Object, source, resources);
        }
        self.pos = next;
        let node = self
            .builder
            .add_node(
                OBJECT_KIND,
                AccountedSemanticNode::Object {
                    member_role: MEMBER_ROLE,
                },
                None,
                resources,
            )
            .map_err(map_data)?;
        if count == 0 {
            self.bind_span(node, start, next)?;
            self.produced = Some(node);
            return Ok(());
        }
        let prune = self.value_prune(source);
        let depth = resources.enter_nesting_owned().map_err(CodecError::from)?;
        let (tags, tag_starts) = self.take_pending_tags();
        let span_slot = self.reserve_container_span();
        self.push_frame(Frame {
            node,
            kind: FrameKind::Map {
                remaining_pairs: count,
                indefinite,
                phase: MapPhase::ExpectKey,
                keys: KeySet::default(),
                pending_key_text: None,
            },
            tags,
            tag_starts,
            start,
            span_slot,
            prune,
            _depth: depth,
        });
        Ok(())
    }

    /// Takes the pending tag chain and its head offsets, leaving empty accounted vectors.
    fn take_pending_tags(&mut self) -> (Vec<u64>, Vec<usize>) {
        (
            core::mem::take(&mut self.pending_tags),
            core::mem::take(&mut self.pending_tag_starts),
        )
    }

    /// Pushes one open-container frame.
    fn push_frame(&mut self, frame: Frame) {
        self.frames.push(frame);
    }

    fn simple_value(
        &mut self,
        kind: u8,
        pos: usize,
        source: ResolvedSource<'_>,
        resources: &ResourceContext<'_>,
    ) -> Result<NodeId, CodecError> {
        // Major-7 floats fold their additional-information value into the argument: 25/26/27 select the width.
        if kind == 25 || kind == 26 || kind == 27 {
            let bits = read_float_bits(source, kind, pos)?;
            return self
                .builder
                .add_node(
                    SCALAR_KIND,
                    AccountedSemanticNode::Float(jqf_data::Float::new(f64::from_bits(bits))),
                    None,
                    resources,
                )
                .map_err(map_data);
        }
        let node = match kind {
            20 => self
                .builder
                .add_node(SCALAR_KIND, AccountedSemanticNode::Bool(false), None, resources)
                .map_err(map_data)?,
            21 => self
                .builder
                .add_node(SCALAR_KIND, AccountedSemanticNode::Bool(true), None, resources)
                .map_err(map_data)?,
            22 => self
                .builder
                .add_node(SCALAR_KIND, AccountedSemanticNode::Null, None, resources)
                .map_err(map_data)?,
            // `undefined` and every other legal simple value project to `Value::Tagged { tag: "cbor:simple:<n>",
            // payload: Null }`.
            other => {
                let mut tag_buf = [0u8; 32];
                let tag = write_cbor_simple_tag(&mut tag_buf, other);
                self.builder
                    .add_node(
                        SCALAR_KIND,
                        AccountedSemanticNode::Null,
                        Some(AccountedIntrinsicTag::Tagged(tag)),
                        resources,
                    )
                    .map_err(map_data)?
            }
        };
        Ok(node)
    }

    /// Wraps one payload node in a tag-layer (`cbor:tag:<n>`).
    fn wrap_layer(&mut self, payload: NodeId, tag: u64, resources: &ResourceContext<'_>) -> Result<NodeId, CodecError> {
        let mut tag_buf = [0u8; 32];
        let tag_text = write_cbor_tag(&mut tag_buf, tag);
        let layer = self
            .builder
            .add_node(
                TAG_KIND,
                AccountedSemanticNode::Unrepresentable,
                Some(AccountedIntrinsicTag::Tagged(tag_text)),
                resources,
            )
            .map_err(map_data)?;
        self.builder
            .add_occurrence(LocalOwnerRef::Node(layer), TAG_PAYLOAD_ROLE, None, payload, resources)
            .map_err(map_data)?;
        Ok(layer)
    }

    /// Reads one complete item into its §5.6.1 identity, advancing the cursor. Parses one map key / recognized-tag
    /// payload into its §5.6.1 identity, advancing the cursor. Delegates to the standalone parser so the walker and
    /// the decoder share one structural key law.
    fn parse_key_value(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &ResourceContext<'_>,
    ) -> Result<KeyValue, CodecError> {
        let (value, next) = parse_key_value(source, self.pos, resources)?;
        self.pos = next;
        Ok(value)
    }

    /// Projects a recognized tag's payload or retains it as a tag-layer.
    fn project_or_retain(
        &mut self,
        tag: u64,
        payload: KeyValue,
        resources: &ResourceContext<'_>,
    ) -> Result<NodeId, CodecError> {
        // The raw-shape law is shared with the byte walker: reject a recognized tag whose payload is not its required
        // shape before any projection.
        validate_tag_payload(tag, &payload)?;
        match tag {
            0 => match &payload {
                KeyValue::Text(bytes) => {
                    let text = core::str::from_utf8(bytes).map_err(|_| data_contract())?;
                    match parse_rfc3339(text) {
                        Ok(datetime) => self
                            .builder
                            .add_node(
                                SCALAR_KIND,
                                AccountedSemanticNode::OffsetDateTime(&datetime),
                                None,
                                resources,
                            )
                            .map_err(map_data),
                        Err(Rfc3339Error::Syntax) => Err(malformed()),
                        Err(Rfc3339Error::OutOfRange) => {
                            let text_node = self
                                .builder
                                .add_node(SCALAR_KIND, AccountedSemanticNode::String(text), None, resources)
                                .map_err(map_data)?;
                            self.wrap_layer(text_node, 0, resources)
                        }
                    }
                }
                _ => Err(malformed()),
            },
            1 => self.project_epoch(payload, resources),
            2 => match &payload {
                KeyValue::Bytes(bytes) => {
                    let value = bignum_from_payload(bytes)?;
                    let text = value.to_decimal_string();
                    self.builder
                        .add_node(SCALAR_KIND, AccountedSemanticNode::Integer(&text), None, resources)
                        .map_err(map_data)
                }
                _ => Err(malformed()),
            },
            3 => match &payload {
                KeyValue::Bytes(bytes) => {
                    let value = bignum_from_payload(bytes)?.add_small(1).negated();
                    let text = value.to_decimal_string();
                    self.builder
                        .add_node(SCALAR_KIND, AccountedSemanticNode::Integer(&text), None, resources)
                        .map_err(map_data)
                }
                _ => Err(malformed()),
            },
            4 => self.project_decimal(payload, resources),
            5 => self.project_bigfloat(payload, resources),
            _ => Err(data_contract()),
        }
    }

    fn project_epoch(&mut self, mut payload: KeyValue, resources: &ResourceContext<'_>) -> Result<NodeId, CodecError> {
        match &mut payload {
            KeyValue::BasicInt { negative, magnitude } => {
                let magnitude = core::mem::replace(magnitude, Big::zero());
                let value = if *negative { magnitude.negated() } else { magnitude };
                if let Some(seconds) = value.to_i64()
                    && let Some(datetime) = epoch_integer_to_offset(seconds)
                {
                    return self
                        .builder
                        .add_node(
                            SCALAR_KIND,
                            AccountedSemanticNode::OffsetDateTime(&datetime),
                            None,
                            resources,
                        )
                        .map_err(map_data);
                }
                let text = value.to_decimal_string();
                let integer = self
                    .builder
                    .add_node(SCALAR_KIND, AccountedSemanticNode::Integer(&text), None, resources)
                    .map_err(map_data)?;
                self.wrap_layer(integer, 1, resources)
            }
            KeyValue::Float(bits) => {
                let value = f64::from_bits(*bits);
                if let Some(datetime) = epoch_float_to_offset(value) {
                    return self
                        .builder
                        .add_node(
                            SCALAR_KIND,
                            AccountedSemanticNode::OffsetDateTime(&datetime),
                            None,
                            resources,
                        )
                        .map_err(map_data);
                }
                let float = self
                    .builder
                    .add_node(
                        SCALAR_KIND,
                        AccountedSemanticNode::Float(jqf_data::Float::new(value)),
                        None,
                        resources,
                    )
                    .map_err(map_data)?;
                self.wrap_layer(float, 1, resources)
            }
            _ => Err(malformed()),
        }
    }

    fn project_decimal(
        &mut self,
        mut payload: KeyValue,
        resources: &ResourceContext<'_>,
    ) -> Result<NodeId, CodecError> {
        let KeyValue::Array(items) = &mut payload else {
            return Err(malformed());
        };
        let mut iter = core::mem::take(items).into_iter();
        let Some(exponent) = iter.next().and_then(KeyValue::take_as_exponent) else {
            return Err(malformed());
        };
        let Some(mantissa) = iter.next().and_then(KeyValue::take_as_integer) else {
            return Err(malformed());
        };
        // The exponent must fit i64 for the core Decimal scale; an exponent beyond it retains the exact tagged payload
        // with its ORIGINAL parts.
        let Some(exponent_i64) = exponent.to_i64() else {
            return self.retain_pair(4, &exponent, &mantissa, resources);
        };
        // The core Decimal law demands a canonical coefficient: remove every factor of ten from the mantissa, folding
        // it into the scale (scale = -exponent after normalization).
        let (coefficient, normalized_exponent) = normalize_decimal_parts(mantissa.clone(), exponent_i64);
        let Some(scale) = normalized_exponent.checked_neg() else {
            return self.retain_pair(4, &exponent, &mantissa, resources);
        };
        let coefficient_text = coefficient.to_decimal_string();
        let projectable = jqf_data::Integer::parse(&coefficient_text)
            .is_ok_and(|coefficient| jqf_data::Decimal::from_parts(coefficient, scale).is_ok());
        if projectable {
            self.builder
                .add_node(
                    SCALAR_KIND,
                    AccountedSemanticNode::Decimal {
                        coefficient: &coefficient_text,
                        scale,
                    },
                    None,
                    resources,
                )
                .map_err(map_data)
        } else {
            self.retain_pair(4, &exponent, &mantissa, resources)
        }
    }

    fn project_bigfloat(
        &mut self,
        mut payload: KeyValue,
        resources: &ResourceContext<'_>,
    ) -> Result<NodeId, CodecError> {
        let KeyValue::Array(items) = &mut payload else {
            return Err(malformed());
        };
        let mut iter = core::mem::take(items).into_iter();
        let Some(exponent) = iter.next().and_then(KeyValue::take_as_exponent) else {
            return Err(malformed());
        };
        let Some(mantissa) = iter.next().and_then(KeyValue::take_as_integer) else {
            return Err(malformed());
        };
        // No core BigFloat: validate the shape then retain the exact tagged payload.
        self.retain_pair(5, &exponent, &mantissa, resources)
    }

    /// Retains a tag-4/5 pair as `Value::Tagged { tag, payload: [exp, mant] }` using the ORIGINAL wire exponent and
    /// mantissa.
    fn retain_pair(
        &mut self,
        tag: u64,
        exponent: &Big,
        mantissa: &Big,
        resources: &ResourceContext<'_>,
    ) -> Result<NodeId, CodecError> {
        let exponent_text = exponent.to_decimal_string();
        let exponent_node = self
            .builder
            .add_node(
                SCALAR_KIND,
                AccountedSemanticNode::Integer(&exponent_text),
                None,
                resources,
            )
            .map_err(map_data)?;
        let mantissa_text = mantissa.to_decimal_string();
        let mantissa_node = self
            .builder
            .add_node(
                SCALAR_KIND,
                AccountedSemanticNode::Integer(&mantissa_text),
                None,
                resources,
            )
            .map_err(map_data)?;
        let array = self
            .builder
            .add_node(
                ARRAY_KIND,
                AccountedSemanticNode::Array { item_role: ITEM_ROLE },
                None,
                resources,
            )
            .map_err(map_data)?;
        self.builder
            .add_occurrence(LocalOwnerRef::Node(array), ITEM_ROLE, None, exponent_node, resources)
            .map_err(map_data)?;
        self.builder
            .add_occurrence(LocalOwnerRef::Node(array), ITEM_ROLE, None, mantissa_node, resources)
            .map_err(map_data)?;
        self.wrap_layer(array, tag, resources)
    }
}

/// The raw-shape law for a recognized tag: tag 0 requires RFC 3339 text, tag 1 an integer or float, tags 2/3 a byte
/// string, tags 4/5 an exponent/ mantissa pair. This is the single source both [`Decoder::project_or_retain`] and the
/// byte walker validate against, so a walk can never accept a recognized tag the decoder rejects.
pub(crate) fn validate_tag_payload(tag: u64, payload: &KeyValue) -> Result<(), CodecError> {
    match tag {
        0 => match payload {
            KeyValue::Text(bytes) => {
                // The text was already validated as UTF-8 when the KeyValue was parsed; borrow it instead of cloning
                // into a String just to hand [`crate::datetime::parse_rfc3339`] a `&str`. Syntax errors refuse; an
                // out-of-range civil date is retained as a tag-layer by the decoder, so the walk must accept it too
                // (the refusal-clause match below).
                let text = core::str::from_utf8(bytes).map_err(|_| data_contract())?;
                match parse_rfc3339(text) {
                    Ok(_) | Err(Rfc3339Error::OutOfRange) => Ok(()),
                    Err(Rfc3339Error::Syntax) => Err(malformed()),
                }
            }
            _ => Err(malformed()),
        },
        1 => match payload {
            KeyValue::BasicInt { .. } | KeyValue::Float(_) => Ok(()),
            _ => Err(malformed()),
        },
        2 | 3 => match payload {
            KeyValue::Bytes(bytes) if bytes.len() > MAX_BIGNUM_BYTES => Err(unrepresentable()),
            KeyValue::Bytes(_) => Ok(()),
            _ => Err(malformed()),
        },
        4 | 5 => match payload {
            KeyValue::Array(items) if items.len() == 2 => {
                if items[0].clone().take_as_exponent().is_some() && items[1].clone().take_as_integer().is_some() {
                    Ok(())
                } else {
                    Err(malformed())
                }
            }
            _ => Err(malformed()),
        },
        _ => Ok(()),
    }
}

/// Reads an indefinite-length string's chunks through BREAK from `pos`, optionally validating each chunk's UTF-8 (text)
/// as it lands. Returns the chunks — each borrowed straight from the source, so nothing is copied here;
/// [`concat_chunks`] owns the one copy — and the offset just past the BREAK byte.
pub(crate) fn read_string_chunks(
    source: ResolvedSource<'_>,
    mut pos: usize,
    text: bool,
) -> Result<(alloc::vec::Vec<&[u8]>, usize), CodecError> {
    let bytes = source.bytes();
    let mut chunks = alloc::vec::Vec::new();
    loop {
        let (head, next) = read::head(bytes, pos).map_err(|error| read_error(source, pos, error))?;
        // RFC 8949 §3.2.3: every chunk must share the opening head's major type; a TEXT chunk inside indefinite BYTES
        // (or the reverse, or a nested indefinite head) is not well-formed.
        match (head.major, head.arg, text) {
            (Major::Bytes, Arg::UInt(length), false) | (Major::Text, Arg::UInt(length), true) => {
                let end = checked_slice_end(source, next, length)?;
                let chunk = bytes
                    .get(next..end)
                    .ok_or_else(|| read_error(source, next, ReadError::Eof))?;
                if text {
                    core::str::from_utf8(chunk).map_err(|_| {
                        crate::error::invalid(source, next, "invalid-utf8", "text string is not valid UTF-8")
                    })?;
                }
                chunks.push(chunk);
                pos = end;
            }
            (Major::Simple, Arg::Indef, _) => return Ok((chunks, pos + 1)),
            _ => {
                return Err(crate::error::invalid(
                    source,
                    pos,
                    "bad-chunk",
                    "indefinite string chunk is not a definite string of the same major type",
                ));
            }
        }
    }
}

/// One OPEN container or tag head while a §5.6.1 key identity is built, standing in for the native frame the parser
/// used to recurse into. The stack is bounded by the governed nesting ceiling: every frame holds the [`DepthGuard`]
/// that admitted its level, so a document deeper than the ceiling is refused before its frame is pushed.
enum KeyFrame<'a> {
    /// An open array or map. `items` collects the children in WIRE order — a map owes a key AND a value per declared
    /// pair, so its items pair off when the frame closes, and `items.len()` parity is the pair position.
    Container {
        items: alloc::vec::Vec<KeyValue>,
        /// Items still owed at this level; unused while `indefinite`.
        owed: u64,
        /// An indefinite-length container: it ends at a break byte, not a count.
        indefinite: bool,
        /// Whether the level is a MAP (pairs) rather than an array.
        map: bool,
        _guard: DepthGuard<'a>,
    },
    /// An open tag head. A tag wraps exactly ONE value, so the content's completion closes the frame and folds the
    /// tagged identity.
    Tag { number: u64, _guard: DepthGuard<'a> },
}

/// Parses one complete item at `pos` into its §5.6.1 identity, returning the value and the offset just past it. This
/// is the shared structural parser for map keys and recognized-tag payloads: it validates UTF-8 text, indefinite
/// strings, nested containers, and tag content exactly as the decoder does.
///
/// The descent is an explicit builder stack, NOT recursion. A CBOR map key may be any value, so a key of `0x81`/`0xc6`
/// repeated is one open level per byte of untrusted input: recursing here overflowed the native stack well inside the
/// governed nesting ceiling (an abort, not a typed failure), and a depth guard does not fix that — `10_000` admitted
/// levels are still `10_000` native frames. The guard keeps its own job and bounds this stack with it, so the frames
/// need no cap of their own.
pub(crate) fn parse_key_value<'a>(
    source: ResolvedSource<'_>,
    pos: usize,
    resources: &'a ResourceContext<'_>,
) -> Result<(KeyValue, usize), CodecError> {
    let bytes = source.bytes();
    let mut frames: alloc::vec::Vec<KeyFrame<'a>> = alloc::vec::Vec::new();
    let mut pos = pos;
    loop {
        // The innermost container either CLOSES here or owes one more item. A tag frame owes exactly one item and
        // closes on delivery instead.
        let mut closed = None;
        if let Some(KeyFrame::Container {
            items,
            owed,
            indefinite,
            map,
            ..
        }) = frames.last_mut()
        {
            if *indefinite {
                // A break byte ends the level — but only where an item STARTS a map pair, so a break in a value
                // position stays the reserved-argument failure it always was.
                if !*map || items.len().is_multiple_of(2) {
                    let (head, _) = read::head(bytes, pos).map_err(|error| read_error(source, pos, error))?;
                    if head.major == Major::Simple && head.arg == Arg::Indef {
                        pos += 1;
                        closed = Some((core::mem::take(items), *map));
                    }
                }
            } else if *owed == 0 {
                closed = Some((core::mem::take(items), *map));
            } else {
                *owed -= 1;
            }
        }
        let mut value = if let Some((items, map)) = closed {
            frames.pop();
            build_key_container(items, map)
        } else {
            // One item: a container or tag head OPENS a frame instead of descending; every other head is a scalar
            // identity, complete here.
            let guard = resources.enter_nesting().map_err(CodecError::from)?;
            let (head, next) = read::head(bytes, pos).map_err(|error| read_error(source, pos, error))?;
            match (head.major, head.arg) {
                (Major::Array, arg) => {
                    pos = next;
                    frames.push(open_key_container(arg, false, guard));
                    continue;
                }
                (Major::Map, arg) => {
                    pos = next;
                    frames.push(open_key_container(arg, true, guard));
                    continue;
                }
                (Major::Tag, Arg::UInt(number)) => {
                    pos = next;
                    frames.push(KeyFrame::Tag { number, _guard: guard });
                    continue;
                }
                _ => {
                    let (value, end) = parse_key_scalar(source, head, next)?;
                    pos = end;
                    value
                }
            }
        };
        // Deliver the completed value to the innermost open frame, folding every tag frame it closes. An empty stack
        // means the item this call parses is complete.
        loop {
            let Some(frame) = frames.last_mut() else {
                return Ok((value, pos));
            };
            match frame {
                KeyFrame::Container { items, .. } => {
                    items.push(value);
                    break;
                }
                // Only a tag frame falls through to the pop below: it is closed by the value it wraps.
                KeyFrame::Tag { number, .. } => value = fold_tagged_key(*number, value)?,
            }
            frames.pop();
        }
    }
}

/// Parses one SCALAR key identity from an already-read head, returning it and the offset just past it. Containers and
/// tags never reach here — the walker opens a frame for those — so every remaining head either is a scalar shape or
/// is the reserved argument the decoder always refused.
fn parse_key_scalar(
    source: ResolvedSource<'_>,
    head: read::Head,
    next: usize,
) -> Result<(KeyValue, usize), CodecError> {
    let bytes = source.bytes();
    match (head.major, head.arg) {
        (Major::UInt, Arg::UInt(value)) => Ok((
            KeyValue::BasicInt {
                negative: false,
                magnitude: Big::from_u64(value),
            },
            next,
        )),
        (Major::NegInt, Arg::UInt(value)) => Ok((
            KeyValue::BasicInt {
                negative: true,
                magnitude: Big::from_u64(value).add_small(1),
            },
            next,
        )),
        (Major::Bytes, Arg::UInt(length)) => {
            let end = checked_slice_end(source, next, length)?;
            let payload = bytes
                .get(next..end)
                .ok_or_else(|| read_error(source, next, ReadError::Eof))?;
            Ok((KeyValue::Bytes(payload.to_vec()), end))
        }
        (Major::Bytes, Arg::Indef) => {
            let (chunks, end) = read_string_chunks(source, next, false)?;
            Ok((KeyValue::Bytes(concat_chunks(&chunks)), end))
        }
        (Major::Text, Arg::UInt(length)) => {
            let end = checked_slice_end(source, next, length)?;
            let payload = bytes
                .get(next..end)
                .ok_or_else(|| read_error(source, next, ReadError::Eof))?;
            core::str::from_utf8(payload)
                .map_err(|_| crate::error::invalid(source, next, "invalid-utf8", "text string is not valid UTF-8"))?;
            Ok((KeyValue::Text(payload.to_vec()), end))
        }
        (Major::Text, Arg::Indef) => {
            let (chunks, end) = read_string_chunks(source, next, true)?;
            Ok((KeyValue::Text(concat_chunks(&chunks)), end))
        }
        (Major::Simple, Arg::UInt(kind)) => {
            let kind = u8::try_from(kind).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?;
            if kind == 25 || kind == 26 || kind == 27 {
                let payload_start = next.saturating_sub(float_width(kind));
                let bits = read_float_bits(source, kind, payload_start)?;
                Ok((KeyValue::Float(bits), next))
            } else {
                Ok((KeyValue::Simple(kind), next))
            }
        }
        _ => Err(read_error(source, next, ReadError::ReservedArgument)),
    }
}

/// Opens one container level for the key parser: the items the level owes (a map owes a key AND a value per declared
/// pair — a count past `u64::MAX / 2` saturates, which the input runs out of bytes long before, reporting the same
/// `Eof` the pair-counted loop reported) and the nesting level it holds open.
fn open_key_container(arg: Arg, map: bool, guard: DepthGuard<'_>) -> KeyFrame<'_> {
    let (owed, indefinite) = match (arg, map) {
        (Arg::UInt(count), true) => (count.saturating_mul(2), false),
        (Arg::UInt(count), false) => (count, false),
        (Arg::Indef, _) => (0, true),
    };
    KeyFrame::Container {
        items: alloc::vec::Vec::new(),
        owed,
        indefinite,
        map,
        _guard: guard,
    }
}

/// Builds one closed container's identity from the items it collected. A map's flat item list is even by construction
/// — a definite map owes two items per declared pair, and an indefinite one honours its break only where a pair
/// starts — so the pairing consumes every item.
fn build_key_container(items: alloc::vec::Vec<KeyValue>, map: bool) -> KeyValue {
    if !map {
        return KeyValue::Array(items);
    }
    debug_assert!(items.len().is_multiple_of(2), "a map level owes whole pairs");
    let mut pairs = alloc::vec::Vec::with_capacity(items.len() / 2);
    let mut items = items.into_iter();
    while let (Some(key), Some(value)) = (items.next(), items.next()) {
        pairs.push((key, value));
    }
    KeyValue::Map(pairs)
}

/// Folds one tag head and its completed content into the tagged identity: the recognized bignum/decimal/bigfloat shapes
/// project, everything else stays an uninterpreted `Tagged`.
fn fold_tagged_key(tag: u64, content: KeyValue) -> Result<KeyValue, CodecError> {
    // The recognized shapes are read through a BORROW: `KeyValue` owns a manual `Drop` (the iterative dismantle), so
    // its payload cannot be moved out by pattern. A shape that does not project falls through to `Tagged` with the
    // content untouched, exactly as the by-value match did.
    let projected = match (tag, &content) {
        (2, KeyValue::Bytes(bytes)) => Some(KeyValue::Bignum2(bignum_from_payload(bytes)?)),
        (3, KeyValue::Bytes(bytes)) => Some(KeyValue::Bignum3(bignum_from_payload(bytes)?)),
        (4 | 5, KeyValue::Array(items)) if items.len() == 2 => {
            match (items[0].clone().take_as_exponent(), items[1].clone().take_as_integer()) {
                (Some(exponent), Some(mantissa)) => Some(if tag == 4 {
                    KeyValue::Decimal {
                        coefficient: mantissa,
                        exponent,
                    }
                } else {
                    KeyValue::Bigfloat { mantissa, exponent }
                }),
                _ => None,
            }
        }
        // Tags 0/1 need no raw-shape check here: a payload that is not its required shape folds to `Tagged`, and every
        // consumer of that fold rejects non-text keys and validates recognized-tag payloads through
        // `validate_tag_payload`, so an unchecked 0/1 can never reach a built document.
        _ => None,
    };
    Ok(projected.unwrap_or_else(|| KeyValue::Tagged {
        number: tag,
        content: alloc::boxed::Box::new(content),
    }))
}

/// Writes `cbor:tag:{n}` into `buf` without allocating.
fn write_cbor_tag(buf: &mut [u8; 32], tag: u64) -> &str {
    write_prefixed_u64(buf, b"cbor:tag:", tag)
}

/// Writes `cbor:simple:{n}` into `buf` without allocating.
fn write_cbor_simple_tag(buf: &mut [u8; 32], simple: u8) -> &str {
    write_prefixed_u64(buf, b"cbor:simple:", u64::from(simple))
}

fn write_prefixed_u64<'a>(buf: &'a mut [u8; 32], prefix: &[u8], mut value: u64) -> &'a str {
    debug_assert!(prefix.len() + 20 <= buf.len());
    buf[..prefix.len()].copy_from_slice(prefix);
    let mut digits = [0u8; 20];
    let mut start = digits.len();
    if value == 0 {
        start -= 1;
        digits[start] = b'0';
    } else {
        while value > 0 {
            start -= 1;
            digits[start] = b'0' + (value % 10) as u8;
            value /= 10;
        }
    }
    let written = digits.len() - start;
    let offset = prefix.len();
    buf[offset..offset + written].copy_from_slice(&digits[start..]);
    core::str::from_utf8(&buf[..offset + written]).unwrap_or("")
}

/// The largest tag-2/3 bignum payload the decoder accepts.
///
/// Rendering a bignum's decimal spelling is quadratic in its byte length (repeated division over the limb vector), and
/// the length arrives from an untrusted length prefix — so without a ceiling a multi-megabyte byte string is one
/// unbounded, uninterruptible conversion inside a single poll, multiplied once per map key by the §5.6.1 fingerprint.
/// 1024 bytes is an 8192-bit integer: past every real bignum, and bounded work by construction (a 256-limb conversion),
/// which is why the division loop needs no work-meter yield of its own.
const MAX_BIGNUM_BYTES: usize = 1024;

/// Builds one tag-2/3 bignum magnitude, refusing a payload past the ceiling.
fn bignum_from_payload(bytes: &[u8]) -> Result<Big, CodecError> {
    if bytes.len() > MAX_BIGNUM_BYTES {
        return Err(unrepresentable());
    }
    Ok(Big::from_big_endian(bytes))
}

/// The negative-integer value `-1 - n` as canonical text.
fn neg_int_text(value: u64) -> alloc::string::String {
    let wide = -(value as i128) - 1;
    alloc::format!("{wide}")
}

/// Removes factors of ten from the mantissa, raising the exponent (the core Decimal canonical-coefficient law).
fn normalize_decimal_parts(mantissa: Big, exponent: i64) -> (Big, i64) {
    let mut mantissa = mantissa;
    let mut exponent = exponent;
    // A zero magnitude is already canonical at scale 0: it has infinitely many factors of ten, so its exponent is
    // irrelevant (0.0, 0.00, and 0E+5 all canonicalize to coefficient 0 at scale 0).
    if mantissa.is_zero() {
        return (mantissa, 0);
    }
    loop {
        let (quotient, remainder) = mantissa.div_rem_small(10);
        if remainder == 0 {
            mantissa = quotient;
            exponent = exponent.saturating_add(1);
        } else {
            break;
        }
    }
    (mantissa, exponent)
}

/// Counts the node/occurrence totals one complete item will build, for the one-shot arena reservation. Walks the wire
/// structurally — heads, definite lengths, container extents — summing exactly what the decoder will add and add a
/// node/occurrence for: an array of `count` elements is `count` child nodes plus `count` item occurrences; a map of
/// `count` pairs is `count` value nodes plus `count` member occurrences (keys are occurrence key text, not nodes). Tags
/// are payload-transparent; definite strings advance by their length prefix. The scan validates NOTHING (the real
/// decode does) and stops early on any shape it cannot price (indefinite lengths, malformed heads), leaving the builder
/// to amortized growth.
///
/// The descent is an explicit stack, NOT recursion. This scan runs BEFORE the decoder's depth-guarded walk, and a
/// payload of repeated `0x81`/`0xc6` is one container head per byte: recursing here overflowed the native stack on a
/// few hundred KB of input, and a depth guard alone does not fix that (the governed ceiling is `10_000` levels, more
/// native frames than a thread has). The stack is bounded at [`MAX_SCAN_DEPTH`] because the reservation is only a hint
/// — past it the scan stops and the builder grows amortized.
fn count_nodes(bytes: &[u8], pos: usize, nodes: &mut usize, occurrences: &mut usize) -> usize {
    // Items still to price at each open container level; the root level owes the single item this call prices. A fixed
    // stack so a bounded prefix scan never heap-allocates its cursor.
    let mut owed = [0usize; MAX_SCAN_DEPTH];
    owed[0] = 1;
    let mut depth = 1usize;
    let mut cursor = pos;
    let limit = bytes.len();
    while depth > 0 {
        let level = owed[depth - 1];
        if level == 0 {
            depth -= 1;
            continue;
        }
        if cursor >= limit {
            return cursor;
        }
        let Ok((head, next)) = read::head(bytes, cursor) else {
            return cursor;
        };
        // A tag is payload-transparent: the level still owes the value it wraps, so the head is consumed without
        // closing an item.
        if let (Major::Tag, Arg::UInt(_)) = (head.major, head.arg) {
            cursor = next;
            continue;
        }
        let opened = match (head.major, head.arg) {
            (Major::Array | Major::Map, Arg::UInt(count)) => {
                let count = usize::try_from(count).unwrap_or(usize::MAX);
                // A corrupt input can declare a count far past the actual bytes (or past the end of the input). The
                // DECODER stops on Eof; this scan must too, or it spins `count` times over the same tail. Bounding the
                // level by the remaining bytes keeps the scan linear and preserves the decoder's fail-fast on truncated
                // containers. The bound must ALSO gate what reaches the reservation: a 5-byte array declaring a billion
                // elements used to price a billion nodes into `try_reserve` — a 43 GB malloc from a 5-byte input.
                let bound = count.min(bytes.len().saturating_sub(next));
                *nodes = nodes.saturating_add(bound);
                *occurrences = occurrences.saturating_add(bound);
                cursor = next;
                // A map owes a key AND a value per declared pair.
                match head.major {
                    Major::Map => bound.saturating_mul(2),
                    _ => bound,
                }
            }
            (Major::UInt | Major::NegInt | Major::Simple, Arg::UInt(_)) => {
                cursor = next;
                0
            }
            (Major::Bytes | Major::Text, Arg::UInt(length)) => {
                // The scan validates nothing and prices only a hint, so a payload past the end of input simply stops
                // the scan here; the real decode reports the truncation with its diagnostic.
                let end = usize::try_from(length)
                    .ok()
                    .and_then(|len| next.checked_add(len))
                    .filter(|&end| end <= bytes.len());
                let Some(end) = end else {
                    return cursor;
                };
                cursor = end;
                0
            }
            // Indefinite lengths and every unhandled shape are not priced: the scan stops and the builder falls back to
            // amortized growth.
            _ => return cursor,
        };
        owed[depth - 1] -= 1;
        if opened > 0 {
            if depth >= MAX_SCAN_DEPTH {
                return cursor;
            }
            owed[depth] = opened;
            depth += 1;
        }
    }
    cursor
}

/// How deep the capacity pre-scan follows nesting. The reservation is a perf hint, so a document nested deeper than
/// this is priced only down to here — no realistic document reaches it, and the cap keeps the scan's own level stack
/// small against an adversarial one-head-per-byte payload.
const MAX_SCAN_DEPTH: usize = 1024;

/// Adds an authored length to a start offset, refusing overflow. Shared arithmetic for the two bounds checks below.
fn length_end(start: usize, length: u64) -> Result<usize, CodecError> {
    let length = usize::try_from(length).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?;
    start
        .checked_add(length)
        .ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))
}

/// A bounds check for one string payload with a source-aware diagnostic, labeled at the end of input (the truncation
/// law).
fn checked_end_with_diagnostic(source: ResolvedSource<'_>, start: usize, length: u64) -> Result<usize, CodecError> {
    let end = length_end(start, length)?;
    if end > source.bytes().len() {
        return Err(crate::error::invalid(
            source,
            source.bytes().len(),
            "truncated",
            "string payload extends past end of input",
        ));
    }
    Ok(end)
}

pub(crate) fn checked_slice_end(source: ResolvedSource<'_>, start: usize, length: u64) -> Result<usize, CodecError> {
    let end = length_end(start, length)?;
    if end > source.bytes().len() {
        return Err(read_error(source, start, ReadError::Eof));
    }
    Ok(end)
}

fn concat_chunks(chunks: &[&[u8]]) -> alloc::vec::Vec<u8> {
    // One allocation sized to the total, one copy per byte: each chunk is extended in place instead of owning a buffer
    // of its own first.
    let mut out = alloc::vec::Vec::with_capacity(chunks.iter().map(|chunk| chunk.len()).sum::<usize>());
    for chunk in chunks {
        out.extend_from_slice(chunk);
    }
    out
}

pub(crate) fn float_width(kind: u8) -> usize {
    match kind {
        25 => 2,
        26 => 4,
        27 => 8,
        _ => 0,
    }
}

/// Reads a major-7 float payload by width and widens it to exact binary64 bits (`f16_fraction << 42` / `f32_fraction <<
/// 29`, sign preserved).
pub(crate) fn read_float_bits(source: ResolvedSource<'_>, kind: u8, pos: usize) -> Result<u64, CodecError> {
    let bytes = source.bytes();
    match kind {
        25 => {
            let raw = bytes
                .get(pos..pos + 2)
                .ok_or_else(|| read_error(source, pos, ReadError::Eof))?;
            Ok(read::widen_f16(read::read_u16(raw, 0)))
        }
        26 => {
            let raw = bytes
                .get(pos..pos + 4)
                .ok_or_else(|| read_error(source, pos, ReadError::Eof))?;
            Ok(jqf_codec_core::widen_f32(read::read_u32(raw, 0)))
        }
        27 => {
            let raw = bytes
                .get(pos..pos + 8)
                .ok_or_else(|| read_error(source, pos, ReadError::Eof))?;
            Ok(read::read_u64(raw, 0))
        }
        _ => Err(read_error(source, pos, ReadError::Eof)),
    }
}

/// Converts one byte-level read failure into its structured diagnostic. A truncation is labeled where the input RAN
/// OUT, not where the item began — the streaming drives' hold/refill law keys on a truncation whose label starts
/// exactly at the window end. Well-formedness faults keep the offset of the offending head.
pub(crate) fn read_error(source: ResolvedSource<'_>, pos: usize, error: ReadError) -> CodecError {
    let (offset, code, message) = match error {
        ReadError::Eof => (source.bytes().len(), "truncated", "input ends inside an item"),
        ReadError::ReservedArgument => (
            pos,
            "reserved-argument",
            "reserved additional-information value is not well-formed",
        ),
        ReadError::ReservedSimple => (
            pos,
            "reserved-simple",
            "two-byte simple value below 32 is not well-formed",
        ),
    };
    crate::error::invalid(source, offset, code, message)
}

/// A bare `InvalidInput` reject for the semantic-shape refusals (tag-payload shapes, §5.6.1 duplicates) that carry no
/// position yet. Deliberately named apart from [`crate::error::invalid`], which builds the structured form.
fn malformed() -> CodecError {
    CodecError::new(CodecFailureKind::InvalidInput)
}

fn unrepresentable() -> CodecError {
    CodecError::new(CodecFailureKind::UnsupportedRepresentation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use core::fmt::Write as _;
    use jqf_codec_core::{
        AccessFootprint, AccessGuarantees, AccessInput, AccessRequirement, CodecDemand, DiagnosticPolicy, ExactPath,
        ExactSelectionRecord, ValidationMode,
    };
    use jqf_data::{CountDemand, CountRow, CountVerdict};
    use jqf_resource::{ContinueControl, RequestAccount, ResourceLimits, WorkMeter};
    use jqf_source::{SourceId, SourceKind, SourceRef};

    static CONTROL: ContinueControl = ContinueControl;

    fn resources() -> ResourceContext<'static> {
        let limits = ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
        let account = RequestAccount::try_new(limits).expect("account allocates");
        let work = WorkMeter::try_new_v1(4_096).expect("work starts");
        ResourceContext::new(account, &CONTROL, work).expect("context starts")
    }

    /// The CLI's real governor shape: the nesting ceiling is 10 000 (the same cap `jqf-cli/src/main.rs` pins), not the
    /// `u32::MAX` the other tests use — the depth-bounded paths can only be exercised against a ceiling that exists.
    fn governed_resources() -> ResourceContext<'static> {
        let limits = ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, 10_000);
        let account = RequestAccount::try_new(limits).expect("account allocates");
        let work = WorkMeter::try_new_v1(4_096).expect("work starts");
        ResourceContext::new(account, &CONTROL, work).expect("context starts")
    }

    fn source(bytes: &[u8]) -> ResolvedSource<'_> {
        ResolvedSource::new(
            SourceRef::new(SourceId::new(77), SourceKind::Input),
            "cbor.bin",
            bytes,
            0,
        )
    }

    /// Drives the whole-route session to a materialized root value, or returns the codec error for a rejected document.
    fn decode(bytes: &[u8]) -> Result<jqf_data::Value, CodecError> {
        let mut ctx = resources();
        let mut state = CborParseState::try_new(
            source(bytes),
            BuilderCoverage::minimal_semantic(),
            None,
            false,
            &mut ctx,
        )
        .expect("session starts");
        let mut run = CodecRunContext::new(&mut ctx);
        let result = state.decode(AccessInput::Source(source(bytes)), &mut run)?;
        let AccessOutcome::FullDocument(product) = result.into_parts().0 else {
            panic!("expected a full document")
        };
        Ok(product.document().materialize_root(&mut ctx).expect("materialize"))
    }

    fn keep_member_tree(name: &str, resources: &ResourceContext<'_>) -> jqf_codec_core::PruneTree {
        let mut tree = jqf_codec_core::PruneTree::try_new(resources).expect("tree");
        let keep = tree.try_push_node(true).expect("keep");
        tree.try_push_key(jqf_codec_core::PruneTree::ROOT, name, keep)
            .expect("key");
        tree
    }

    fn object_keys(value: &jqf_data::Value) -> alloc::vec::Vec<&str> {
        let jqf_data::Value::Object(object) = value else {
            panic!("expected object");
        };
        object.iter().map(jqf_data::ObjectEntry::key).collect()
    }

    fn decode_request() -> jqf_codec_core::DecodeRequest<'static> {
        let dialect: &'static jqf_data::DialectId = alloc::boxed::Box::leak(alloc::boxed::Box::new(
            jqf_data::DialectId::try_new(crate::CBOR_GENERIC_DIALECT_ID).expect("dialect"),
        ));
        jqf_codec_core::DecodeRequest {
            validation: ValidationMode::Strict,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            dialect,
            options: None,
            allow_adjacent_values: false,
            value_separator: &[],
        }
    }

    fn decode_requirement(
        bytes: &[u8],
        requirement: &AccessRequirement,
    ) -> Result<(jqf_data::Value, usize), CodecError> {
        let mut ctx = resources();
        let registration = crate::registration().expect("registration");
        let mut provider = registration
            .decoder()
            .expect("decoder")
            .create_provider(source(bytes), decode_request(), &mut ctx)
            .expect("provider");
        let handle = provider.bind(requirement).expect("bind");
        let mut session = provider.open(&handle, &mut ctx).expect("open");
        let mut run = CodecRunContext::new(&mut ctx);
        run.set_cooperative_credits(4_096);
        let result = session.decode(&mut run)?;
        let outcome = result.into_parts().0;
        let product = match &outcome {
            AccessOutcome::FullDocument(product) => product,
            AccessOutcome::Located(located) => located.product(),
        };
        let nodes = product.document().node_count();
        let value = product.document().materialize_root(&mut ctx).expect("materialize");
        Ok((value, nodes))
    }

    fn tag_chain(value: &jqf_data::Value) -> alloc::vec::Vec<&str> {
        let mut chain = alloc::vec::Vec::new();
        let mut current = value;
        while let jqf_data::Value::Tagged { tag, payload } = current {
            chain.push(tag.as_str());
            current = payload;
        }
        chain
    }

    /// Compact render for readable assertions over the owned value.
    fn render(value: &jqf_data::Value) -> alloc::string::String {
        use jqf_data::Value as V;
        match value {
            V::Null => "null".into(),
            V::Bool(true) => "true".into(),
            V::Bool(false) => "false".into(),
            V::Number(number) => {
                if let Some(integer) = number.to_integer() {
                    integer.as_str().into()
                } else if let Some(float) = number.as_float() {
                    alloc::format!("{}", float.get())
                } else {
                    alloc::format!("{number:?}")
                }
            }
            V::String(text) => alloc::format!("{text:?}"),
            V::Bytes(bytes) => alloc::format!("h{:?}", bytes.as_ref()),
            V::Array(array) => {
                let mut out = alloc::string::String::from("[");
                for (index, item) in array.iter().enumerate() {
                    if index != 0 {
                        out.push_str(", ");
                    }
                    out.push_str(&render(item));
                }
                out.push(']');
                out
            }
            V::Object(object) => {
                let mut out = alloc::string::String::from("{");
                for (index, entry) in object.iter().enumerate() {
                    if index != 0 {
                        out.push_str(", ");
                    }
                    out.push('"');
                    out.push_str(entry.key());
                    out.push('"');
                    out.push_str(": ");
                    out.push_str(&render(entry.value()));
                }
                out.push('}');
                out
            }
            V::Tagged { tag, payload } => {
                alloc::format!("{}({})", tag.as_str(), render(payload))
            }
            V::OffsetDateTime(datetime) => {
                let date = datetime.local.date;
                let time = &datetime.local.time;
                let fraction = time.fraction().digits();
                let mut out = alloc::format!(
                    "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
                    date.year(),
                    date.month(),
                    date.day(),
                    time.hour(),
                    time.minute(),
                    time.second(),
                );
                if !fraction.is_empty() {
                    out.push('.');
                    out.push_str(fraction);
                }
                match datetime.offset {
                    jqf_data::UtcOffset::KnownSeconds(offset) if offset.seconds() == 0 => {
                        out.push('Z');
                    }
                    jqf_data::UtcOffset::KnownSeconds(offset) => {
                        let seconds = offset.seconds();
                        let sign = if seconds < 0 { '-' } else { '+' };
                        let absolute = seconds.unsigned_abs();
                        write!(out, "{sign}{:02}:{:02}", absolute / 3600, (absolute % 3600) / 60)
                            .expect("write into a String cannot fail");
                    }
                    jqf_data::UtcOffset::UnknownLocalOffset => {
                        out.push_str("-00:00");
                    }
                }
                out
            }
            other => alloc::format!("{other:?}"),
        }
    }

    #[test]
    fn decodes_scalars_arrays_and_objects() {
        let value = decode(&[0x83, 0x01, 0x02, 0x03]).expect("array decodes");
        assert_eq!(render(&value), "[1, 2, 3]");
        let value = decode(&[0xa2, 0x61, 0x61, 0x01, 0x61, 0x62, 0x82, 0xf5, 0xf6]).expect("object decodes");
        assert_eq!(render(&value), "{\"a\": 1, \"b\": [true, null]}");
    }

    #[test]
    fn decodes_integer_edges_and_floats() {
        // -1 (0x20), 23 (0x17), 256 (0x19 0x0100), -2^63 edge via 0x3b.
        let value = decode(&[0x20]).expect("negative one");
        assert_eq!(render(&value), "-1");
        let value = decode(&[0x17]).expect("twenty-three");
        assert_eq!(render(&value), "23");
        let value = decode(&[0x19, 0x01, 0x00]).expect("two-byte argument");
        assert_eq!(render(&value), "256");
        // 1.5 as f16 (0x3e00).
        let value = decode(&[0xf9, 0x3e, 0x00]).expect("f16 float");
        assert_eq!(render(&value), "1.5");
    }

    #[test]
    fn zero_mantissa_decimal_projects() {
        // `0.0` encodes as tag 4 `[exp, 0]` (a zero mantissa with a nonzero exponent): a zero coefficient is canonical
        // at scale 0 whatever the authored exponent, so the decoder must project it to 0 rather than retain it as an
        // unrepresentable tag.
        let value = decode(&[0xc4, 0x82, 0x20, 0x00]).expect("decode tag 4 [-1, 0]");
        let jqf_data::Value::Number(number) = value else {
            panic!("expected a number, got {value:?}");
        };
        let decimal = number
            .as_decimal()
            .expect("canonical zero is a decimal, got {number:?}");
        assert_eq!(decimal.scale(), 0);
        assert_eq!(decimal.coefficient().as_str(), "0");
    }

    #[test]
    fn decodes_indefinite_strings() {
        // _("foo" "bar") via 0x7f 0x63 0x66 0x6f 0x6f 0x63 0x62 0x61 0x72 0xff.
        let value =
            decode(&[0x7f, 0x63, 0x66, 0x6f, 0x6f, 0x63, 0x62, 0x61, 0x72, 0xff]).expect("indefinite text decodes");
        assert_eq!(render(&value), "\"foobar\"");
    }

    #[test]
    fn indefinite_string_chunks_must_match_the_opening_major_type() {
        // RFC 8949 §3.2.3 well-formedness: a chunk with the wrong major type is not well-formed. `5f 61 61 41 62 ff`
        // mixes a TEXT chunk into indefinite BYTES; `7f 41 61 41 62 ff` the reverse.
        let error =
            decode(&[0x5f, 0x61, 0x61, 0x41, 0x62, 0xff]).expect_err("text chunk inside indefinite bytes fails");
        assert!(matches!(error.kind(), CodecFailureKind::InvalidInput));
        let error =
            decode(&[0x7f, 0x41, 0x61, 0x41, 0x62, 0xff]).expect_err("bytes chunk inside indefinite text fails");
        assert!(matches!(error.kind(), CodecFailureKind::InvalidInput));
        // The pure forms still decode: _b("a" "b") and _("a" "b").
        let value = decode(&[0x5f, 0x41, 0x61, 0x41, 0x62, 0xff]).expect("indefinite bytes decode");
        assert_eq!(render(&value), "h[97, 98]");
        let value = decode(&[0x7f, 0x61, 0x61, 0x61, 0x62, 0xff]).expect("indefinite text decode");
        assert_eq!(render(&value), "\"ab\"");
    }

    #[test]
    fn tag_chain_materializes_nested_tagged_values() {
        // 55799(34([1, 2])).
        let value = decode(&[0xd9, 0xd9, 0xf7, 0xd8, 0x22, 0x82, 0x01, 0x02]).expect("tag chain decodes");
        assert_eq!(tag_chain(&value), ["cbor:tag:55799", "cbor:tag:34"]);
        assert_eq!(render(value.untagged()), "[1, 2]");
    }

    #[test]
    fn recognized_tags_project() {
        // Tag 2 bignum 1: c2 41 01.
        let value = decode(&[0xc2, 0x41, 0x01]).expect("bignum decodes");
        assert_eq!(render(&value), "1");
        // Tag 3 bignum 1 -> -2: c3 41 01.
        let value = decode(&[0xc3, 0x41, 0x01]).expect("negative bignum decodes");
        assert_eq!(render(&value), "-2");
        // Tag 1 epoch 0 -> 1970-01-01T00:00:00Z.
        let value = decode(&[0xc1, 0x00]).expect("epoch decodes");
        assert_eq!(render(&value), "1970-01-01T00:00:00Z");
        // Tag 0 RFC 3339 text.
        let mut text = vec![0xc0, 0x78, 0x14];
        text.extend_from_slice(b"2024-06-01T12:00:00Z");
        let value = decode(&text).expect("date-time decodes");
        assert_eq!(render(&value), "2024-06-01T12:00:00Z");
    }

    #[test]
    fn undefined_and_simple_values_are_tagged() {
        let value = decode(&[0xf7]).expect("undefined decodes");
        assert_eq!(render(&value), "cbor:simple:23(null)");
    }

    #[test]
    fn duplicate_text_keys_are_rejected() {
        // {"a": 1, "a": 2}.
        let error = decode(&[0xa2, 0x61, 0x61, 0x01, 0x61, 0x61, 0x02]).expect_err("dup keys fail");
        assert!(matches!(error.kind(), CodecFailureKind::InvalidInput));
    }

    /// RFC 8949 §5.6: a BREAK in an indefinite map is legal only in key position. A dangling key is not well-formed
    /// and must not be dropped.
    #[test]
    fn an_indefinite_map_with_a_dangling_key_is_rejected() {
        // {_ "a"} — key then BREAK, no value.
        let error = decode(&[0xbf, 0x61, 0x61, 0xff]).expect_err("dangling key fails");
        assert!(matches!(error.kind(), CodecFailureKind::InvalidInput));
        // {_ "a": 1, "b"} — complete pair then dangling key.
        let error = decode(&[0xbf, 0x61, 0x61, 0x01, 0x61, 0x62, 0xff]).expect_err("trailing dangling key fails");
        assert!(matches!(error.kind(), CodecFailureKind::InvalidInput));
        // [{_ "a"}] — the dangling map is nested.
        let error = decode(&[0x81, 0xbf, 0x61, 0x61, 0xff]).expect_err("nested dangling key fails");
        assert!(matches!(error.kind(), CodecFailureKind::InvalidInput));
        // A well-formed indefinite map still decodes.
        let value = decode(&[0xbf, 0x61, 0x61, 0x01, 0xff]).expect("even pair decodes");
        assert_eq!(render(&value), "{\"a\": 1}");
        // The two-byte simple form is well-formed for value ≥ 32.
        decode(&[0xf8, 0x20]).expect("simple 32 decodes");
    }

    /// Every CBOR item's extent is exact from its header (RFC 8949 §3), so the eager whole-document decode binds each
    /// node's authored span — header through payload — and the span slices the source to the exact bytes that
    /// re-decode to the node's semantic (the `record_authored_span` contract, in binary clothes). Containers span
    /// header through the close (an indefinite container INCLUDES its BREAK byte); a tagged item spans its first tag
    /// head through the value's end.
    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "one span-extent law test covers every item family (object members, containers, indefinite BREAK, tagged items) so the seal discipline has a single witness"
    )]
    fn per_item_spans_slice_the_exact_authored_bytes() {
        // {"a": 1, "b": "xy", "c": h'ff', "d": null}
        //   a4 61 61 01   61 62 62 78 79   61 63 41 ff   61 64 f6
        let bytes: &[u8] = &[
            0xa4, 0x61, 0x61, 0x01, 0x61, 0x62, 0x62, 0x78, 0x79, 0x61, 0x63, 0x41, 0xff, 0x61, 0x64, 0xf6,
        ];
        let mut ctx = resources();
        let mut state = CborParseState::try_new(
            source(bytes),
            BuilderCoverage::minimal_semantic(),
            None,
            false,
            &mut ctx,
        )
        .expect("session starts");
        let src = source(bytes);
        let mut run = CodecRunContext::new(&mut ctx);
        let result = state.decode(AccessInput::Source(src), &mut run).expect("decode");
        let AccessOutcome::FullDocument(product) = result.into_parts().0 else {
            panic!("expected a full document")
        };
        let document = product.document();
        let segment = document
            .source_segment()
            .expect("per-item spans make the document source-backed");
        let member = |key: &str| {
            document
                .value_view(document.root_handle())
                .expect("root view")
                .object()
                .expect("object")
                .expect("root is an object")
                .get(key)
                .expect("member")
                .node()
        };
        // The root container's span: header 0xa4 through its last member.
        let root_span = document
            .node_source_span(document.root())
            .expect("span")
            .expect("root container must carry a span");
        assert_eq!(
            &segment[root_span.start() as usize..root_span.end() as usize],
            bytes,
            "the root container's span is the whole document"
        );
        // Scalars: header through payload, exactly the authored bytes.
        for (key, expected) in [
            ("a", &[0x01][..]),         // uint 1: header alone
            ("b", &[0x62, 0x78, 0x79]), // text "xy": header + 2 payload bytes
            ("c", &[0x41, 0xff][..]),   // bytes h'ff': header + 1 payload byte
            ("d", &[0xf6][..]),         // null: header alone
        ] {
            let span = document
                .node_source_span(member(key))
                .expect("span")
                .unwrap_or_else(|| panic!("{key} must carry a span"));
            assert_eq!(
                &segment[span.start() as usize..span.end() as usize],
                expected,
                "{key}'s span must slice its exact authored bytes"
            );
        }
        // An indefinite container's span includes its BREAK byte: 9f 01 ff.
        let indefinite = &[0x9f, 0x01, 0xff][..];
        let mut ctx = resources();
        let mut state = CborParseState::try_new(
            source(indefinite),
            BuilderCoverage::minimal_semantic(),
            None,
            false,
            &mut ctx,
        )
        .expect("session starts");
        let src = source(indefinite);
        let mut run = CodecRunContext::new(&mut ctx);
        let result = state.decode(AccessInput::Source(src), &mut run).expect("decode");
        let AccessOutcome::FullDocument(product) = result.into_parts().0 else {
            panic!("expected a full document")
        };
        let document = product.document();
        let segment = document.source_segment().expect("source segment");
        let span = document
            .node_source_span(document.root())
            .expect("span")
            .expect("indefinite array must carry a span");
        assert_eq!(
            &segment[span.start() as usize..span.end() as usize],
            indefinite,
            "the indefinite array's span runs through its BREAK byte"
        );
        // A tagged item spans its first tag head through the value's end: d8 06 01 is tag 6 of 1, one item of three
        // bytes.
        let tagged = &[0xd8, 0x06, 0x01][..];
        let mut ctx = resources();
        let mut state = CborParseState::try_new(
            source(tagged),
            BuilderCoverage::minimal_semantic(),
            None,
            false,
            &mut ctx,
        )
        .expect("session starts");
        let src = source(tagged);
        let mut run = CodecRunContext::new(&mut ctx);
        let result = state.decode(AccessInput::Source(src), &mut run).expect("decode");
        let AccessOutcome::FullDocument(product) = result.into_parts().0 else {
            panic!("expected a full document")
        };
        let document = product.document();
        let segment = document.source_segment().expect("source segment");
        for node in [
            document.root(),
            document
                .payload_view(document.root_handle())
                .expect("payload view")
                .node(),
        ] {
            let span = document
                .node_source_span(node)
                .expect("span")
                .expect("tagged item must carry a span");
            assert_eq!(
                &segment[span.start() as usize..span.end() as usize],
                tagged,
                "the tagged item's span covers its tag head through the value"
            );
        }
    }

    #[test]
    fn non_text_key_is_unrepresentable_for_the_whole_route() {
        // {1: "a"}.
        let error = decode(&[0xa1, 0x01, 0x61, 0x61]).expect_err("non-text key fails");
        assert!(matches!(error.kind(), CodecFailureKind::UnsupportedRepresentation));
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let error = decode(&[0x81, 0x01, 0x00]).expect_err("trailing bytes fail");
        assert!(matches!(error.kind(), CodecFailureKind::InvalidInput));
    }

    /// Drives ONE top-level item in adjacent mode, returning the materialized value and the item's consumed offset.
    fn decode_adjacent(bytes: &[u8]) -> Result<(jqf_data::Value, u64), CodecError> {
        let mut ctx = resources();
        let mut state =
            CborParseState::try_new(source(bytes), BuilderCoverage::minimal_semantic(), None, true, &mut ctx)
                .expect("session starts");
        let mut run = CodecRunContext::new(&mut ctx);
        let result = state.decode(AccessInput::Source(source(bytes)), &mut run)?;
        let consumed = result.report().consumed_offset();
        let AccessOutcome::FullDocument(product) = result.into_parts().0 else {
            panic!("expected a full document")
        };
        let value = product.document().materialize_root(&mut ctx).expect("materialize");
        Ok((value, consumed.expect("adjacent decode reports a consumed offset")))
    }

    /// Adjacent-mode receipt: N adjacent items decoded from ONE buffer, each item's end offset asserted EXACTLY — the
    /// cut is the decoder's own cursor, never a byte more or less.
    #[test]
    fn adjacent_items_report_exact_consumed_offsets() {
        // `[1, 2]` (3 bytes) then `true` (1 byte) then `"x"` (2 bytes).
        let buffer: &[u8] = &[0x82, 0x01, 0x02, 0xf5, 0x61, 0x78];
        let (value, consumed) = decode_adjacent(buffer).expect("item 1 decodes");
        assert_eq!(consumed, 3, "item 1 ends exactly at offset 3");
        assert_eq!(render(&value), "[1, 2]");
        let (value, consumed) = decode_adjacent(&buffer[3..]).expect("item 2 decodes");
        assert_eq!(consumed, 1, "item 2 ends exactly at offset 1");
        assert_eq!(render(&value), "true");
        let (value, consumed) = decode_adjacent(&buffer[4..]).expect("item 3 decodes");
        assert_eq!(consumed, 2, "item 3 ends exactly at offset 2");
        assert_eq!(render(&value), "\"x\"");
        // The whole buffer under adjacent mode decodes only item 1: the cut never consumes past the item.
        assert_eq!(
            decode_adjacent(buffer).expect("first item").1,
            3,
            "the cut stops at the first item's end"
        );
    }

    /// An item followed by GARBAGE fails at the garbage, never at the good item: the first item decodes with its exact
    /// end offset, and the garbage fails as its own decode.
    #[test]
    fn adjacent_item_followed_by_garbage_fails_at_the_garbage() {
        // `1` (0x01) followed by `0xff` (reserved simple value / invalid).
        let buffer: &[u8] = &[0x01, 0xff];
        let (value, consumed) = decode_adjacent(buffer).expect("the good item decodes");
        assert_eq!(consumed, 1);
        assert_eq!(render(&value), "1");
        let error = decode_adjacent(&buffer[1..]).expect_err("the garbage fails");
        assert!(matches!(error.kind(), CodecFailureKind::InvalidInput));
    }

    /// A truncated TAIL item fails cleanly: the hold/refill law's prerequisite is that the truncation error is a decode
    /// failure whose label starts exactly at the window end, never a wrong value.
    #[test]
    fn adjacent_truncated_tail_item_fails() {
        // `0x18` needs one argument byte; only the head is present.
        let buffer: &[u8] = &[0x01, 0x18];
        let (_, consumed) = decode_adjacent(buffer).expect("the good item decodes");
        assert_eq!(consumed, 1);
        let error = decode_adjacent(&buffer[1..]).expect_err("the truncated tail fails");
        assert!(matches!(error.kind(), CodecFailureKind::InvalidInput));
        let diagnostic = error.diagnostic().expect("a structured truncation");
        let label = diagnostic.labels().first().expect("a primary label");
        assert_eq!(
            usize::try_from(label.span().start()).unwrap(),
            buffer.len() - 1,
            "the truncation label starts at the window end"
        );
    }

    /// A wrong-major chunk inside an indefinite string is a well-formedness violation at the CHUNK head (RFC 8949
    /// §3.2.3), never an EOF truncation: the rejection cites the chunk's own offset.
    #[test]
    fn wrong_major_chunk_is_rejected_at_its_own_offset() {
        // (_ "ab") with a BYTES chunk where TEXT is owed.
        let bytes: &[u8] = &[0x7f, 0x42, 0x61, 0x62, 0xff];
        let error = decode(bytes).expect_err("the chunk type mismatch fails");
        assert!(matches!(error.kind(), CodecFailureKind::InvalidInput));
        let diagnostic = error.diagnostic().expect("a structured reject");
        let label = diagnostic.labels().first().expect("a primary label");
        assert_eq!(
            usize::try_from(label.span().start()).unwrap(),
            1,
            "the chunk head is the cited offset"
        );
    }

    /// Source retention and the adjacent opt-in combine independently: a session opened for adjacent values with a
    /// frontier hint decodes ONE span-backed container item and reports its end as the consumed offset, instead of
    /// rejecting the remainder as trailing content.
    #[test]
    fn source_retention_and_adjacent_combine() {
        // `[1]` then `true`.
        let bytes: &[u8] = &[0x81, 0x01, 0xf5];
        let mut ctx = resources();
        let mut state = CborParseState::try_new(
            source(bytes),
            BuilderCoverage::minimal_semantic(),
            Some(usize::MAX),
            true,
            &mut ctx,
        )
        .expect("session starts");
        let mut run = CodecRunContext::new(&mut ctx);
        let result = state
            .decode(AccessInput::Source(source(bytes)), &mut run)
            .expect("item 1 decodes under retention + adjacent");
        assert_eq!(
            result.report().consumed_offset(),
            Some(2),
            "the consumed offset is the item's end, not the whole input"
        );
        let (outcome, _) = result.into_parts();
        assert!(matches!(outcome, AccessOutcome::FullDocument(_)));
    }

    /// A recognized tag (id 0-5) whose tagged item is an indefinite container's BREAK was left in `pending_tags`; the
    /// BREAK closed the container and `deliver` popped the tag — the `debug_assert!(tag > 5)` invariant panicked in
    /// the fuzz build, and a plain release build ACCEPTED the malformed document as a `cbor:tag:N`-wrapped empty
    /// container. A tag must receive its content item; a BREAK is not a data item, so `9f c1 ff` is a malformed
    /// document that must be REJECTED — never a panic, never a value — in debug and release semantics alike.
    #[test]
    fn tag_at_an_indefinite_break_is_rejected_not_wrapped() {
        // `corpus/crash-cbor-tag-at-break/indef-array-tag1-break.cbor`: indefinite array, tag 1, BREAK.
        for bytes in [
            &[0x9f, 0xc1, 0xff][..],
            // The 4-byte form with a trailing scalar after the BREAK.
            &[0x9f, 0xc1, 0xff, 0x0c][..],
            // Tag 5 (every recognized id 0-5 triggers the invariant).
            &[0x9f, 0xc5, 0xff][..],
            // An UNINTERPRETED tag at the BREAK is equally malformed: it has no content item either.
            &[0x9f, 0xd9, 0x00, 0x06, 0xff][..],
        ] {
            let error = decode(bytes).expect_err("tag at a break must fail");
            assert!(
                matches!(error.kind(), CodecFailureKind::InvalidInput),
                "expected InvalidInput for {bytes:?}, got {:?}",
                error.kind()
            );
        }
    }

    #[test]
    fn invalid_text_utf8_is_rejected() {
        // Text string with an invalid UTF-8 byte 0xff.
        let error = decode(&[0x61, 0xff]).expect_err("invalid utf-8 fails");
        assert!(matches!(error.kind(), CodecFailureKind::InvalidInput));
    }

    /// Drives the whole-route session with a lazy frontier armed and returns the published result and the session, so a
    /// test can read both `progress()` and the span-backed product.
    fn decode_lazy(bytes: &[u8]) -> AccessResult<'_> {
        let mut ctx = resources();
        let mut state = CborParseState::try_new(
            source(bytes),
            BuilderCoverage::minimal_semantic(),
            Some(1),
            false,
            &mut ctx,
        )
        .expect("session starts");
        let mut run = CodecRunContext::new(&mut ctx);
        state
            .decode(AccessInput::Source(source(bytes)), &mut run)
            .expect("decode")
    }

    #[test]
    fn lazy_frontier_publishes_span_root_and_reports_consumed_offset() {
        // The whole-document route with `lazy_frontier` armed defers the CONTAINER root to one source span, and the
        // session must report the consumed offset so the adjacent-value drive (`-s` and the sequence lanes) sees
        // forward progress. A scalar root falls through to the ordinary eager decode and reports the same consumed
        // offset.
        for (bytes, expected_render) in [
            (&[0x82, 0x01, 0x02][..], "[1, 2]"),
            (&[0xa1, 0x61, 0x61, 0x01][..], "{\"a\": 1}"),
            (&[0x01][..], "1"),
        ] {
            let result = decode_lazy(bytes);
            let AccessOutcome::FullDocument(product) = result.into_parts().0 else {
                panic!("expected a full document")
            };
            let document = product.document();
            if bytes[0] & 0xe0 == 0x80 || bytes[0] & 0xe0 == 0xa0 {
                // A container root is deferred to one span the engine's `materialize_if_span` can materialize on touch.
                assert_eq!(document.container_span_count(), 1);
                let root = document.root_handle();
                assert!(
                    document
                        .value_view(root)
                        .expect("root view")
                        .is_container_span()
                        .expect("span check")
                );
            }
            let value = document.materialize_root(&mut resources()).expect("materialize");
            assert_eq!(render(&value), expected_render);
        }
    }

    /// The one-shot pre-scan sums the EXACT node/occurrence totals the decoder will build. A definite array of `count`
    /// elements is `count` nodes + `count` occurrences; a map of `count` pairs is `count` value nodes + `count` member
    /// occurrences. Nested containers recurse; tags are payload-transparent.
    #[test]
    fn count_nodes_sums_definite_lengths() {
        // [1, 2, 3]
        let bytes = [0x83, 0x01, 0x02, 0x03];
        let mut nodes = 1;
        let mut occurrences = 0;
        count_nodes(&bytes, 0, &mut nodes, &mut occurrences);
        assert_eq!((nodes, occurrences), (4, 3));
        // {"a": 1, "b": [true, null]}: root map(2) = 2 value nodes + 2 member occurrences; the array value adds 2 nodes
        // + 2 occurrences.
        let bytes = [0xa2, 0x61, 0x61, 0x01, 0x61, 0x62, 0x82, 0xf5, 0xf6];
        let mut nodes = 1;
        let mut occurrences = 0;
        count_nodes(&bytes, 0, &mut nodes, &mut occurrences);
        assert_eq!((nodes, occurrences), (5, 4));
        // An indefinite container is not priced: the scan stops and the builder falls back to amortized growth.
        let bytes = [0x9f, 0x01, 0xff];
        let mut nodes = 1;
        let mut occurrences = 0;
        count_nodes(&bytes, 0, &mut nodes, &mut occurrences);
        assert_eq!((nodes, occurrences), (1, 0));
    }

    /// A container head declaring a huge count past the end of the input must not make the scan iterate the declared
    /// count — the decoder fails on Eof, and the scan must terminate on the same bound. The declared count is priced
    /// only up to the remaining bytes, so a huge head can never inflate `nodes` into a multi-GB `try_reserve`.
    #[test]
    fn count_nodes_terminates_on_truncated_huge_count() {
        // An array head declaring 2^64-1 elements with one byte of content.
        let bytes = [0x9b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01];
        let mut nodes = 1;
        let mut occurrences = 0;
        count_nodes(&bytes, 0, &mut nodes, &mut occurrences);
        // The scan prices the declared count only up to the remaining bytes (the one byte of content is one element);
        // the builder's reserve is a hint that rolls back, and the real decode reports the truncation. The bound is
        // what keeps a 5-byte input from reserving 43 GB.
        assert_eq!((nodes, occurrences), (2, 1));
    }

    /// `9a 40 27 c9 c9` is an ARRAY head declaring 0x4027c9c9 = 1,075,929,545 elements with zero bytes of content. The
    /// bound must gate what reaches `nodes`/`occurrences`, not just the owed level, or the reservation alone is a
    /// multi-GB allocation.
    #[test]
    fn count_nodes_never_reserves_past_the_remaining_bytes() {
        // An array declaring ~1.07 billion elements, no content bytes.
        let bytes = [0x9a, 0x40, 0x27, 0xc9, 0xc9];
        let mut nodes = 1;
        let mut occurrences = 0;
        count_nodes(&bytes, 0, &mut nodes, &mut occurrences);
        // Root plus nothing: the remaining bytes price no element.
        assert_eq!((nodes, occurrences), (1, 0));
        // A map declaring a huge pair count with a couple of content bytes prices only those bytes (a pair needs a key
        // AND a value, so the owed items double, but the reservation stays bounded).
        let bytes = [0xba, 0x83, 0x30, 0x8b, 0x01, 0x02, 0x01];
        let mut nodes = 1;
        let mut occurrences = 0;
        count_nodes(&bytes, 0, &mut nodes, &mut occurrences);
        assert_eq!((nodes, occurrences), (3, 2));
    }

    /// The capacity pre-scan runs BEFORE the guarded walk: one head per byte of `0xc6` (tag) or `0x81` (array-of-1) was
    /// one native frame per byte, and a few hundred KB of either overflowed the stack — an abort, not a typed
    /// failure. A depth guard alone did NOT fix it (`10_000` governed levels are already more native frames than the
    /// test thread has), so the scan walks an explicit stack and stops at [`MAX_SCAN_DEPTH`].
    #[test]
    fn a_tag_or_array_chain_does_not_overflow_the_capacity_prescan() {
        for filler in [0xc6_u8, 0x81] {
            let bytes = vec![filler; 300_000];
            let mut nodes = 1;
            let mut occurrences = 0;
            count_nodes(&bytes, 0, &mut nodes, &mut occurrences);
            // And the whole session over the same payload fails cleanly: the pre-scan only ever costs the capacity
            // hint.
            let mut ctx = governed_resources();
            let mut state = CborParseState::try_new(
                source(&bytes),
                BuilderCoverage::minimal_semantic(),
                None,
                false,
                &mut ctx,
            )
            .expect("session starts");
            let mut run = CodecRunContext::new(&mut ctx);
            assert!(
                state.decode(AccessInput::Source(source(&bytes)), &mut run).is_err(),
                "an unterminated chain is a decode failure"
            );
        }
    }

    /// A CBOR map key may be ANY value, so a container-valued key carries the document's depth into the key-identity
    /// parser. That parser recursed once per open level, so the key of `{[[[…1…]]]: 1}` — `0xa1`, then 10 000
    /// heads of `0x81`, then `0x01` twice — ended the process with a stack overflow instead of an answer, on a trust
    /// boundary the governor had already ADMITTED. The parser builds on an explicit frame stack now, and the nesting
    /// guard keeps its own job, which the second half asserts: one level past the ceiling is a typed refusal, still not
    /// an abort.
    ///
    /// The depth is the governed ceiling itself, so this one test is the receipt for the whole class: the parse, the
    /// ledger's `fingerprint` walk, the exact `equivalent` comparison, and the tree's `Drop` all run to completion over
    /// a key nested 9 999 deep.
    #[test]
    fn a_deep_container_valued_map_key_does_not_overflow_the_parse() {
        let resources = governed_resources();
        // The key alone: 9 999 nested arrays around `1`, one level inside the 10 000 the CLI's ceiling admits.
        let mut bytes = vec![0x81_u8; 9_999];
        bytes.push(0x01);
        let (key, end) = parse_key_value(source(&bytes), 0, &resources).expect("a legal deep key parses");
        assert_eq!(end, bytes.len());
        assert!(matches!(key, KeyValue::Array(_)));
        // The ledger's whole path over the same key: fingerprint, retain, then the exact comparison a duplicate forces,
        // then the drop of both.
        let (duplicate, _) = parse_key_value(source(&bytes), 0, &resources).expect("parses again");
        let mut keys = KeySet::default();
        assert!(keys.try_insert(&bytes, key));
        assert!(!keys.try_insert(&bytes, duplicate), "a deep duplicate is a duplicate");

        // One level past the ceiling is a typed refusal, still not an abort: the guard keeps its own job.
        let mut bytes = vec![0x81_u8; 10_000];
        bytes.push(0x01);
        assert!(
            parse_key_value(source(&bytes), 0, &resources).is_err(),
            "the nesting ceiling still refuses the level past it"
        );
    }

    /// What the ledger receives must be byte-for-byte the identity the recursive parser built: the same items in the
    /// same order, keys paired with their own values, definite and indefinite spellings meeting, and the recognized-tag
    /// folds applied to the completed content.
    #[test]
    fn a_container_valued_key_parses_to_the_ledger_identity() {
        let resources = governed_resources();
        // [1, {"a": 2}] definite, then the same value spelled indefinite.
        let definite: &[u8] = &[0x82, 0x01, 0xa1, 0x61, 0x61, 0x02];
        let indefinite: &[u8] = &[0x9f, 0x01, 0xbf, 0x61, 0x61, 0x02, 0xff, 0xff];
        let (left, end) = parse_key_value(source(definite), 0, &resources).expect("definite key parses");
        assert_eq!(end, definite.len());
        let (right, end) = parse_key_value(source(indefinite), 0, &resources).expect("indefinite key parses");
        assert_eq!(end, indefinite.len());
        assert!(left.equivalent(&right), "§5.6.1 identity ignores the length spelling");
        assert_eq!(left.fingerprint(), right.fingerprint());
        // Order and pairing, not just equivalence: item 1 is the map, whose one pair is ("a", 2) and not its transpose.
        let KeyValue::Array(items) = &left else {
            panic!("expected an array key, got {left:?}")
        };
        assert_eq!(items.len(), 2);
        let KeyValue::Map(pairs) = &items[1] else {
            panic!("expected a map member, got {:?}", items[1])
        };
        assert!(matches!(&pairs[0].0, KeyValue::Text(text) if text.as_slice() == b"a"));
        assert!(matches!(&pairs[0].1, KeyValue::BasicInt { negative: false, .. }));
        // The ledger's duplicate law over container keys.
        let mut keys = KeySet::default();
        assert!(keys.try_insert(definite, left));
        assert!(
            !keys.try_insert(indefinite, right),
            "the same container key is a duplicate"
        );

        // Tag folds apply to the COMPLETED content: 4([-2, 27]) is a decimal, and an uninterpreted chain nests as it
        // was written.
        let (decimal, _) =
            parse_key_value(source(&[0xc4, 0x82, 0x21, 0x18, 0x1b]), 0, &resources).expect("tag 4 key parses");
        assert!(matches!(decimal, KeyValue::Decimal { .. }));
        let (chain, _) = parse_key_value(source(&[0xc6, 0xc6, 0x01]), 0, &resources).expect("tag chain key parses");
        let KeyValue::Tagged { number, content } = &chain else {
            panic!("expected a tagged key")
        };
        assert_eq!(*number, 6);
        assert!(matches!(**content, KeyValue::Tagged { number: 6, .. }));

        // A break byte where a map VALUE is owed stays a reserved-argument failure; it does not close the pair.
        let error = parse_key_value(source(&[0xbf, 0x61, 0x61, 0xff]), 0, &resources)
            .expect_err("a break in a value position fails");
        assert!(matches!(error.kind(), CodecFailureKind::InvalidInput));
    }

    /// A tag-2/3 bignum renders decimal in time quadratic in its byte length, and that length is an untrusted prefix:
    /// the decoder caps it instead of admitting an unbounded conversion inside one poll.
    #[test]
    fn an_oversize_bignum_payload_is_refused() {
        // 0xc2 (tag 2) + a byte-string head declaring MAX_BIGNUM_BYTES + 1.
        let length = MAX_BIGNUM_BYTES + 1;
        let mut bytes = alloc::vec![0xc2, 0x59];
        bytes.extend_from_slice(&u16::try_from(length).expect("fits").to_be_bytes());
        bytes.extend(core::iter::repeat_n(0xff_u8, length));
        let error = decode(&bytes).expect_err("an oversize bignum is refused");
        assert!(matches!(error.kind(), CodecFailureKind::UnsupportedRepresentation));
        // One byte under the ceiling still decodes to its exact decimal.
        let mut bytes = alloc::vec![0xc2, 0x59];
        bytes.extend_from_slice(&u16::try_from(MAX_BIGNUM_BYTES).expect("fits").to_be_bytes());
        bytes.push(0x01);
        bytes.extend(core::iter::repeat_n(0x00_u8, MAX_BIGNUM_BYTES - 1));
        decode(&bytes).expect("a bignum at the ceiling decodes");
    }

    #[test]
    fn whole_prune_omits_unobservable_members() {
        let resources = resources();
        let requirement = AccessRequirement::try_whole(
            CodecDemand::try_new(&resources),
            AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            &resources,
        )
        .expect("requirement")
        .with_prune(keep_member_tree("a", &resources));
        // {"a": 1, "b": [true, null]}
        let (value, nodes) =
            decode_requirement(&[0xa2, 0x61, 0x61, 0x01, 0x61, 0x62, 0x82, 0xf5, 0xf6], &requirement).expect("decode");
        assert_eq!(object_keys(&value), ["a"]);
        assert_eq!(nodes, 2, "object + kept scalar; omitted array is not built");
        decode_requirement(&[0xa2, 0x61, 0x61, 0x01, 0x61, 0x62], &requirement)
            .expect_err("omitted members still validate");
    }

    #[test]
    fn count_only_open_ignores_the_prune_hint() {
        let resources = resources();
        let requirement = AccessRequirement::try_whole(
            CodecDemand::try_new(&resources),
            AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            &resources,
        )
        .expect("requirement")
        .with_count(CountDemand {
            row: CountRow::Container,
            path: Vec::new(),
            range: None,
            probe: Vec::new(),
            filter: None,
        })
        .with_prune(keep_member_tree("a", &resources));
        let (value, _) =
            decode_requirement(&[0xa2, 0x61, 0x61, 0x01, 0x61, 0x62, 0x82, 0xf5, 0xf6], &requirement).expect("decode");
        assert_eq!(
            object_keys(&value),
            ["a", "b"],
            "count-only ignores prune and delivers every member"
        );
    }

    #[test]
    fn lazy_frontier_open_ignores_the_prune_hint() {
        let resources = resources();
        let requirement = AccessRequirement::try_whole(
            CodecDemand::try_new(&resources),
            AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            &resources,
        )
        .expect("requirement")
        .with_lazy_frontier(1)
        .with_prune(keep_member_tree("a", &resources));
        let (value, _) =
            decode_requirement(&[0xa2, 0x61, 0x61, 0x01, 0x61, 0x62, 0x82, 0xf5, 0xf6], &requirement).expect("decode");
        assert_eq!(
            object_keys(&value),
            ["a", "b"],
            "a lazy open ignores prune and delivers every member"
        );
    }

    #[test]
    fn reopen_declines_a_count_only_profile_mismatch() {
        let bytes: &[u8] = &[0xa2, 0x61, 0x61, 0x82, 0xf5, 0xf6, 0x61, 0x62, 0x82, 0xf5, 0xf6];
        let mut ctx = resources();
        let registration = crate::registration().expect("registration");
        let mut provider = registration
            .decoder()
            .expect("decoder")
            .create_provider(source(bytes), decode_request(), &mut ctx)
            .expect("provider");
        let identity = AccessRequirement::try_whole(
            CodecDemand::try_new(&ctx),
            AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            &ctx,
        )
        .expect("identity");
        let count = AccessRequirement::try_whole(
            CodecDemand::try_new(&ctx),
            AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            &ctx,
        )
        .expect("count")
        .with_count(CountDemand {
            row: CountRow::Container,
            path: Vec::new(),
            range: None,
            probe: Vec::new(),
            filter: None,
        });
        let mut reuse = jqf_codec_core::ReusableAccessSession::new();
        let identity_handle = provider.bind(&identity).expect("bind identity");
        {
            let session = provider
                .open_at_reusing(&identity_handle, 0, &mut reuse, &mut ctx)
                .expect("open identity");
            let mut run = CodecRunContext::new(&mut ctx);
            run.set_cooperative_credits(4_096);
            let result = session.decode(&mut run).expect("decode identity");
            let AccessOutcome::FullDocument(product) = result.outcome() else {
                panic!("expected a full document")
            };
            assert!(
                product.document().node_count() > 3,
                "eager identity builds omitted member payloads"
            );
        }
        let count_handle = provider.bind(&count).expect("bind count");
        let session = provider
            .open_at_reusing(&count_handle, 0, &mut reuse, &mut ctx)
            .expect("reopen");
        let mut run = CodecRunContext::new(&mut ctx);
        run.set_cooperative_credits(4_096);
        let result = session.decode(&mut run).expect("decode count");
        let AccessOutcome::FullDocument(product) = result.outcome() else {
            panic!("expected a full document")
        };
        assert_eq!(
            product.document().node_count(),
            3,
            "a declined recycle must open a fresh count-only session"
        );
    }

    #[test]
    fn owned_filter_equality_declines_a_live_tagged_member() {
        // [{"x": 6(1)}]
        let value = decode(&[0x81, 0xa1, 0x61, 0x78, 0xc6, 0x01]).expect("decode");
        let jqf_data::Value::Array(items) = &value else {
            panic!("expected array")
        };
        let item = items.iter().next().expect("one element");
        let jqf_data::Value::Object(object) = item else {
            panic!("expected object")
        };
        let x = object
            .iter()
            .find(|entry| entry.key() == "x")
            .map(jqf_data::ObjectEntry::value)
            .expect("x");
        assert!(matches!(x, jqf_data::Value::Tagged { .. }), "tag 6 stays tagged");
        let filter = jqf_data::CountFilter {
            path: vec![jqf_data::CountStep::ObjectKey(alloc::string::String::from("x"))],
            test: jqf_data::CountTest::Compare {
                op: jqf_data::CountCompare::Equal,
                rhs: jqf_data::CountLiteral::Decimal {
                    negative: false,
                    digits: alloc::string::String::from("1"),
                    scale: 0,
                },
            },
        };
        assert_eq!(
            filter.contributes(item),
            None,
            "tagged equality must decline to the floor"
        );
    }

    #[test]
    fn exact_prune_omits_unread_members_of_the_located_object() {
        let resources = resources();
        let mut path = ExactPath::try_new(&resources);
        path.try_push_semantic_member("catalog", &resources).expect("member");
        let requirement = AccessRequirement::try_exact(
            AccessFootprint::try_exact(path, &resources),
            CodecDemand::try_new(&resources),
            AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            &resources,
        )
        .expect("requirement")
        .with_prune(keep_member_tree("id", &resources));
        // {"catalog": {"id": 1, "name": "x"}}
        let bytes: &[u8] = &[
            0xa1, 0x67, b'c', b'a', b't', b'a', b'l', b'o', b'g', 0xa2, 0x62, b'i', b'd', 0x01, 0x64, b'n', b'a', b'm',
            b'e', 0x61, b'x',
        ];
        let (value, nodes) = decode_requirement(bytes, &requirement).expect("decode");
        assert_eq!(object_keys(&value), ["id"]);
        assert_eq!(nodes, 2, "located object + kept scalar; omitted siblings are not built");
        let truncated: &[u8] = &[
            0xa1, 0x67, b'c', b'a', b't', b'a', b'l', b'o', b'g', 0xa2, 0x62, b'i', b'd', 0x01, 0x64, b'n', b'a', b'm',
            b'e',
        ];
        decode_requirement(truncated, &requirement).expect_err("omitted members still validate");
    }

    /// Exact + count publishes the walk's container span. The locate pass already
    /// proved the range and counted its children; count must not re-decode the hit.
    #[test]
    fn exact_count_demand_uses_the_locate_span() {
        // {"users":[1,2,3]}
        let bytes: &[u8] = &[0xa1, 0x65, b'u', b's', b'e', b'r', b's', 0x83, 0x01, 0x02, 0x03];
        let result = decode_exact_count(bytes, "users").expect("count demand");
        let AccessOutcome::Located(located) = result.outcome() else {
            panic!("located")
        };
        let ExactSelectionRecord::Node { node, .. } = located.result() else {
            panic!("node")
        };
        let document = located.product().document();
        assert!(
            document
                .value_view(*node)
                .expect("view")
                .is_container_span()
                .expect("span"),
            "count Exact must publish the locate span, not a rematerialized tree"
        );
        assert_eq!(document.container_span_count(), 1);
        assert_eq!(document.node_count(), 1, "skeleton Exact is one container-span node");
        assert_eq!(
            document.container_span_child_count(*node).expect("handle"),
            Some(3),
            "Exact count records cardinality on the proving walk"
        );
        let mut ctx = resources();
        assert_eq!(
            document
                .count_children_from(*node, &empty_path_count(), &mut ctx)
                .expect("span count"),
            CountVerdict::Count(3)
        );
    }

    #[test]
    fn exact_count_demand_still_validates_unread_bytes() {
        // {"users":[1,2,3]} plus a trailing 0xff that Whole would refuse.
        let bytes: &[u8] = &[0xa1, 0x65, b'u', b's', b'e', b'r', b's', 0x83, 0x01, 0x02, 0x03, 0xff];
        decode_exact_count(bytes, "users").expect_err("unread trailing byte must still fail Exact validation");
    }

    fn decode_exact_count<'source>(bytes: &'source [u8], member: &str) -> Result<AccessResult<'source>, CodecError> {
        let mut ctx = resources();
        let mut path = ExactPath::try_new(&ctx);
        path.try_push_semantic_member(member, &ctx).expect("member");
        let requirement = AccessRequirement::try_exact(
            AccessFootprint::try_exact(path, &ctx),
            CodecDemand::try_new(&ctx),
            AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            &ctx,
        )
        .expect("requirement")
        .with_count(empty_path_count());
        let registration = crate::registration().expect("registration");
        let mut provider = registration
            .decoder()
            .expect("decoder")
            .create_provider(source(bytes), decode_request(), &mut ctx)
            .expect("provider");
        let handle = provider.bind(&requirement).expect("bind");
        let mut session = provider.open(&handle, &mut ctx).expect("open");
        let mut run = CodecRunContext::new(&mut ctx);
        run.set_cooperative_credits(4_096);
        session.decode(&mut run)
    }

    fn empty_path_count() -> jqf_data::CountDemand {
        jqf_data::CountDemand {
            row: jqf_data::CountRow::Container,
            path: alloc::vec::Vec::new(),
            range: None,
            probe: alloc::vec::Vec::new(),
            filter: None,
        }
    }

    fn decode_count(bytes: &[u8]) -> Result<(jqf_data::CountVerdict, usize, u32), CodecError> {
        let demand = empty_path_count();
        let mut ctx = resources();
        let requirement = AccessRequirement::try_whole(
            CodecDemand::try_new(&ctx),
            AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            &ctx,
        )
        .expect("requirement")
        .with_count(demand.clone());
        let registration = crate::registration().expect("registration");
        let mut provider = registration
            .decoder()
            .expect("decoder")
            .create_provider(source(bytes), decode_request(), &mut ctx)
            .expect("provider");
        let handle = provider.bind(&requirement).expect("bind");
        let mut session = provider.open(&handle, &mut ctx).expect("open");
        let mut run = CodecRunContext::new(&mut ctx);
        run.set_cooperative_credits(4_096);
        let result = session.decode(&mut run)?;
        let AccessOutcome::FullDocument(product) = result.into_parts().0 else {
            panic!("expected a full document")
        };
        let document = product.document();
        let nodes = document.node_count();
        let spans = document.container_span_count();
        let verdict = document
            .count_children_from(document.root_handle(), &demand, &mut ctx)
            .expect("count");
        Ok((verdict, nodes, spans))
    }

    #[test]
    fn empty_path_length_on_a_map_does_not_retain_omitted_member_payloads() {
        // {"a": [true, null], "b": [true, null]}
        let bytes: &[u8] = &[0xa2, 0x61, 0x61, 0x82, 0xf5, 0xf6, 0x61, 0x62, 0x82, 0xf5, 0xf6];
        let (verdict, nodes, spans) = decode_count(bytes).expect("decode");
        assert_eq!(verdict, jqf_data::CountVerdict::Count(2));
        assert_eq!(spans, 2, "each map value is a deferred array span");
        assert_eq!(
            nodes, 3,
            "object + two child spans; omitted array payloads are not built"
        );
        decode_count(&[0xa2, 0x61, 0x61, 0x82, 0xf5, 0xf6, 0x61, 0x62, 0x82, 0xf5])
            .expect_err("truncated input still fails");
    }

    #[test]
    fn empty_path_length_on_an_array_does_not_retain_omitted_member_payloads() {
        // [[true, null], [true, null]]
        let bytes: &[u8] = &[0x82, 0x82, 0xf5, 0xf6, 0x82, 0xf5, 0xf6];
        let (verdict, nodes, spans) = decode_count(bytes).expect("decode");
        assert_eq!(verdict, jqf_data::CountVerdict::Count(2));
        assert_eq!(spans, 2, "each element is a deferred array span");
        assert_eq!(
            nodes, 3,
            "root array + two child spans; omitted element payloads are not built"
        );
        decode_count(&[0x82, 0x82, 0xf5, 0xf6, 0x82, 0xf5]).expect_err("truncated input still fails");
    }
}
