//! Single-document CSV payload decoding over a narrowed record range.
//!
//! The record drive (`jqf-sdk::execute_record_sequence`) opens ONE provider over the WHOLE retained source, then
//! decodes each record's byte range through it via `open_range_reusing`. For CSV, that provider is this one: a
//! whole-document `InputProvider` whose single route, over a narrowed record range, builds the document for that ONE
//! record.
//!
//! ## The two sealed row shapes
//!
//! Under `csv.rfc4180@1` — the DEFAULT — every record publishes as an ARRAY of its field strings. There is no
//! implicit header consumption: the first record (which a user may treat as a header) is just the first array, and no
//! record is ever silently dropped.
//!
//! Under the opt-in `csv.rfc4180-header@1` the FIRST record is a header: the framer consumes it and every later record
//! publishes as an OBJECT keyed by the header names, in header order. The provider reads those names from the START of
//! the same retained source ([`crate::header`]), so no state crosses the record ABI and every session — whole and
//! scoped — sees the identical names from its first poll.
//!
//! Field values are never type-inferred under either dialect: every field is a core `String`, and `tonumber` stays
//! explicit.
//!
//! ## Ragged rows are an error
//!
//! A data record whose field count differs from the header's is `InvalidInput`, which the strict record stream makes
//! terminal. A dialect that silently padded or truncated would make "does this table have three columns?" unanswerable
//! from the output.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use jqf_codec_core::{
    AccessGuarantees, AccessInput, AccessOutcome, AccessResult, AccessResultKind, AccessSession, CodecError,
    CodecFailureKind, CodecRunContext, DocumentProduct, ErasedAccessSession, PhysicalRouteId, ProviderInput,
    RouteDescription, RouteSlot,
};
use jqf_data::{
    AccountedDocumentBuilder, AuthoritativeEmptyFamilies, BuilderCoverage, DataError, DiagnosticCoverage,
    DocumentCapabilityFamily, DocumentSchemaPrototype, DocumentSchemaRecipe, NodeId, PreparedDocumentSchema,
    PreparedNodeKind, PreparedOccurrenceRole, PreparedSemanticNode,
};
use jqf_resource::ResourceContext;
use jqf_source::ResolvedSource;

use crate::CsvDecodeOptions;

/// Provider-local slot of the whole-document decode route.
const DECODE_ROUTE_SLOT: RouteSlot = RouteSlot::new(0);

/// Provider-local slot of the scoped exact-path decode route.
const SCOPED_ROUTE_SLOT: RouteSlot = RouteSlot::new(1);

/// Stable physical identity of scoped single-column CSV decoding.
pub const SCOPED_PHYSICAL_ROUTE_ID: PhysicalRouteId = match PhysicalRouteId::derive(crate::FORMAT_ID, 2, 1) {
    Some(id) => id,
    None => panic!("nonzero route identity"),
};

/// Stable physical identity of single-record CSV payload decoding.
pub(crate) const DECODE_PHYSICAL_ROUTE_ID: PhysicalRouteId = match PhysicalRouteId::derive(crate::FORMAT_ID, 1, 1) {
    Some(id) => id,
    None => panic!("nonzero route identity"),
};

/// The one-time-per-request schema prototype for a row shape.
///
/// The record drive opens ONE payload provider per request (or per morsel) and decodes each record's byte range through
/// it. The recipe→prepare→bind chain (`AccountedDocumentBuilder::try_new_prepared_with_coverage`) runs ~14 identity
/// validations and ~14 tracked allocations per call; json amortizes it by building one [`DocumentSchemaPrototype`] per
/// request and cloning its schema `Arc` per document. This is the same mechanism: the provider builds the prototype
/// once, and every per-record builder starts from the cheap `try_clone` + `try_prepare_document` path instead of
/// re-binding the recipe.
///
/// The recipe carries the DIALECT the row shape belongs to, so a headered document can never claim the array dialect's
/// identity — under either grammar and either CSV alphabet family (the TSV dialects name the same row shapes; the
/// row-shape vocabulary is shared).
pub(crate) fn csv_schema_recipe(options: &CsvDecodeOptions) -> Result<DocumentSchemaRecipe<'static>, DataError> {
    DocumentSchemaRecipe::try_new(
        options.format_id(),
        Some(options.dialect_id()),
        &["csv.array@1", "csv.object@1", "csv.scalar@1"],
        &["csv.item@1", "csv.field@1"],
        &[],
        &[],
    )
}

/// Builds the per-request immutable schema prototype for this provider's dialect, caching it on first use (json's
/// `try_schema_prototype` shape).
fn try_schema_prototype(
    prototype: &mut Option<DocumentSchemaPrototype>,
    options: CsvDecodeOptions,
    _resources: &ResourceContext<'_>,
) -> Result<DocumentSchemaPrototype, CodecError> {
    if prototype.is_none() {
        let recipe = csv_schema_recipe(&options).map_err(map_data)?;
        *prototype = Some(DocumentSchemaPrototype::try_new(&recipe).map_err(map_data)?);
    }
    prototype.as_ref().ok_or_else(data_contract).cloned()
}

/// The prepared node-kind and occurrence-role handles of one row document.
///
/// Resolved once per record from the prepared schema; every node and occurrence in the record builds through them (the
/// prepared path json uses), never through string lookups against a dynamic schema.
#[derive(Clone, Copy)]
pub(crate) struct CsvHandles {
    /// `csv.array@1` — the array-dialect root.
    pub(crate) array: PreparedNodeKind,
    /// `csv.object@1` — the headered-dialect root.
    pub(crate) object: PreparedNodeKind,
    /// `csv.scalar@1` — every field value (and the null stand-in).
    pub(crate) scalar: PreparedNodeKind,
    /// `csv.item@1` — array-member occurrences.
    pub(crate) item: PreparedOccurrenceRole,
    /// `csv.field@1` — object-member occurrences.
    pub(crate) field: PreparedOccurrenceRole,
}

impl CsvHandles {
    fn resolve(schema: &PreparedDocumentSchema) -> Result<Self, CodecError> {
        Ok(Self {
            array: schema.node_kind(0).ok_or_else(data_contract)?,
            object: schema.node_kind(1).ok_or_else(data_contract)?,
            scalar: schema.node_kind(2).ok_or_else(data_contract)?,
            item: schema.occurrence_role(0).ok_or_else(data_contract)?,
            field: schema.occurrence_role(1).ok_or_else(data_contract)?,
        })
    }
}

/// The session-owned copy of the requirement's kept-subtree prune hint, flattened to plain lookup nodes (json's
/// `PruneMap` shape). `None` when no tree rides the requirement or it keeps everything at the root (nothing to prune).
///
/// The hint is MONOTONE: omitting members the tree names unobservable is always sound, and a codec is free to ignore it
/// entirely. The headered dialect consults it to build only the members the program provably reads; the array dialect
/// never receives one (the engine's static-`.[i]` widening keeps array containers whole), so the array arm is
/// untouched.
pub(crate) struct PruneLookup {
    nodes: Vec<PruneLookupNode>,
}

struct PruneLookupNode {
    all: bool,
    element: Option<u32>,
    /// Ascending by key bytes (the transport's push-order contract).
    keys: Vec<(Box<[u8]>, u32)>,
}

impl PruneLookup {
    /// Copies the transported tree; `None` when the hint keeps everything at the root (nothing to prune).
    fn from_transport(tree: &jqf_codec_core::PruneTree) -> Option<Self> {
        if tree.root().is_all() {
            return None;
        }
        let mut nodes = Vec::new();
        for id in 0..u32::MAX {
            let Some(node) = tree.node(id) else { break };
            nodes.push(PruneLookupNode {
                all: node.is_all(),
                element: node.element(),
                keys: node
                    .members()
                    .map(|(name, child)| (Box::from(name.as_bytes()), child))
                    .collect(),
            });
        }
        Some(Self { nodes })
    }

    /// Whether the root object's member `name` is unobservable and MAY be omitted. A node that keeps everything, a
    /// named key, or a shared element node all deliver the member.
    fn omits_member(&self, name: &str) -> bool {
        let Some(node) = self.nodes.get(jqf_codec_core::PruneTree::ROOT as usize) else {
            return false;
        };
        if node.all {
            return false;
        }
        node.keys
            .binary_search_by(|(key, _)| key.as_ref().cmp(name.as_bytes()))
            .is_err()
            && node.element.is_none()
    }
}

/// Starts one record document's builder from the request prototype, with the CSV coverage law, returning the prepared
/// schema and handles beside it.
pub(crate) fn prepare_document(
    prototype: &DocumentSchemaPrototype,
    _resources: &mut ResourceContext<'_>,
) -> Result<(AccountedDocumentBuilder<'static>, PreparedDocumentSchema, CsvHandles), CodecError> {
    let (mut builder, schema) = prototype
        .try_new_builder_with_coverage(BuilderCoverage::minimal_semantic())
        .map_err(map_data)?;
    builder.set_authoritative_empty_families(AuthoritativeEmptyFamilies::from_family(
        DocumentCapabilityFamily::Attributes,
    ));
    builder.set_diagnostic_coverage(DiagnosticCoverage::NotRequested);
    let handles = CsvHandles::resolve(&schema)?;
    Ok((builder, schema, handles))
}

/// Copies one owned field string into the builder's stored text and returns its document-local identity (the
/// prepared-path equivalent of `AccountedSemanticNode::String`).
fn add_field_text(
    builder: &mut AccountedDocumentBuilder<'static>,
    field: &str,
    resources: &mut ResourceContext<'_>,
) -> Result<jqf_data::DocumentTextId, CodecError> {
    builder.store_text(field, resources).map_err(map_data)
}

/// One record's authored byte ranges, bound into the record's document so the edit lane can splice at exact offsets.
/// The spans are an addressing channel, never a second value: the stored semantic is exactly the decoded field string,
/// and the published bytes never move.
pub(crate) struct RecordSpans<'a> {
    /// One span per FIELD, aligned with the split's field order. Each names the field's RAW authored bytes — quotes
    /// included — never the decoded string, so a splice replaces the field's authored text exactly as it was written.
    pub(crate) fields: &'a [jqf_source::Span],
    /// The record payload's complete extent: every authored byte of the row (separators and quotes included). The
    /// record TERMINATOR is the framer's byte, never the payload's, so it is deliberately outside this span — the
    /// splice policy owns the per-dialect newline.
    pub(crate) record: jqf_source::Span,
}

/// Records one authored source span on the builder (the TOML `record_authored_span` shape): the semantic is stored
/// exactly as without the span; the span only names the authored bytes the edit lane echoes verbatim or replaces.
#[allow(
    unsafe_code,
    reason = "the late-sealing span contract is an unsafe fn by jqf-data's design; the spans were produced by this codec's own walk over the exact session-owned source authority"
)]
fn record_authored_span(
    builder: &mut AccountedDocumentBuilder<'static>,
    node: NodeId,
    span: jqf_source::Span,
    resources: &mut ResourceContext<'_>,
) -> Result<(), CodecError> {
    // SAFETY: the span was produced by this codec's own split walk over the
    // exact source authority the record's session binds to the builder, and
    // its bytes are the field's validated UTF-8 — the
    // `record_authored_span` contract.
    unsafe { builder.record_authored_span(node, span, resources) }.map_err(map_data)
}

/// Builds a fresh document from the validated recipe prototype for one record's document.
///
/// The document is built entirely from owned field strings with no source borrow, so the builder is `'static` and the
/// published product outlives the session's poll. With no header the record publishes as an ARRAY of its field strings;
/// with one it publishes as an OBJECT keyed by the header names in header order, and a field count that disagrees with
/// the header is the ragged-row rejection.
///
/// Under the headered dialect a prune hint may name the members the program provably reads: unobservable members are
/// OMITTED from the built document (fewer nodes per row). The ragged-row law still runs over the FULL field count, and
/// the pruned member set can never change a published byte (the residual only ever reads the members the tree kept).
///
/// The record extent binds the ROOT node and each field value binds its own raw authored span, in strict node order
/// (the `record_authored_span` record-order law: the root is this build's first node, the field values follow in field
/// order).
pub(crate) fn build_document(
    prototype: &DocumentSchemaPrototype,
    fields: &[String],
    header: Option<&[String]>,
    prune: Option<&PruneLookup>,
    spans: Option<&RecordSpans<'_>>,
    resources: &mut ResourceContext<'_>,
) -> Result<(AccountedDocumentBuilder<'static>, NodeId), CodecError> {
    let (mut builder, schema, handles) = prepare_document(prototype, resources)?;
    let Some(names) = header else {
        let root = builder
            .add_prepared_node(
                &schema,
                handles.array,
                PreparedSemanticNode::Array(handles.item),
                resources,
            )
            .map_err(map_data)?;
        if let Some(spans) = spans {
            record_authored_span(&mut builder, root, spans.record, resources)?;
        }
        for (index, field) in fields.iter().enumerate() {
            let text = add_field_text(&mut builder, field, resources)?;
            let value = builder
                .add_prepared_stored_string_node(&schema, handles.scalar, text, resources)
                .map_err(map_data)?;
            if let Some(span) = spans.and_then(|spans| spans.fields.get(index)) {
                record_authored_span(&mut builder, value, *span, resources)?;
            }
            builder
                .add_prepared_occurrence(
                    &schema,
                    jqf_data::LocalOwnerRef::Node(root),
                    handles.item,
                    None,
                    value,
                    resources,
                )
                .map_err(map_data)?;
        }
        return Ok((builder, root));
    };
    if names.len() != fields.len() {
        return Err(CodecError::new(CodecFailureKind::InvalidInput));
    }
    let root = builder
        .add_prepared_node(
            &schema,
            handles.object,
            PreparedSemanticNode::Object(handles.field),
            resources,
        )
        .map_err(map_data)?;
    if let Some(spans) = spans {
        record_authored_span(&mut builder, root, spans.record, resources)?;
    }
    for (index, (name, field)) in names.iter().zip(fields).enumerate() {
        if prune.is_some_and(|lookup| lookup.omits_member(name)) {
            continue;
        }
        let text = add_field_text(&mut builder, field, resources)?;
        let value = builder
            .add_prepared_stored_string_node(&schema, handles.scalar, text, resources)
            .map_err(map_data)?;
        if let Some(span) = spans.and_then(|spans| spans.fields.get(index)) {
            record_authored_span(&mut builder, value, *span, resources)?;
        }
        builder
            .add_prepared_occurrence(
                &schema,
                jqf_data::LocalOwnerRef::Node(root),
                handles.field,
                Some(jqf_data::AccountedOccurrenceKey::Text(name)),
                value,
                resources,
            )
            .map_err(map_data)?;
    }
    Ok((builder, root))
}

/// One record's decoded document.
///
/// The session is reset per record range. It splits the record's bytes into fields and builds a `CompleteDocument`: an
/// ARRAY of field strings, or — under the headered dialect — an OBJECT keyed by the header names.
pub(crate) struct CsvPayloadSession {
    options: CsvDecodeOptions,
    /// The header names under the headered dialect, `None` under the array dialect. Shared by handle so recycling a
    /// session costs no copy.
    header: Option<HeaderNames>,
    /// Exclusive physical end of the header unit (terminator included). `None` under the array dialect. Named in the
    /// ragged-row diagnostic so a blank first row is findable from the data-row rejection.
    header_end: Option<u64>,
    /// The request's immutable schema prototype, cloned (an `Arc` bump) from the provider at session creation and kept
    /// across resets, so every record's document starts from the cheap prototype path instead of re-binding the recipe.
    schema_prototype: DocumentSchemaPrototype,
    /// The headered dialect's prune hint: which header-named members the requesting program provably reads. `None`
    /// under the array dialect and for any program the tree cannot bound.
    prune: Option<PruneLookup>,
    /// Per-column keep mask resolved from `header` + `prune` once per session. `None` keeps every column (array
    /// dialect, or a tree that names every header).
    keep: Option<Box<[bool]>>,
    /// Whether any consumer of this decode may read authored spans. TRUE by construction of the requirement; the
    /// edit/splice drives leave it on, and a filter drive that can prove it never splices clears it.
    authored_spans: bool,
    /// Reused per-record field buffers: the vector grows to the widest row and each String slot keeps its allocation
    /// across records, so a record drive's decode path allocates per field shape, not per field.
    fields: Vec<String>,
    /// Reused per-field byte scratch for the quote/UTF-8 walk.
    scratch: Vec<u8>,
    /// Reused per-field authored-span ledger.
    spans: Vec<jqf_source::Span>,
    published: bool,
}

/// Resolves the headered prune tree into a per-column keep-mask. The header name order is fixed for the stream, so
/// `omits_member` is a per-column constant, not a per-record lookup.
fn keep_mask(header: Option<&[String]>, prune: Option<&PruneLookup>) -> Option<Box<[bool]>> {
    let names = header?;
    let lookup = prune?;
    let mask: Box<[bool]> = names.iter().map(|name| !lookup.omits_member(name)).collect();
    if mask.iter().all(|&keep| keep) {
        None
    } else {
        Some(mask)
    }
}

impl CsvPayloadSession {
    fn new(
        options: CsvDecodeOptions,
        header: Option<HeaderNames>,
        header_end: Option<u64>,
        schema_prototype: DocumentSchemaPrototype,
        prune: Option<PruneLookup>,
        authored_spans: bool,
    ) -> Self {
        let keep = keep_mask(header.as_ref().map(|names| names.as_slice()), prune.as_ref());
        Self {
            options,
            header,
            header_end,
            schema_prototype,
            prune,
            keep,
            authored_spans,
            fields: Vec::new(),
            scratch: Vec::new(),
            spans: Vec::new(),
            published: false,
        }
    }

    /// Re-seeds this session for a new record range.
    fn reset(&mut self) {
        self.published = false;
    }

    /// Re-arms prune / span demand from a reopened requirement.
    fn rearm(&mut self, requirement: &jqf_codec_core::AccessRequirement) {
        self.prune = requirement.prune().and_then(PruneLookup::from_transport);
        self.keep = keep_mask(self.header.as_ref().map(|names| names.as_slice()), self.prune.as_ref());
        self.authored_spans = requirement.authored_spans();
    }
}

impl AccessSession for CsvPayloadSession {
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
    #[allow(
        unsafe_code,
        reason = "the source-binding and span-admission APIs are unsafe by jqf-data's design; the codec-core session continuously owns the exact record-range authority every call receives"
    )]
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
        if self.published {
            return Err(data_contract());
        }
        let payload = source.bytes();
        self.spans.clear();
        let spans = self.authored_spans.then_some(&mut self.spans);
        let count = crate::fields::split_fields_into(
            payload,
            self.options.delimiter(),
            self.options.quote(),
            self.options.textdata(),
            &mut self.fields,
            &mut self.scratch,
            spans,
            self.keep.as_deref(),
        )?;
        if let Some(names) = &self.header
            && names.len() != count
        {
            return Err(crate::error::ragged_row(
                names.len(),
                count,
                self.header_end.unwrap_or(0),
            ));
        }
        // The record extent binds the row's own authored bytes only: the payload the framer handed over, terminator
        // excluded.
        let record_span = jqf_source::Span::try_from_usize(0, payload.len())
            .map_err(|_| CodecError::new(CodecFailureKind::InvalidInput))?;
        let prototype = &self.schema_prototype;
        let record_spans = self.authored_spans.then_some(RecordSpans {
            fields: &self.spans[..count],
            record: record_span,
        });
        let (mut builder, root) = build_document(
            prototype,
            &self.fields[..count],
            self.header.as_ref().map(|names| names.as_slice()),
            self.prune.as_ref(),
            record_spans.as_ref(),
            context.resources(),
        )?;
        let document = if self.authored_spans {
            // The spans name the record's own narrowed source, so the record's segment must seal the builder before
            // publication — TOML's late-sealing shape. Hash is off because every consumer of this binding reads
            // through metadata-checked access and never re-verifies the digest (the record session's only span
            // consumers are `record_authored_span` bounds checks and the edit lane).
            let mut stage = jqf_data::DocumentSourceBindingStage::new(source).map_err(map_data)?;
            let binding = loop {
                // SAFETY: codec-core retains one immutable source authority for
                // the complete access session and passes that exact authority
                // each poll; the stage was constructed over the same narrowed
                // record segment and re-verifies identity on every call.
                match unsafe { stage.poll(source, context.resources()) }.map_err(map_data)? {
                    jqf_data::DocumentSourceBindingPoll::Pending => context.replenish_work()?,
                    jqf_data::DocumentSourceBindingPoll::Ready(binding) => break binding,
                }
            };
            builder.bind_source(binding).map_err(map_data)?;
            builder.finish(root, context.resources()).map_err(map_data)?
        } else {
            // No spans were admitted, so finish needs no seal. The borrowed-source attach is gated with the same hint:
            // it reads the seal `bind_source` would have written.
            builder.finish(root, context.resources()).map_err(map_data)?
        };
        let product = DocumentProduct::try_new(document, context.resources())?;
        let product = if self.authored_spans {
            // SAFETY: codec-core owns this exact immutable record-range byte
            // authority for the whole access session; the binding was taken over
            // this same narrowed segment and `record_authored_span` proved every
            // committed span against it.
            unsafe { product.attach_borrowed_source_from_access_session(source, context.resources())? }
        } else {
            product
        };
        self.published = true;
        Ok(AccessResult::from_outcome(AccessOutcome::FullDocument(product)))
    }
}

/// The header row's resolved names, shared by every session of one provider.
///
/// Under the headered dialect the names are read ONCE from the start of the retained source and shared by handle, so
/// recycling a session — or opening the scoped route — costs no copy and can never observe a half-filled header.
/// The array dialect has no header at all, which is what makes a member-name path there a type mismatch by CONSTRUCTION
/// rather than by a lazily-empty cell.
pub(crate) type HeaderNames = alloc::rc::Rc<alloc::vec::Vec<alloc::string::String>>;

/// Provider-level state for CSV payload decode.
pub(crate) struct CsvPayloadProvider {
    routes: Vec<RouteDescription>,
    options: CsvDecodeOptions,
    /// The header row's names under the headered dialect, `None` otherwise.
    header: Option<HeaderNames>,
    /// Exclusive physical end of the header unit. Lockstep with `header`.
    header_end: Option<u64>,
    /// The request's immutable schema prototype, built lazily on the first document-building open and cloned per
    /// session (json's fixed-floor shape). `None` until something actually builds a record document.
    schema_prototype: Option<DocumentSchemaPrototype>,
}

impl CsvPayloadProvider {
    pub(crate) fn try_new(
        options: CsvDecodeOptions,
        header: Option<HeaderNames>,
        header_end: Option<u64>,
        diagnostics: jqf_codec_core::DiagnosticPolicy,
        resources: &ResourceContext<'_>,
    ) -> Result<Self, CodecError> {
        let guarantees = AccessGuarantees::new(jqf_codec_core::ValidationMode::Strict, diagnostics);
        let routes = RouteDescription::try_table(
            &[
                (
                    DECODE_ROUTE_SLOT,
                    jqf_codec_core::AccessFootprintKind::Whole,
                    AccessResultKind::CompleteDocument,
                ),
                // Scoped exact-path route (slot 1): a static column path (`.[1]`, `.name`) materializes only the
                // demanded field. The core binder Direct-binds it for Located requirements whose demand fits within the
                // minimal semantic ceiling.
                (
                    SCOPED_ROUTE_SLOT,
                    jqf_codec_core::AccessFootprintKind::Exact,
                    AccessResultKind::Located,
                ),
            ],
            guarantees,
            resources,
        )?;
        Ok(Self {
            routes,
            options,
            header,
            header_end,
            schema_prototype: None,
        })
    }

    /// Returns a cheap clone (an `Arc` bump) of the request's immutable schema prototype, building it once from this
    /// provider's dialect on first use — json's fixed-floor mechanism: the full recipe→prepare→ bind chain runs
    /// once per request instead of once per record.
    fn try_schema_prototype(&mut self, resources: &ResourceContext<'_>) -> Result<DocumentSchemaPrototype, CodecError> {
        try_schema_prototype(&mut self.schema_prototype, self.options, resources)
    }
}

impl jqf_codec_core::InputProvider for CsvPayloadProvider {
    fn route_descriptions(&self) -> &[RouteDescription] {
        self.routes.as_slice()
    }

    fn supports_attribute_absence(&self) -> bool {
        true
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one dispatch per advertised slot: the route table is read as a single unit"
    )]
    fn open_route<'source>(
        &mut self,
        input: ProviderInput<'source>,
        slot: RouteSlot,
        requirement: &jqf_codec_core::AccessRequirement,
        resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedAccessSession<'source>, CodecError> {
        if slot == SCOPED_ROUTE_SLOT {
            if requirement.footprint().is_whole()
                || requirement.schedule().is_empty_complete()
                || requirement.result() != AccessResultKind::Located
            {
                return Err(CodecError::new(CodecFailureKind::ProviderRouteMismatch));
            }
            let path = requirement
                .footprint()
                .exact_path()
                .ok_or_else(|| CodecError::new(CodecFailureKind::ProviderRouteMismatch))?;
            // Dispatch never falls back: a `ProviderRouteMismatch` here is a hard error, not a floor retry. A longer
            // path still opens this session; the session answers the residual as the floor would (a CSV field is a
            // string, so step 1 is a type mismatch).
            let origin = requirement
                .schedule()
                .singleton_origin()
                .ok_or_else(|| CodecError::new(CodecFailureKind::ProviderRouteMismatch))?;
            let header = self.header.clone();
            let header_end = self.header_end;
            let options = self.options;
            let schema_prototype = self.try_schema_prototype(resources)?;
            return ErasedAccessSession::try_new_source_with_route(input.source(), SCOPED_PHYSICAL_ROUTE_ID, || {
                Ok(crate::column::CsvColumnSession::new(
                    options,
                    header,
                    header_end,
                    path.steps(),
                    origin,
                    schema_prototype,
                ))
            });
        }
        // Whole-record validation is served by the ordinary whole-document slot: a record-independent program binds the
        // lazy whole document, which validates to the same full strictness and never materializes when nothing reads
        // it.
        if slot != DECODE_ROUTE_SLOT
            || !requirement.footprint().is_whole()
            || !requirement.schedule().is_empty_complete()
            || requirement.result() != AccessResultKind::CompleteDocument
        {
            return Err(CodecError::new(CodecFailureKind::ProviderRouteMismatch));
        }
        let options = self.options;
        let header = self.header.clone();
        let header_end = self.header_end;
        let schema_prototype = self.try_schema_prototype(resources)?;
        // The prune hint rides the whole-document requirement: the engine lowered the kept-subtree tree for the
        // requesting program, and the headered build consults it to omit members the program can never read. The array
        // dialect never receives a tree (static indices keep the container whole).
        let prune = requirement.prune().and_then(PruneLookup::from_transport);
        let authored_spans = requirement.authored_spans();
        ErasedAccessSession::try_new_source_with_route(input.source(), DECODE_PHYSICAL_ROUTE_ID, || {
            Ok(CsvPayloadSession::new(
                options,
                header,
                header_end,
                schema_prototype,
                prune,
                authored_spans,
            ))
        })
    }

    fn try_reopen_route(
        &mut self,
        state: &mut jqf_codec_core::RecycledSessionState<'_>,
        _slot: RouteSlot,
        requirement: &jqf_codec_core::AccessRequirement,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<bool, jqf_codec_core::CodecError> {
        // A recycled whole-document session must reset its per-record publication state; a recycled single-column
        // session resets its own flag (both modes reset identically).
        if let Some(session) = state.downcast_mut::<CsvPayloadSession>() {
            session.reset();
            session.rearm(requirement);
            return Ok(true);
        }
        if let Some(session) = state.downcast_mut::<crate::column::CsvColumnSession>() {
            session.reset();
            return Ok(true);
        }
        Ok(false)
    }
}

/// The record drive's payload provider: opens the CSV decode route over the whole source. This is the entry the SDK's
/// `execute_record_sequence` resolves through the catalog: `catalog.decoder("csv", <dialect>)` must yield a provider
/// whose per-record-range session builds that record's document.
///
/// `DecodeRequest` carries no dialect, so HEADER MODE arrives the only way it can: through the request's normalized
/// [`CsvDecodeOptions`] payload, whose schema identity the registration already validates. An absent payload is the
/// array dialect's defaults, which keeps every existing caller's meaning exactly.
pub fn create_payload_provider<'source>(
    source: ResolvedSource<'source>,
    request: jqf_codec_core::DecodeRequest<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<jqf_codec_core::ErasedProvider<'source>, CodecError> {
    if request.validation != jqf_codec_core::ValidationMode::Strict || request.allow_adjacent_values {
        return Err(CodecError::new(CodecFailureKind::RequirementMismatch));
    }
    let options = match request.options {
        None => CsvDecodeOptions::try_new(None, None, u64::MAX, false)
            .map_err(|_| CodecError::new(CodecFailureKind::RequirementMismatch))?,
        Some(payload) => *payload
            .downcast_ref::<CsvDecodeOptions>()
            .ok_or_else(|| CodecError::new(CodecFailureKind::RequirementMismatch))?,
    };
    // The header is read from the START of the retained source, not from the first record the drive happens to hand
    // over: the framer has already consumed that record under this dialect, and a session that learned the names from
    // its own first payload could never serve the first DATA row.
    let header = if options.header() {
        crate::header::parse(source.bytes(), options.delimiter(), options.quote(), options.textdata())?
            .map(alloc::rc::Rc::new)
    } else {
        None
    };
    let header_end = header
        .as_ref()
        .map(|_| crate::header::physical_end(source.bytes(), options.delimiter(), options.quote()));
    jqf_codec_core::ErasedProvider::try_new_provider::<CsvPayloadProvider, _>(source, resources, || {
        CsvPayloadProvider::try_new(options, header.clone(), header_end, request.diagnostics, resources)
    })
}

fn map_data(error: DataError) -> CodecError {
    jqf_codec_core::map_data(error, "CSV builder rejected document construction")
}

pub(crate) fn data_contract() -> CodecError {
    jqf_codec_core::data_contract("CSV authoritative document construction")
}
