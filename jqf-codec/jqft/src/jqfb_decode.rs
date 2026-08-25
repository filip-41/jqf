//! The `jqfb` decode provider and access session.
//!
//! Advertised route slots: 0 Whole/`CompleteDocument` (this file's decode walk) and 1 Exact/`Located` (the node-table
//! walk in `jqfb_routes.rs`). The session validates the header, the footer directory, and every chunk against the file
//! extent FIRST (the trust boundary), then verifies each chunk's blake3 digest, re-slices the core chunk payloads from
//! the source, and walks the flattened preorder node table into the Document model with an EXPLICIT container stack —
//! document depth costs heap, never call stack — validating the subtree-size invariant exactly. The FACT chunk
//! re-attaches attached facts (markup name/attrs/content/attribute/comment), the SOUR chunk becomes a `jqfb.source@1`
//! fact on the root (conformance level 1, the lossless re-emission authority), and the PROV chunk becomes a
//! `.@provenance` fact.

use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use jqf_codec_core::{
    AccessFootprintKind, AccessGuarantees, AccessInput, AccessOutcome, AccessRequirement, AccessResult,
    AccessResultKind, AccessSession, CodecError, CodecFailureKind, CodecRunContext, DecodeRequest, DiagnosticPolicy,
    DocumentProduct, ErasedAccessSession, ErasedProvider, InputProvider, ProviderInput, RouteDescription, RouteSlot,
};
use jqf_data::{
    AccountedDocumentBuilder, AccountedDocumentFinalizer, AccountedIntrinsicTag, AccountedOccurrenceKey,
    AccountedSemanticNode, BuilderCoverage, DataError, DocumentFinalizationPoll, DocumentTextId, FactPayload,
    LocalOwnerRef, NodeId, TagId,
};
use jqf_resource::{ResourceContext, WorkAdmission};
use jqf_source::ResolvedSource;

use crate::jqfb::{self, CoreChunks, DirectoryEntry, Footer, NodeEntry, kinds};
use crate::jqfb_routes::{self};
use crate::parse::{Temporal, try_temporal};
use crate::provider::{self, jqfb_recipe};
use crate::{JQFB_FULL_PHYSICAL_ROUTE_ID, JQFB_LOCATED_PHYSICAL_ROUTE_ID};

pub(crate) fn create_jqfb_provider<'source>(
    source: ResolvedSource<'source>,
    request: DecodeRequest<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedProvider<'source>, CodecError> {
    request.expect_strict_defaults()?;
    let provider = JqfbProvider {
        routes: jqfb_route_inventory(resources)?,
    };
    ErasedProvider::try_new_provider(source, resources, || Ok(provider))
}

fn jqfb_route_inventory(resources: &ResourceContext<'_>) -> Result<Vec<RouteDescription>, CodecError> {
    let guarantees = AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly);
    // Slot 0: Whole/CompleteDocument; slots 1: the native Located route served by the node-table walk with subtree_size
    // skips.
    RouteDescription::try_table(
        &[
            (
                RouteSlot::new(0),
                AccessFootprintKind::Whole,
                AccessResultKind::CompleteDocument,
            ),
            (RouteSlot::new(1), AccessFootprintKind::Exact, AccessResultKind::Located),
        ],
        guarantees,
        resources,
    )
}

struct JqfbProvider {
    routes: Vec<RouteDescription>,
}

impl InputProvider for JqfbProvider {
    fn route_descriptions(&self) -> &[RouteDescription] {
        self.routes.as_slice()
    }

    fn open_route<'source>(
        &mut self,
        input: ProviderInput<'source>,
        slot: RouteSlot,
        requirement: &AccessRequirement,
        resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedAccessSession<'source>, CodecError> {
        let source = input.source();
        if slot == RouteSlot::new(0) {
            requirement.expect_whole(AccessResultKind::CompleteDocument)?;
            let state = JqfbDecodeState::try_new(source, resources)?;
            return ErasedAccessSession::try_new_source_with_route(source, JQFB_FULL_PHYSICAL_ROUTE_ID, || Ok(state));
        }
        // The demand routes share one validated image (the trust boundary: header, footer directory, every chunk
        // digest, pool extents).
        let image = JqfbImage::validate(source.bytes())?;
        if slot == RouteSlot::new(1) {
            let (path, origin) = requirement.expect_exact(AccessResultKind::Located)?;
            let session = jqfb_routes::NativeLocatedSession::try_new(image, path.steps(), origin)?;
            return ErasedAccessSession::try_new_source_with_route(source, JQFB_LOCATED_PHYSICAL_ROUTE_ID, || {
                Ok(session)
            });
        }
        Err(CodecError::new(CodecFailureKind::ProviderRouteMismatch))
    }
}

enum Phase {
    Decode,
    Seal,
    Finalize,
    Publish,
}

enum Frame {
    Array {
        owner: NodeId,
        start: usize,
        subtree_size: u32,
        remaining: usize,
    },
    Object {
        owner: NodeId,
        start: usize,
        subtree_size: u32,
        remaining: usize,
        pending_key: Option<DocumentTextId>,
    },
    Tag {
        owner: NodeId,
        start: usize,
        subtree_size: u32,
        remaining: usize,
    },
}

impl Frame {
    fn remaining(&self) -> usize {
        match self {
            Self::Array { remaining, .. } | Self::Object { remaining, .. } | Self::Tag { remaining, .. } => *remaining,
        }
    }

    fn owner(&self) -> NodeId {
        match self {
            Self::Array { owner, .. } | Self::Object { owner, .. } | Self::Tag { owner, .. } => *owner,
        }
    }

    /// The frame's authored span so far, in bytes. A span past the u32 range cannot match any stored `subtree_size`, so
    /// it is a structural rejection — never a saturating value that could pass the check.
    fn span(&self, cursor: usize) -> Result<u32, CodecError> {
        let start = match self {
            Self::Array { start, .. } | Self::Object { start, .. } | Self::Tag { start, .. } => *start,
        };
        u32::try_from(cursor.saturating_sub(start))
            .map_err(|_| jqfb::invalid("a container span exceeds the subtree-size range"))
    }

    fn subtree_size(&self) -> u32 {
        match self {
            Self::Array { subtree_size, .. } | Self::Object { subtree_size, .. } | Self::Tag { subtree_size, .. } => {
                *subtree_size
            }
        }
    }
}

pub(crate) struct JqfbDecodeState {
    cursor: usize,
    /// The first node-table index this decode covers (0 for a whole-image decode; the located subtree's start for the
    /// scoped route).
    start: usize,
    /// One past the last node-table index this decode covers (`node_count` for a whole-image decode; `start +
    /// subtree_size` for the scoped route — the `subtree_size` skip advance in its bounded form).
    end: usize,
    frames: Vec<Frame>,
    /// The document node produced for each covered node-table entry (None for a KEYTEXT entry, which never becomes a
    /// document node), indexed by `cursor - 1 - start`. Fact records map their absolute table index through `start` to
    /// this vector.
    node_ids: Vec<Option<NodeId>>,
    root: Option<NodeId>,
    builder: Option<AccountedDocumentBuilder<'static>>,
    finalizer: Option<AccountedDocumentFinalizer<'static>>,
    product: Option<DocumentProduct<'static>>,
    phase: Phase,
    published: bool,
    /// The in-flight cooperative source seal, when the whole-document decode bound authored spans: every node's span
    /// runs to the image end, so the decode seals the source before finalize.
    binding_stage: Option<jqf_data::DocumentSourceBindingStage>,
    /// Whether this decode is SCOPED to a located subtree: FACT records whose owner sits in the range attach
    /// (markup/comment travel with the subtree); source and provenance stay whole-document; no authored spans are bound
    /// (the edit lane uses the whole-document route).
    scoped: bool,
    /// The validated image: core chunk extents within the source and the pool offset tables. The erased session cannot
    /// hold a borrow of the source, so `try_new` records each chunk's extent and every poll re-slices the chunks from
    /// the source bytes it is handed.
    image: JqfbImage,
}

/// One validated jqfb image: the core chunk extents within the source and the pool offset tables, built by the shared
/// validation pass (header, footer directory, every chunk digest, pool extents). Shared by the whole-document decode
/// and the demand routes — each re-slices the chunks from its own poll's source bytes.
///
/// Cloning is cheap by design (the pool offset tables are `Arc` slices): the scoped session clones the whole image per
/// located subtree, and a copy of an offset table there would cost O(pool entries) per hit.
#[derive(Clone)]
pub(crate) struct JqfbImage {
    pub(crate) chunks: ChunkRanges,
    pub(crate) node_count: usize,
    /// The image's total byte length, recorded at `validate`: the authored span of every node runs from its table entry
    /// through this end, so a tail replacement in the edit lane carries the footer words.
    pub(crate) len: usize,
    /// The byte offset of each STRG pool entry's length word, built ONCE by `validate`: a per-index rescan from offset
    /// 8 made the decode Theta(N^2) over the pool.
    pub(crate) strg_offsets: Arc<[u32]>,
    /// The byte offset of each NUMB pool entry's tag byte, built once by `validate` — the number pool's identical
    /// rescan pattern, fixed with it.
    pub(crate) numb_offsets: Arc<[u32]>,
}

impl JqfbImage {
    /// Validates the header, footer directory, every chunk digest, the pool extents, and builds the pool offset tables.
    /// One validating walk per pool, Theta(N).
    pub(crate) fn validate(bytes: &[u8]) -> Result<Self, CodecError> {
        if !bytes.starts_with(jqfb::MAGIC) {
            return Err(jqfb::invalid("the file is not a jqfb image (bad magic)"));
        }
        let version = jqfb::read_u16(bytes, 4).ok_or_else(|| jqfb::invalid("truncated header"))?;
        if version != jqfb::VERSION {
            return Err(CodecError::new(CodecFailureKind::UnsupportedRepresentation));
        }
        let footer = jqfb::read_footer(bytes)?;
        jqfb::v1_accepts(&footer.entries)?;
        for entry in &footer.entries {
            let payload = chunk_payload(&footer, entry, bytes)?;
            let digest = *blake3::hash(payload).as_bytes();
            if digest != entry.digest {
                return Err(jqfb::invalid("a chunk digest does not match its payload"));
            }
        }
        let core = jqfb::locate_core(&footer.entries, bytes)?;
        let node_count = jqfb::node_count(core.node)?;
        let strg_count = jqfb::pool_count(core.strg)?;
        let numb_count = jqfb::pool_count(core.numb)?;
        let strg_offsets = pool_offsets(core.strg, strg_count)?;
        let numb_offsets = number_offsets(core.numb, numb_count)?;
        // The chunk extents within the source, so each poll can re-slice them (V7a: no whole-file staging copy).
        // Pointer-arithmetic offset of a subslice is safe: every non-empty slice comes from `bytes` (an absent FACT
        // chunk is `locate_core`'s zero-length default, which points nowhere and maps to the empty range).
        let range_of = |chunk: &[u8]| -> (usize, usize) {
            if chunk.is_empty() {
                (0, 0)
            } else {
                let start = chunk.as_ptr() as usize - bytes.as_ptr() as usize;
                (start, start + chunk.len())
            }
        };
        Ok(Self {
            chunks: ChunkRanges {
                node: range_of(core.node),
                strg: range_of(core.strg),
                numb: range_of(core.numb),
                fact: range_of(core.fact),
                prov: core.prov.map(range_of),
                sour: core.sour.map(range_of),
            },
            node_count,
            len: bytes.len(),
            strg_offsets: strg_offsets.into(),
            numb_offsets: numb_offsets.into(),
        })
    }

    /// Re-borrows the core chunks from the poll's source bytes.
    pub(crate) fn slice<'s>(&self, bytes: &'s [u8]) -> Result<CoreChunks<'s>, CodecError> {
        self.chunks.slice(bytes)
    }
}

/// The byte extents of the core chunks within the source, recorded at `try_new` (the erased session cannot hold a
/// borrow). `slice` re-borrows them from the poll's source bytes, so the decode reads the resolved bytes directly —
/// never a whole-file staging copy.
#[derive(Clone, Copy)]
pub(crate) struct ChunkRanges {
    node: (usize, usize),
    strg: (usize, usize),
    numb: (usize, usize),
    fact: (usize, usize),
    prov: Option<(usize, usize)>,
    sour: Option<(usize, usize)>,
}

impl ChunkRanges {
    fn slice<'s>(&self, bytes: &'s [u8]) -> Result<CoreChunks<'s>, CodecError> {
        let take = |(start, end): (usize, usize)| {
            bytes
                .get(start..end)
                .ok_or_else(|| jqfb::invalid("a recorded chunk range lies outside the source"))
        };
        Ok(CoreChunks {
            node: take(self.node)?,
            strg: take(self.strg)?,
            numb: take(self.numb)?,
            fact: take(self.fact)?,
            prov: self.prov.map(take).transpose()?,
            sour: self.sour.map(take).transpose()?,
        })
    }
}

impl JqfbDecodeState {
    pub(crate) fn try_new(source: ResolvedSource<'_>, resources: &mut ResourceContext<'_>) -> Result<Self, CodecError> {
        let image = JqfbImage::validate(source.bytes())?;
        let node_count = image.node_count;
        let mut state = Self::new_with_image(image, BuilderCoverage::complete(), resources)?;
        state.cursor = 0;
        state.start = 0;
        state.end = node_count;
        state.scoped = false;
        state.node_ids = Vec::with_capacity(node_count);
        Ok(state)
    }

    /// A decode state SCOPED to one located subtree (the scoped route's second read): the walk covers `[start, start +
    /// size)` node-table entries and attaches FACT records whose owner sits in that range (markup/comment facts travel
    /// with the located subtree). Source and provenance stay whole-document. The builder keeps minimal semantics plus
    /// attached facts. The image was already validated by the session's walk, so this construction is allocation-only.
    pub(crate) fn try_new_scoped(
        image: &JqfbImage,
        start: usize,
        size: u32,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Self, CodecError> {
        let node_count = image.node_count;
        let end = start
            .checked_add(usize::try_from(size).map_err(|_| jqfb::invalid("subtree size overflows"))?)
            .ok_or_else(|| jqfb::invalid("subtree extent overflows"))?;
        if end > node_count {
            return Err(jqfb::invalid("the located subtree exceeds the node table"));
        }
        let mut state = Self::new_with_image(
            image.clone(),
            BuilderCoverage::minimal_semantic().with_attached_facts(true),
            resources,
        )?;
        state.cursor = start;
        state.start = start;
        state.end = end;
        state.scoped = true;
        state.node_ids = Vec::with_capacity(end - start);
        Ok(state)
    }

    /// Shared construction: the validated image plus a fresh builder at the requested coverage.
    fn new_with_image(
        image: JqfbImage,
        coverage: BuilderCoverage,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<Self, CodecError> {
        let recipe = jqfb_recipe().map_err(map_data)?;
        let (mut builder, _schema) =
            AccountedDocumentBuilder::try_new_prepared_with_coverage(&recipe, coverage).map_err(map_data)?;
        builder.set_authoritative_empty_families(jqf_data::AuthoritativeEmptyFamilies::from_family(
            jqf_data::DocumentCapabilityFamily::Attributes,
        ));
        builder.set_diagnostic_coverage(jqf_data::DiagnosticCoverage::NotRequested);
        Ok(Self {
            cursor: 0,
            start: 0,
            end: 0,
            frames: Vec::new(),
            node_ids: Vec::new(),
            root: None,
            builder: Some(builder),
            finalizer: None,
            product: None,
            phase: Phase::Decode,
            published: false,
            binding_stage: None,
            scoped: false,
            image,
        })
    }

    /// The decode walk: one bounded step (a node entry or a frame pop).
    #[allow(
        clippy::too_many_lines,
        reason = "one node-kind dispatch table: every table kind's build law sits beside the others"
    )]
    pub(crate) fn decode_step(
        &mut self,
        chunks: &CoreChunks<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        // Pop finished frames and attach their owners.
        while let Some(frame) = self.frames.last() {
            if frame.remaining() != 0 {
                break;
            }
            let frame = self.frames.pop().ok_or_else(data_contract)?;
            if frame.span(self.cursor)? != frame.subtree_size() {
                return Err(jqfb::invalid("a node's subtree size does not match its span"));
            }
            let owner = frame.owner();
            self.attach(owner, resources)?;
        }
        if self.frames.is_empty() && self.root.is_some() {
            if self.cursor != self.end {
                return Err(jqfb::invalid("the node table has trailing entries"));
            }
            return Ok(false);
        }
        let entry = jqfb::read_node(chunks.node, self.cursor)?;
        self.cursor += 1;
        if self.cursor > self.end {
            return Err(jqfb::invalid("the node table walk exceeds its extent"));
        }
        self.node_ids.push(None);
        let table_index = self.cursor - 1 - self.start;
        match entry.kind {
            kinds::NULL | kinds::BOOL => {
                let semantic = if entry.kind == kinds::NULL {
                    AccountedSemanticNode::Null
                } else {
                    AccountedSemanticNode::Bool(entry.payload != 0)
                };
                self.finish_scalar(entry, semantic, table_index, resources)?;
            }
            kinds::INTEGER | kinds::DECIMAL | kinds::FLOAT => {
                let offset = self.numb_offset(entry.payload)?;
                let body = number_body(chunks.numb, offset)?;
                let node = match body.first() {
                    Some(0) => {
                        let (text, _) = jqfb::pool_entry(body, 1)?;
                        let text = core::str::from_utf8(text)
                            .map_err(|_| jqfb::invalid("an integer pool entry is not UTF-8"))?;
                        jqf_data::Integer::parse(text)
                            .map_err(|_| jqfb::invalid("an integer pool entry does not parse"))?;
                        self.add_scalar(AccountedSemanticNode::Integer(text), resources)?
                    }
                    Some(1) => {
                        let (coef, after) = jqfb::pool_entry(body, 1)?;
                        let coef = core::str::from_utf8(coef)
                            .map_err(|_| jqfb::invalid("a decimal pool entry is not UTF-8"))?;
                        jqf_data::Integer::parse(coef)
                            .map_err(|_| jqfb::invalid("a decimal pool entry does not parse"))?;
                        let scale =
                            jqfb::read_u64(body, after).ok_or_else(|| jqfb::invalid("truncated decimal scale"))?;
                        self.add_scalar(
                            AccountedSemanticNode::Decimal {
                                coefficient: coef,
                                scale: i64::from_ne_bytes(scale.to_ne_bytes()),
                            },
                            resources,
                        )?
                    }
                    Some(2) => {
                        let bits = jqfb::read_u64(body, 1).ok_or_else(|| jqfb::invalid("truncated float bits"))?;
                        self.add_scalar(
                            AccountedSemanticNode::Float(jqf_data::Float::new(f64::from_bits(bits))),
                            resources,
                        )?
                    }
                    _ => return Err(jqfb::invalid("unknown number pool tag")),
                };
                self.note_scalar(entry, node, table_index, resources)?;
            }
            kinds::STRING | kinds::BYTES => {
                let offset = self.strg_offset(entry.payload)?;
                let raw = pool_bytes(chunks.strg, offset)?;
                let semantic = if entry.kind == kinds::STRING {
                    let text =
                        core::str::from_utf8(raw).map_err(|_| jqfb::invalid("a string pool entry is not UTF-8"))?;
                    AccountedSemanticNode::String(text)
                } else {
                    AccountedSemanticNode::Bytes(raw)
                };
                self.finish_scalar(entry, semantic, table_index, resources)?;
            }
            kinds::LOCAL_DATE | kinds::LOCAL_TIME | kinds::LOCAL_DATE_TIME | kinds::OFFSET_DATE_TIME => {
                let offset = self.strg_offset(entry.payload)?;
                let raw = pool_bytes(chunks.strg, offset)?;
                let text =
                    core::str::from_utf8(raw).map_err(|_| jqfb::invalid("a temporal pool entry is not UTF-8"))?;
                let temporal =
                    try_temporal(text).ok_or_else(|| jqfb::invalid("a temporal pool entry does not parse"))?;
                let node = match &temporal {
                    Temporal::LocalDate(date) => self.add_scalar(AccountedSemanticNode::LocalDate(*date), resources)?,
                    Temporal::LocalTime(time) => self.add_scalar(AccountedSemanticNode::LocalTime(time), resources)?,
                    Temporal::LocalDateTime(datetime) => {
                        self.add_scalar(AccountedSemanticNode::LocalDateTime(datetime), resources)?
                    }
                    Temporal::OffsetDateTime(datetime) => {
                        self.add_scalar(AccountedSemanticNode::OffsetDateTime(datetime), resources)?
                    }
                };
                self.note_scalar(entry, node, table_index, resources)?;
            }
            kinds::TAG => {
                let offset = self.strg_offset(entry.payload)?;
                let raw = pool_bytes(chunks.strg, offset)?;
                let text = core::str::from_utf8(raw).map_err(|_| jqfb::invalid("a tag pool entry is not UTF-8"))?;
                let tag =
                    TagId::try_new_unaccounted(text).map_err(|_| jqfb::invalid("a tag is not one nonempty string"))?;
                let layer = self
                    .builder_mut()?
                    .add_node(
                        provider::kind_for("jqfb", &AccountedSemanticNode::Unrepresentable),
                        AccountedSemanticNode::Unrepresentable,
                        Some(AccountedIntrinsicTag::Tagged(tag.as_str())),
                        resources,
                    )
                    .map_err(map_data)?;
                self.node_ids[table_index] = Some(layer);
                self.bind_node_span(layer, table_index, resources)?;
                self.frames.push(Frame::Tag {
                    owner: layer,
                    start: self.cursor - 1,
                    subtree_size: entry.subtree_size,
                    remaining: 1,
                });
            }
            kinds::ARRAY => {
                let semantic = AccountedSemanticNode::Array {
                    item_role: provider::role_for("jqfb", "array"),
                };
                let owner = self
                    .builder_mut()?
                    .add_node(provider::kind_for("jqfb", &semantic), semantic, None, resources)
                    .map_err(map_data)?;
                self.node_ids[table_index] = Some(owner);
                self.bind_node_span(owner, table_index, resources)?;
                let children =
                    usize::try_from(entry.payload).map_err(|_| jqfb::invalid("array child count overflows"))?;
                self.frames.push(Frame::Array {
                    owner,
                    start: self.cursor - 1,
                    subtree_size: entry.subtree_size,
                    remaining: children,
                });
            }
            kinds::OBJECT => {
                let semantic = AccountedSemanticNode::Object {
                    member_role: provider::role_for("jqfb", "object"),
                };
                let owner = self
                    .builder_mut()?
                    .add_node(provider::kind_for("jqfb", &semantic), semantic, None, resources)
                    .map_err(map_data)?;
                self.node_ids[table_index] = Some(owner);
                self.bind_node_span(owner, table_index, resources)?;
                let members =
                    usize::try_from(entry.payload).map_err(|_| jqfb::invalid("object member count overflows"))?;
                let remaining = members
                    .checked_mul(2)
                    .ok_or_else(|| jqfb::invalid("object member count overflows"))?;
                self.frames.push(Frame::Object {
                    owner,
                    start: self.cursor - 1,
                    subtree_size: entry.subtree_size,
                    remaining,
                    pending_key: None,
                });
            }
            kinds::KEYTEXT => {
                let in_key_position = matches!(self.frames.last(), Some(Frame::Object { pending_key: None, .. }));
                if !in_key_position {
                    return Err(jqfb::invalid("a KEYTEXT node appears outside object key position"));
                }
                if entry.subtree_size != 1 {
                    return Err(jqfb::invalid("a KEYTEXT node must be a leaf"));
                }
                let offset = self.strg_offset(entry.payload)?;
                let raw = pool_bytes(chunks.strg, offset)?;
                let text = core::str::from_utf8(raw).map_err(|_| jqfb::invalid("a key pool entry is not UTF-8"))?;
                let stored = self.builder_mut()?.store_text(text, resources).map_err(map_data)?;
                let Some(Frame::Object {
                    pending_key, remaining, ..
                }) = self.frames.last_mut()
                else {
                    return Err(jqfb::invalid("a KEYTEXT node appears outside object key position"));
                };
                *pending_key = Some(stored);
                *remaining -= 1;
            }
            _ => return Err(CodecError::new(CodecFailureKind::UnsupportedRepresentation)),
        }
        Ok(true)
    }

    /// Attaches a completed node to its parent frame (or the root).
    fn attach(&mut self, node: NodeId, resources: &mut ResourceContext<'_>) -> Result<(), CodecError> {
        // Extract the parent's attachment facts before borrowing the builder.
        enum Attachment {
            Root,
            Array(NodeId),
            Object(NodeId, DocumentTextId),
            Tag(NodeId),
        }
        let attachment = match self.frames.last_mut() {
            None => Attachment::Root,
            Some(Frame::Array { owner, remaining, .. }) => {
                *remaining -= 1;
                Attachment::Array(*owner)
            }
            Some(Frame::Object {
                owner,
                pending_key,
                remaining,
                ..
            }) => {
                let key = pending_key
                    .take()
                    .ok_or_else(|| jqfb::invalid("an object value arrives before its key"))?;
                *remaining -= 1;
                Attachment::Object(*owner, key)
            }
            Some(Frame::Tag { owner, remaining, .. }) => {
                *remaining -= 1;
                Attachment::Tag(*owner)
            }
        };
        match attachment {
            Attachment::Root => {
                if self.root.is_some() {
                    return Err(jqfb::invalid("two root values"));
                }
                self.root = Some(node);
            }
            Attachment::Array(owner) => {
                self.builder_mut()?
                    .add_occurrence(
                        LocalOwnerRef::Node(owner),
                        provider::role_for("jqfb", "array"),
                        None,
                        node,
                        resources,
                    )
                    .map_err(map_data)?;
            }
            Attachment::Object(owner, key) => {
                self.builder_mut()?
                    .add_occurrence(
                        LocalOwnerRef::Node(owner),
                        provider::role_for("jqfb", "object"),
                        Some(AccountedOccurrenceKey::StoredText(key)),
                        node,
                        resources,
                    )
                    .map_err(map_data)?;
            }
            Attachment::Tag(owner) => {
                self.builder_mut()?
                    .add_occurrence(
                        LocalOwnerRef::Node(owner),
                        provider::role_for("jqfb", "tag-payload"),
                        None,
                        node,
                        resources,
                    )
                    .map_err(map_data)?;
            }
        }
        Ok(())
    }

    fn finish_scalar(
        &mut self,
        entry: NodeEntry,
        semantic: AccountedSemanticNode<'_>,
        table_index: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        let node = self.add_scalar(semantic, resources)?;
        self.note_scalar(entry, node, table_index, resources)
    }

    /// The single copy of the scalar leaf law — the leaf-size check, the node-table slot store, the span bind, and the
    /// attach — over an already-added node (`finish_scalar` adds first, then lands here).
    fn note_scalar(
        &mut self,
        entry: NodeEntry,
        node: NodeId,
        table_index: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        if entry.subtree_size != 1 {
            return Err(jqfb::invalid("a scalar node must be a leaf"));
        }
        self.node_ids[table_index] = Some(node);
        self.bind_node_span(node, table_index, resources)?;
        self.attach(node, resources)
    }

    fn add_scalar(
        &mut self,
        semantic: AccountedSemanticNode<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<NodeId, CodecError> {
        let builder = self.builder_mut()?;
        let node = builder
            .add_node(provider::kind_for("jqfb", &semantic), semantic, None, resources)
            .map_err(map_data)?;
        Ok(node)
    }

    /// Binds one document node's authored span: from its node-table entry through the image end. This is the T5 splice
    /// policy's span shape — a jqfb leaf's value bytes live in the pools and every chunk's position and digest is
    /// recorded in the footer directory, so the one contiguous span that can carry a splice's whole bookkeeping (the
    /// node entry, the pool entry, and the footer words) is the tail from the node's own entry through EOF. The scoped
    /// route binds no spans (the edit lane decodes through the whole-document route; the located document is a bare
    /// demand read).
    ///
    /// # Safety
    ///
    /// The span names bytes of the exact immutable source authority this session seals before publication, and the
    /// node's table entry lives there by construction (the node table was validated against the extent at `try_new`).
    fn bind_node_span(
        &mut self,
        node: NodeId,
        table_index: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        if self.scoped {
            return Ok(());
        }
        let file_index = self
            .start
            .checked_add(table_index)
            .ok_or_else(|| jqfb::invalid("node index overflows"))?;
        let entry = self
            .image
            .chunks
            .node
            .0
            .checked_add(
                file_index
                    .checked_mul(kinds::ENTRY_LEN)
                    .ok_or_else(|| jqfb::invalid("node index overflows"))?,
            )
            .ok_or_else(|| jqfb::invalid("node index overflows"))?;
        let span = jqf_source::Span::try_new(
            u32::try_from(entry).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?,
            u32::try_from(self.image.len).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?,
        )
        .ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?;
        // SAFETY: the span is this decoder's own byte arithmetic over the
        // exact source authority the session seals before publication.
        unsafe { self.builder_mut()?.record_authored_span(node, span, resources) }.map_err(map_data)
    }

    fn numb_offset(&self, payload: u32) -> Result<u32, CodecError> {
        self.image
            .numb_offsets
            .get(usize::try_from(payload).map_err(|_| jqfb::invalid("number pool index overflows"))?)
            .copied()
            .ok_or_else(|| jqfb::invalid("a number pool index exceeds the pool"))
    }

    fn strg_offset(&self, index: u32) -> Result<u32, CodecError> {
        self.image
            .strg_offsets
            .get(usize::try_from(index).map_err(|_| jqfb::invalid("string pool index overflows"))?)
            .copied()
            .ok_or_else(|| jqfb::invalid("a string pool index exceeds the pool"))
    }

    fn builder_mut(&mut self) -> Result<&mut AccountedDocumentBuilder<'static>, CodecError> {
        self.builder.as_mut().ok_or_else(|| {
            CodecError::new(CodecFailureKind::InternalContractViolation {
                contract: "jqfb builder present",
            })
        })
    }

    /// Attaches the FACT records, the SOUR fact, and the PROV fact, then finalizes. Runs EXACTLY ONCE, guarded by the
    /// caller (`finish_document` takes `root` and `builder`).
    fn finish_document(
        &mut self,
        chunks: &CoreChunks<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        let root = self.root.ok_or_else(data_contract)?;
        self.attach_facts(chunks.fact, resources)?;
        if !self.scoped
            && let Some(sour) = chunks.sour
            && let Some((label, origin)) = split_source_record(sour)?
        {
            self.builder_mut()?
                .add_fact(
                    LocalOwnerRef::Node(root),
                    provider::JQFB_SOURCE_FACT,
                    provider::JQFB_SOURCE_FACT,
                    1,
                    &FactPayload::OpaqueBytes(origin.to_vec()),
                    resources,
                )
                .map_err(map_data)?;
            let _ = label;
        }
        if !self.scoped
            && let Some(prov) = chunks.prov
            && let Some(payload) = parse_provenance(prov)?
        {
            self.builder_mut()?
                .add_fact(
                    LocalOwnerRef::Node(root),
                    provider::JQFT_PROVENANCE_FACT,
                    provider::JQFT_PROVENANCE_FACT,
                    1,
                    &payload,
                    resources,
                )
                .map_err(map_data)?;
        }
        Ok(())
    }

    /// Runs the document finalization for a SCOPED decode (FACT records whose owner sits in `[start, end)` attach;
    /// source and provenance stay whole-document) and hands the finished document to the caller's finalizer driver. The
    /// scoped session drives the finalizer itself so a large located subtree can yield across polls.
    pub(crate) fn finish_scoped_document(
        &mut self,
        chunks: &CoreChunks<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<AccountedDocumentFinalizer<'static>, CodecError> {
        self.finish_document(chunks, resources)?;
        let root = self.root.take().ok_or_else(data_contract)?;
        let builder = self.builder.take().ok_or_else(data_contract)?;
        builder.begin_finish(root, resources).map_err(map_data)
    }

    /// Attaches every FACT chunk record to its node.
    fn attach_facts(&mut self, fact: &[u8], resources: &mut ResourceContext<'_>) -> Result<(), CodecError> {
        // An ABSENT FACT chunk is zero facts. `locate_core` defaults the missing chunk to the empty slice; only foreign
        // or hand-built images hit this (the encoder always writes the chunk, even with no records), and absence is
        // tolerated upstream, so it finalizes as zero facts rather than a misleading truncation error.
        if fact.is_empty() {
            return Ok(());
        }
        // Two phases: read every record into owned tuples (the record read borrows `fact`), then attach (which borrows
        // the builder).
        let count = jqfb::read_u64(fact, 0).ok_or_else(|| jqfb::invalid("truncated FACT count"))?;
        let mut cursor = 8usize;
        let mut records: Vec<(NodeId, String, String, u32, FactPayload)> = Vec::new();
        for _ in 0..count {
            let node_index = jqfb::read_u32(fact, cursor).ok_or_else(|| jqfb::invalid("truncated fact node index"))?;
            cursor += 4;
            let (role, after) = jqfb::pool_entry(fact, cursor)?;
            let role = core::str::from_utf8(role).map_err(|_| jqfb::invalid("a fact role is not UTF-8"))?;
            cursor = after;
            let (kind, after) = jqfb::pool_entry(fact, cursor)?;
            let kind = core::str::from_utf8(kind).map_err(|_| jqfb::invalid("a fact kind is not UTF-8"))?;
            cursor = after;
            let revision = jqfb::read_u32(fact, cursor).ok_or_else(|| jqfb::invalid("truncated fact revision"))?;
            cursor += 4;
            let (payload, after) = read_fact_payload(fact, cursor)?;
            cursor = after;
            let abs = usize::try_from(node_index).map_err(|_| jqfb::invalid("fact node index overflows"))?;
            if abs < self.start || abs >= self.end {
                // The FACT chunk is whole-image. A scoped walk only built `[start, end)`; off-path facts stay with
                // their nodes.
                if self.scoped {
                    continue;
                }
                return Err(jqfb::invalid("a fact names an unknown node"));
            }
            let owner = self
                .node_ids
                .get(abs - self.start)
                .copied()
                .flatten()
                .ok_or_else(|| jqfb::invalid("a fact names an unknown node"))?;
            records.push((owner, role.to_owned(), kind.to_owned(), revision, payload));
        }
        for (owner, role, kind, revision, payload) in records {
            self.builder_mut()?
                .add_fact(LocalOwnerRef::Node(owner), &role, &kind, revision, &payload, resources)
                .map_err(map_data)?;
        }
        Ok(())
    }
}

impl AccessSession for JqfbDecodeState {
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
    fn decode<'source>(
        &mut self,
        input: AccessInput<'_, 'source>,
        context: &mut CodecRunContext<'_, '_>,
    ) -> Result<AccessResult<'source>, CodecError> {
        let AccessInput::Source(source) = input else {
            return Err(CodecError::new(CodecFailureKind::ProviderRouteMismatch));
        };
        // Re-slice the core chunks from the decode's source bytes (V7a: the decode reads the resolved bytes directly,
        // never an owned staging copy; the ranges were validated against the extent at `try_new`).
        let chunks = self.image.slice(source.bytes())?;
        if self.published {
            return Err(data_contract());
        }
        loop {
            match self.phase {
                Phase::Decode => {
                    if context.resources().admit_work_transition()? == WorkAdmission::Pending {
                        context.replenish_work()?;
                        continue;
                    }
                    let progress = self.decode_step(&chunks, context.resources())?;
                    if !progress {
                        // The whole-document decode bound authored spans (the T5 span law: every node's span runs to
                        // the image end), so the source is sealed before finalize; the scoped route binds no spans and
                        // skips the seal.
                        self.phase = if self.scoped { Phase::Finalize } else { Phase::Seal };
                    }
                }
                Phase::Seal => {
                    if self.binding_stage.is_none() {
                        // Hash off: every consumer of this binding reads through metadata-checked access (the edit
                        // lane's span arithmetic), never re-verifying the digest — exactly the cbor seal's contract.
                        self.binding_stage = Some(jqf_data::DocumentSourceBindingStage::new(source).map_err(map_data)?);
                    }
                    let stage = self.binding_stage.as_mut().ok_or_else(data_contract)?;
                    // SAFETY: codec-core retains one immutable source
                    // authority for the complete access session and passes that exact authority each poll; the stage
                    // was constructed over the same segment and re-verifies identity on every call.
                    match unsafe { stage.poll(source, context.resources()) }.map_err(map_data)? {
                        jqf_data::DocumentSourceBindingPoll::Pending => {
                            context.replenish_work()?;
                        }
                        jqf_data::DocumentSourceBindingPoll::Ready(binding) => {
                            self.binding_stage = None;
                            self.builder_mut()?.bind_source(binding).map_err(map_data)?;
                            self.phase = Phase::Finalize;
                        }
                    }
                }
                Phase::Finalize => {
                    // The document assembly (fact attachment + finalizer construction) runs EXACTLY ONCE: it takes
                    // `root` and `builder`, and re-entering it on a later pass — when the finalizer's own poll returns
                    // `Pending` and this phase is re-visited — would find `root` already taken and fail with a spurious
                    // contract violation (a jqfb image whose pool crosses the finalizer's per-pass budget decoded with
                    // `InternalContractViolation`).
                    if self.finalizer.is_none() {
                        self.finish_document(&chunks, context.resources())?;
                        let root = self.root.take().ok_or_else(data_contract)?;
                        let builder = self.builder.take().ok_or_else(data_contract)?;
                        self.finalizer = Some(builder.begin_finish(root, context.resources()).map_err(map_data)?);
                    }
                    let finalizer = self.finalizer.as_mut().ok_or_else(data_contract)?;
                    let poll = finalizer.poll(context.resources()).map_err(map_data)?;
                    let DocumentFinalizationPoll::Ready(document) = poll else {
                        context.replenish_work()?;
                        continue;
                    };
                    self.finalizer = None;
                    self.product = Some(DocumentProduct::try_new(document, context.resources())?);
                    self.phase = Phase::Publish;
                }
                Phase::Publish => {
                    let product = self.product.take().ok_or_else(data_contract)?;
                    let product = if self.scoped {
                        product
                    } else {
                        // SAFETY: codec-core owns this exact immutable source
                        // for the whole access session; the decoder sealed against it and every authored span names it.
                        unsafe { product.attach_borrowed_source_from_access_session(source, context.resources())? }
                    };
                    self.published = true;
                    let outcome = AccessOutcome::FullDocument(product);
                    return Ok(AccessResult::from_outcome(outcome));
                }
            }
        }
    }
}

/// The SOUR chunk record: a length-prefixed source label then the raw bytes.
fn split_source_record(sour: &[u8]) -> Result<Option<(&str, &[u8])>, CodecError> {
    if sour.is_empty() {
        return Ok(None);
    }
    let (label, after) = jqfb::pool_entry(sour, 0)?;
    let label = core::str::from_utf8(label).map_err(|_| jqfb::invalid("the source label is not UTF-8"))?;
    let origin = sour
        .get(after..)
        .ok_or_else(|| jqfb::invalid("the source record is truncated"))?;
    Ok(Some((label, origin)))
}

/// The PROV chunk record: codec, dialect, version, optional source label.
fn parse_provenance(prov: &[u8]) -> Result<Option<FactPayload>, CodecError> {
    let (codec, after) = jqfb::pool_entry(prov, 0)?;
    let codec = core::str::from_utf8(codec).map_err(|_| jqfb::invalid("the provenance codec is not UTF-8"))?;
    let (dialect, after) = jqfb::pool_entry(prov, after)?;
    let dialect = core::str::from_utf8(dialect).map_err(|_| jqfb::invalid("the provenance dialect is not UTF-8"))?;
    let (version, after) = jqfb::pool_entry(prov, after)?;
    let version = core::str::from_utf8(version).map_err(|_| jqfb::invalid("the provenance version is not UTF-8"))?;
    let mut fields: Vec<(String, FactPayload)> = Vec::new();
    fields.push(("codec".into(), FactPayload::Text(codec.into())));
    fields.push(("dialect".into(), FactPayload::Text(dialect.into())));
    fields.push(("version".into(), FactPayload::Text(version.into())));
    if let Ok((source, _)) = jqfb::pool_entry(prov, after)
        && let Ok(source) = core::str::from_utf8(source)
    {
        fields.push(("source".into(), FactPayload::Text(source.into())));
    }
    Ok(Some(FactPayload::Map(fields)))
}

/// Reads one serialized fact payload; returns the payload and the next offset.
///
/// List and map nesting walks an explicit stack so attacker-chosen depth costs heap, never call stack.
#[allow(
    clippy::too_many_lines,
    reason = "one payload-tag table: every fact tag's law sits beside the others"
)]
pub(crate) fn read_fact_payload(bytes: &[u8], offset: usize) -> Result<(FactPayload, usize), CodecError> {
    enum Frame {
        List {
            items: Vec<FactPayload>,
            remaining: u64,
        },
        Map {
            entries: Vec<(String, FactPayload)>,
            remaining: u64,
            pending_key: Option<String>,
        },
    }

    let mut cursor = offset;
    let mut stack: Vec<Frame> = Vec::new();
    loop {
        if let Some(Frame::Map {
            pending_key, remaining, ..
        }) = stack.last_mut()
            && pending_key.is_none()
            && *remaining > 0
        {
            let (key, after) = jqfb::pool_entry(bytes, cursor)?;
            let key = core::str::from_utf8(key).map_err(|_| jqfb::invalid("a fact map key is not UTF-8"))?;
            cursor = after;
            *pending_key = Some(String::from(key));
        }

        let tag = *bytes
            .get(cursor)
            .ok_or_else(|| jqfb::invalid("truncated fact payload tag"))?;
        cursor += 1;
        let payload = match tag {
            0 => FactPayload::Null,
            1 => {
                let value = *bytes.get(cursor).ok_or_else(|| jqfb::invalid("truncated fact bool"))?;
                cursor += 1;
                FactPayload::Bool(value != 0)
            }
            2 => {
                let (text, after) = jqfb::pool_entry(bytes, cursor)?;
                let text = core::str::from_utf8(text).map_err(|_| jqfb::invalid("a fact integer is not UTF-8"))?;
                let integer =
                    jqf_data::Integer::parse(text).map_err(|_| jqfb::invalid("a fact integer does not parse"))?;
                cursor = after;
                FactPayload::Integer(integer)
            }
            3 => {
                let (coef, after) = jqfb::pool_entry(bytes, cursor)?;
                let coef = core::str::from_utf8(coef).map_err(|_| jqfb::invalid("a fact decimal is not UTF-8"))?;
                let scale =
                    jqfb::read_u64(bytes, after).ok_or_else(|| jqfb::invalid("truncated fact decimal scale"))?;
                let coefficient =
                    jqf_data::Integer::parse(coef).map_err(|_| jqfb::invalid("a fact decimal does not parse"))?;
                cursor = after + 8;
                FactPayload::Decimal(
                    jqf_data::Decimal::from_literal_parts(coefficient, i64::from_ne_bytes(scale.to_ne_bytes()))
                        .map_err(|_| jqfb::invalid("a fact decimal is out of range"))?,
                )
            }
            4 => {
                let (text, after) = jqfb::pool_entry(bytes, cursor)?;
                let text = core::str::from_utf8(text).map_err(|_| jqfb::invalid("a fact text is not UTF-8"))?;
                cursor = after;
                FactPayload::Text(text.into())
            }
            5 => {
                let (raw, after) = jqfb::pool_entry(bytes, cursor)?;
                cursor = after;
                FactPayload::Bytes(raw.to_vec())
            }
            6 => {
                let count = jqfb::read_u64(bytes, cursor).ok_or_else(|| jqfb::invalid("truncated fact list count"))?;
                cursor += 8;
                if count == 0 {
                    FactPayload::List(Vec::new())
                } else {
                    stack.push(Frame::List {
                        items: Vec::new(),
                        remaining: count,
                    });
                    continue;
                }
            }
            7 => {
                let count = jqfb::read_u64(bytes, cursor).ok_or_else(|| jqfb::invalid("truncated fact map count"))?;
                cursor += 8;
                if count == 0 {
                    FactPayload::Map(Vec::new())
                } else {
                    stack.push(Frame::Map {
                        entries: Vec::new(),
                        remaining: count,
                        pending_key: None,
                    });
                    continue;
                }
            }
            8 => {
                let (raw, after) = jqfb::pool_entry(bytes, cursor)?;
                cursor = after;
                FactPayload::OpaqueBytes(raw.to_vec())
            }
            _ => return Err(jqfb::invalid("unknown fact payload tag")),
        };

        let mut finished = payload;
        loop {
            match stack.last_mut() {
                None => return Ok((finished, cursor)),
                Some(Frame::List { items, remaining }) => {
                    items.push(finished);
                    *remaining -= 1;
                    if *remaining > 0 {
                        break;
                    }
                    match stack.pop() {
                        Some(Frame::List { items, .. }) => finished = FactPayload::List(items),
                        _ => return Err(jqfb::invalid("fact list frame")),
                    }
                }
                Some(Frame::Map {
                    entries,
                    remaining,
                    pending_key,
                }) => {
                    let key = pending_key
                        .take()
                        .ok_or_else(|| jqfb::invalid("a fact map value arrives before its key"))?;
                    entries.push((key, finished));
                    *remaining -= 1;
                    if *remaining > 0 {
                        break;
                    }
                    match stack.pop() {
                        Some(Frame::Map { entries, .. }) => finished = FactPayload::Map(entries),
                        _ => return Err(jqfb::invalid("fact map frame")),
                    }
                }
            }
        }
    }
}

fn chunk_payload<'a>(footer: &Footer, entry: &DirectoryEntry, bytes: &'a [u8]) -> Result<&'a [u8], CodecError> {
    let _ = footer;
    bytes
        .get(entry.offset..entry.end())
        .ok_or_else(|| jqfb::invalid("a chunk extent lies outside the file"))
}

fn map_data(error: DataError) -> CodecError {
    match error {
        DataError::Resource(error) => error.into(),
        DataError::Control(error) => error.into(),
        DataError::ArithmeticOverflow => CodecError::new(CodecFailureKind::Overflow),
        DataError::Allocation => CodecError::new(CodecFailureKind::AllocationFailure),
        DataError::InvalidDocument => CodecError::new(CodecFailureKind::InvalidInput),
        _ => CodecError::new(CodecFailureKind::InternalContractViolation {
            contract: "jqfb document construction",
        }),
    }
}

fn data_contract() -> CodecError {
    CodecError::new(CodecFailureKind::InternalContractViolation {
        contract: "jqfb authoritative document construction",
    })
}

/// A pool chunk's leading count word.
const POOL_COUNT_LEN: usize = 8;
/// The smallest STRG pool entry: its own 4-byte length word alone (a zero-length payload).
const STRG_MIN_ENTRY_LEN: usize = 4;
/// The smallest NUMB pool entry: the tag byte plus the shortest tagged body (a length word over a zero-length payload,
/// tag 0).
const NUMB_MIN_ENTRY_LEN: usize = 5;

/// The end offset of one number-pool entry (tag-dependent extent).
fn number_body(numb: &[u8], offset: u32) -> Result<&[u8], CodecError> {
    let end = number_entry_end(numb, offset as usize)?;
    numb.get(offset as usize..end)
        .ok_or_else(|| jqfb::invalid("a number pool entry exceeds the chunk"))
}

fn pool_bytes(strg: &[u8], offset: u32) -> Result<&[u8], CodecError> {
    let (bytes, _) = jqfb::pool_entry(strg, offset as usize)?;
    Ok(bytes)
}

pub(crate) fn number_entry_end(numb: &[u8], offset: usize) -> Result<usize, CodecError> {
    match *numb
        .get(offset)
        .ok_or_else(|| jqfb::invalid("truncated number entry tag"))?
    {
        0 => {
            let (_, after) = jqfb::pool_entry(numb, offset + 1)?;
            Ok(after)
        }
        1 => {
            let (_, after) = jqfb::pool_entry(numb, offset + 1)?;
            let end = after
                .checked_add(8)
                .ok_or_else(|| jqfb::invalid("a decimal scale word overflows"))?;
            if end > numb.len() {
                return Err(jqfb::invalid("truncated decimal scale"));
            }
            Ok(end)
        }
        2 => {
            let end = offset
                .checked_add(9)
                .ok_or_else(|| jqfb::invalid("a float entry overflows"))?;
            if end > numb.len() {
                return Err(jqfb::invalid("truncated float bits"));
            }
            Ok(end)
        }
        _ => Err(jqfb::invalid("unknown number pool tag")),
    }
}

/// Builds the STRG pool's entry-offset table: the byte offset (from the chunk's start, past the 8-byte count word) of
/// each entry's length word, validated against the chunk extent as it walks. Built once per session in `try_new` — this
/// is the walk V3b de-quadraticized.
fn pool_offsets(pool: &[u8], count: usize) -> Result<Vec<u32>, CodecError> {
    // The extent precheck BEFORE the allocation: every STRG entry occupies at least its own 4-byte length word, so a
    // count above the chunk's entry capacity is malformed no matter what the entries contain. This is the pool-side
    // twin of the footer's extent-vs-count check (`read_footer`) — without it an attacker-chosen count asks
    // `with_capacity` for an absurd (or overflowing) allocation.
    if count > pool.len().saturating_sub(POOL_COUNT_LEN) / STRG_MIN_ENTRY_LEN {
        return Err(jqfb::invalid("a string pool count exceeds its chunk"));
    }
    let mut offsets = Vec::with_capacity(count);
    let mut offset = 8usize;
    for _ in 0..count {
        let index = u32::try_from(offset).map_err(|_| jqfb::invalid("a pool exceeds 4 GiB"))?;
        offsets.push(index);
        let (_, next) = jqfb::pool_entry(pool, offset)?;
        offset = next;
    }
    Ok(offsets)
}

/// Builds the NUMB pool's entry-offset table. The number pool's entries are tag-led with tag-dependent extents, so the
/// advance is `number_entry_end`, not the length-word read `pool_offsets` uses.
fn number_offsets(numb: &[u8], count: usize) -> Result<Vec<u32>, CodecError> {
    // Same precheck as `pool_offsets`, against the number pool's smallest possible entry (the tag byte plus the
    // shortest tagged body). A count beyond that capacity is malformed before the walk proves it.
    if count > numb.len().saturating_sub(POOL_COUNT_LEN) / NUMB_MIN_ENTRY_LEN {
        return Err(jqfb::invalid("a number pool count exceeds its chunk"));
    }
    let mut offsets = Vec::with_capacity(count);
    let mut offset = 8usize;
    for _ in 0..count {
        let index = u32::try_from(offset).map_err(|_| jqfb::invalid("a pool exceeds 4 GiB"))?;
        offsets.push(index);
        offset = number_entry_end(numb, offset)?;
    }
    Ok(offsets)
}

#[cfg(test)]
mod tests {
    use super::read_fact_payload;
    use jqf_data::FactPayload;

    #[test]
    fn nested_fact_lists_do_not_overflow_the_stack() {
        let depth = 20_000usize;
        let mut bytes = Vec::with_capacity(depth * 9 + 1);
        for _ in 0..depth {
            bytes.push(6);
            bytes.extend_from_slice(&1u64.to_le_bytes());
        }
        bytes.push(0);
        let (mut payload, rest) = read_fact_payload(&bytes, 0).expect("iterative nest");
        assert_eq!(rest, bytes.len());
        for _ in 0..depth {
            match payload {
                FactPayload::List(mut items) => {
                    assert_eq!(items.len(), 1);
                    payload = items.pop().expect("child");
                }
                other => panic!("expected list, got {other:?}"),
            }
        }
        assert!(matches!(payload, FactPayload::Null));
    }

    #[test]
    fn empty_fact_list_and_map_round_trip_the_tag() {
        let (list, n) = read_fact_payload(&[6, 0, 0, 0, 0, 0, 0, 0, 0], 0).expect("empty list");
        assert_eq!(n, 9);
        assert!(matches!(list, FactPayload::List(items) if items.is_empty()));
        let (map, n) = read_fact_payload(&[7, 0, 0, 0, 0, 0, 0, 0, 0], 0).expect("empty map");
        assert_eq!(n, 9);
        assert!(matches!(map, FactPayload::Map(entries) if entries.is_empty()));
    }
}
