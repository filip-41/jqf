//! Build one document, charging the request ledger.
//!
//! [`AccountedDocumentBuilder`] admits nodes, occurrences, and facts. Each admission checks its inputs and binds schema
//! names. Finish with [`AccountedDocumentBuilder::finish`] in one shot, or with
//! [`super::publish::AccountedDocumentFinalizer`] in cooperative polls.

use alloc::{string::String, sync::Arc, vec::Vec};
use core::marker::PhantomData;

#[cfg(feature = "benchmark-internals")]
use super::SchemaExecution;
use super::publish::{
    AccountedDocumentFinalizer, CoverageEvidence, FinalizationPhase, copy_accounted_local_date_time,
    copy_accounted_local_time, copy_accounted_offset_date_time, published_coverage,
};
use super::storage::StorageRange;
use super::{
    AccountedSchemaBuilder, AuthoritativeEmptyFamilies, BuilderCoverage, DataError, DiagnosticCoverage,
    DialectBindingId, Document, DocumentId, DocumentSchema, DocumentSchemaPrototype, DocumentSchemaPrototypeId,
    DocumentSchemaRecipe, DocumentSourceText, DocumentStorage, DocumentStorageOwner, DocumentTextId, FactId,
    FactPayload, FormatBindingId, IntrinsicTag, IntrinsicTagRef, LocalOwnerRef, NodeId, NodeRecord, NodeSemantic,
    OccurrenceId, OccurrenceRecord, OccurrenceRoleBindingId, PlacedSemantic, PreparedDocumentSchema, PreparedNodeKind,
    PreparedOccurrenceRole, StoredDocumentFact, StoredFactPayload, StoredOccurrenceKey, StoredSemanticNode,
    WidePayload, WidePayloadId, place_semantic,
};
use jqf_resource::ResourceContext;

/// Classifies a freshly built payload and pushes it to the `wide` side arena when the payload is large or uncommon,
/// returning the header semantic.
#[inline]
fn prepare_node_semantic(wide: &mut Vec<WidePayload>, semantic: StoredSemanticNode) -> Result<NodeSemantic, DataError> {
    match place_semantic(semantic) {
        PlacedSemantic::Inline(node_semantic) => Ok(node_semantic),
        PlacedSemantic::Wide(payload) => {
            let kind = payload.kind();
            let id = WidePayloadId(u32::try_from(wide.len()).map_err(|_| DataError::ArithmeticOverflow)?);
            // The wide side arena grows with wide-payload count: reserve fallibly so a decode that would cross the
            // ceiling refuses at the growth instead of aborting.
            wide.try_reserve(1).map_err(jqf_resource::ResourceError::from)?;
            wide.push(payload);
            Ok(NodeSemantic::Wide { id, kind })
        }
    }
}

/// Pushes an optional intrinsic tag to the `tags` side arena, returning the compact header reference.
fn prepare_intrinsic_tag(
    tags: &mut Vec<IntrinsicTag>,
    tag: Option<IntrinsicTag>,
) -> Result<IntrinsicTagRef, DataError> {
    match tag {
        None => Ok(IntrinsicTagRef::NONE),
        Some(tag) => {
            let tagged = tag.semantics() == super::IntrinsicTagSemantics::Tagged;
            let index = u32::try_from(tags.len()).map_err(|_| DataError::ArithmeticOverflow)?;
            let reference = IntrinsicTagRef::present(index, tagged)?;
            // The tags side arena grows with tag count: reserve fallibly so a decode that would cross the ceiling
            // refuses instead of aborting.
            tags.try_reserve(1).map_err(jqf_resource::ResourceError::from)?;
            tags.push(tag);
            Ok(reference)
        }
    }
}

struct PreparedStoredText {
    span: jqf_source::Span,
    key: DocumentId,
    generation: u64,
}

impl PreparedStoredText {
    fn text_ref(&self) -> super::TextRef {
        super::TextRef::Stored(self.span)
    }

    fn commit(self) -> DocumentTextId {
        DocumentTextId::new_accounted(self.span, self.key, self.generation)
    }
}

fn prepare_stored_text(
    text: &mut String,
    value: &str,
    key: DocumentId,
    generation: u64,
) -> Result<PreparedStoredText, DataError> {
    let start = text.len();
    let end = start.checked_add(value.len()).ok_or(DataError::ArithmeticOverflow)?;
    let range = jqf_source::Span::try_new(
        u32::try_from(start).map_err(|_| DataError::ArithmeticOverflow)?,
        u32::try_from(end).map_err(|_| DataError::ArithmeticOverflow)?,
    )
    .ok_or(DataError::ArithmeticOverflow)?;
    // The compact text arena grows with stored text: reserve fallibly so a decode that would cross the ceiling refuses
    // instead of aborting: the arena is one of the input-driven accumulation surfaces.
    text.try_reserve(value.len())
        .map_err(jqf_resource::ResourceError::from)?;
    text.push_str(value);
    Ok(PreparedStoredText {
        span: range,
        key,
        generation,
    })
}

/// Additional capacity requested for one document construction phase.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DocumentCapacity {
    /// Additional semantic/topology nodes.
    pub nodes: usize,
    /// Additional ordered topology occurrences.
    pub occurrences: usize,
    /// Additional decoded UTF-8 bytes.
    pub stored_text_bytes: usize,
    /// Additional attached facts.
    pub facts: usize,
}

/// Opaque continuation for one incrementally copied text entry.
#[derive(Debug)]
pub struct AccountedTextStage {
    key: DocumentId,
    builder_generation: u64,
    stage_generation: u64,
    start: usize,
    cursor: usize,
}

/// Node payload accepted by [`AccountedDocumentBuilder`].
#[derive(Clone, Copy, Debug)]
pub enum AccountedSemanticNode<'input> {
    /// Null scalar.
    Null,
    /// Boolean scalar.
    Bool(bool),
    /// Canonical signed base-ten integer text copied into owned storage.
    Integer(&'input str),
    /// Canonical exact decimal coefficient and scale copied into owned storage.
    Decimal {
        /// Canonical signed coefficient.
        coefficient: &'input str,
        /// Base-ten scale.
        scale: i64,
    },
    /// Exact inline binary64 payload.
    Float(crate::Float),
    /// UTF-8 scalar copied into owned document text storage.
    String(&'input str),
    /// Bytes copied into owned retained storage.
    Bytes(&'input [u8]),
    /// Existing validated retained-source text.
    SourceString(DocumentSourceText),
    /// Local calendar date retained inline.
    LocalDate(crate::LocalDate),
    /// Local wall-clock time copied into accounted temporal storage.
    LocalTime(&'input crate::LocalTime),
    /// Local date and time copied into accounted temporal storage.
    LocalDateTime(&'input crate::LocalDateTime),
    /// Offset date and time copied into accounted temporal storage.
    OffsetDateTime(&'input crate::OffsetDateTime),
    /// Array projection using this topology role.
    Array {
        /// Role selecting array-item occurrences.
        item_role: &'input str,
    },
    /// Object projection using this topology role.
    Object {
        /// Role selecting object-member occurrences.
        member_role: &'input str,
    },
    /// Topology only. Materializing this node fails.
    Unrepresentable,
}

impl<'input> AccountedSemanticNode<'input> {
    fn kind(self) -> Option<crate::ValueKind> {
        Some(match self {
            Self::Null => crate::ValueKind::Null,
            Self::Bool(_) => crate::ValueKind::Bool,
            Self::Integer(_) | Self::Decimal { .. } | Self::Float(_) => crate::ValueKind::Number,
            Self::String(_) | Self::SourceString(_) => crate::ValueKind::String,
            Self::Bytes(_) => crate::ValueKind::Bytes,
            Self::LocalDate(_) => crate::ValueKind::LocalDate,
            Self::LocalTime(_) => crate::ValueKind::LocalTime,
            Self::LocalDateTime(_) => crate::ValueKind::LocalDateTime,
            Self::OffsetDateTime(_) => crate::ValueKind::OffsetDateTime,
            Self::Array { .. } => crate::ValueKind::Array,
            Self::Object { .. } => crate::ValueKind::Object,
            Self::Unrepresentable => return None,
        })
    }

    fn role(self) -> Option<&'input str> {
        match self {
            Self::Array { item_role } => Some(item_role),
            Self::Object { member_role } => Some(member_role),
            _ => None,
        }
    }
}

/// Same as [`AccountedSemanticNode`], with schema ids already interned.
#[derive(Clone, Copy, Debug)]
pub enum PreparedSemanticNode {
    /// Null scalar.
    Null,
    /// Boolean scalar.
    Bool(bool),
    /// Exact inline binary64 payload.
    Float(crate::Float),
    /// Array projection selected by a prepared occurrence role.
    Array(PreparedOccurrenceRole),
    /// Object projection selected by a prepared occurrence role.
    Object(PreparedOccurrenceRole),
}

enum PreparedNodeSemantic {
    Ready(StoredSemanticNode),
    StoredString,
    StoredInteger,
    StoredDecimal { scale: i64 },
    Array,
    Object,
}

/// Borrowed occurrence-key input accepted by [`AccountedDocumentBuilder`].
#[derive(Debug)]
pub enum AccountedOccurrenceKey<'input> {
    /// UTF-8 key copied into owned document text storage.
    Text(&'input str),
    /// Existing text completed through the builder's staged text API.
    StoredText(DocumentTextId),
    /// Existing validated retained-source text.
    SourceText(DocumentSourceText),
}

/// Borrowed intrinsic-tag input accepted by [`AccountedDocumentBuilder`].
#[derive(Clone, Copy, Debug)]
pub enum AccountedIntrinsicTag<'input> {
    /// Resolved core tag agreeing with the supplied category.
    Core {
        /// Exact format-neutral resolved tag.
        tag: &'input str,
        /// Required core semantic category.
        kind: crate::ValueKind,
    },
    /// Non-core tag wrapping the semantic payload.
    Tagged(&'input str),
}

/// A deferred position-counter store: the counter must not advance until the occurrence it stamps is itself committed,
/// so a failed occurrence push can never consume a position.
struct PositionCommit<'counter> {
    next: &'counter mut u32,
    replacement: u32,
}

impl PositionCommit<'_> {
    #[inline]
    fn commit(self) {
        *self.next = self.replacement;
    }
}

/// Reads the `(owner, role)` position counter and prepares its bump.
///
/// The counter is one cell of the dense per-role ledger; the returned position is the count of occurrences already
/// stamped for the pair, and the commit advances it by one. A counter is `u32` — the position of one role on one
/// owner is bounded by the occurrence count of that single pair, and an overflow past 2^32-1 raises the same
/// `ArithmeticOverflow` the previous `u64` counter raised, at a boundary no shipped document approaches.
#[inline]
fn prepare_accounted_position(counter: &mut u32) -> Result<(u32, PositionCommit<'_>), DataError> {
    let position = *counter;
    let replacement = position.checked_add(1).ok_or(DataError::ArithmeticOverflow)?;
    Ok((
        position,
        PositionCommit {
            next: counter,
            replacement,
        },
    ))
}

/// Prepares the position for one occurrence: grows the `(owner, role)` counter cell's table when it is missing (the
/// fallible growth is the refusal point), resolves the cell in the dense per-role ledger, and reads it for the bump —
/// reservation and resolution walk the tables ONCE.
///
/// Counters are authoring scratch only an owner reads, and most nodes are leaves that own nothing, so the ledger grows
/// from the occurrence side — charging only the prefix reaching the highest-indexed owner — in a dense per-role
/// layout of 4 bytes per (role, node) cell. The growth may run before the transaction it serves commits: an absent
/// table or cell reads the same as a zero counter, so a later rollback of the staged occurrence leaves nothing to undo
/// here; the cells stay charged to the builder's account and are released with the tables, which `finish` drops before
/// the arena pass. The two ledger fields are passed separately so the returned [`PositionCommit`] borrows only the one
/// table and the occurrence push against the disjoint `occurrences` field can proceed while it is live.
#[inline]
fn prepare_owner_position<'tables>(
    owner: LocalOwnerRef,
    role: OccurrenceRoleBindingId,
    node_tables: &'tables mut Vec<Vec<u32>>,
    root_positions: &'tables mut Vec<u32>,
) -> Result<(u32, PositionCommit<'tables>), DataError> {
    match owner {
        LocalOwnerRef::DocumentRoot => {
            let required = role.index().checked_add(1).ok_or(DataError::ArithmeticOverflow)?;
            grow_position_table(root_positions, required)?;
            let counter = root_positions
                .as_mut_slice()
                .get_mut(role.index())
                .ok_or(DataError::InvalidDocument)?;
            prepare_accounted_position(counter)
        }
        LocalOwnerRef::Node(node) => {
            let required = node.index().checked_add(1).ok_or(DataError::ArithmeticOverflow)?;
            while node_tables.len() <= role.index() {
                node_tables.push(Vec::new());
            }
            let table = node_tables
                .as_mut_slice()
                .get_mut(role.index())
                .ok_or(DataError::InvalidDocument)?;
            if table.len() < required {
                grow_position_table(table, required)?;
            }
            let counter = table
                .as_mut_slice()
                .get_mut(node.index())
                .ok_or(DataError::InvalidNode)?;
            prepare_accounted_position(counter)
        }
        LocalOwnerRef::Occurrence(_) => Err(DataError::InvalidDocument),
    }
}

/// Builds one document, charging the request ledger.
pub struct AccountedDocumentBuilder<'source> {
    /// Dynamic-schema intern tables. `None` on a prepared-route builder: identities come from `shared_schema` and this
    /// builder never mints a name.
    pub(super) schema: Option<AccountedSchemaBuilder>,
    pub(super) shared_schema: Option<Arc<DocumentSchema>>,
    pub(super) schema_prototype: Option<DocumentSchemaPrototypeId>,
    #[cfg(feature = "benchmark-internals")]
    pub(super) schema_execution: SchemaExecution,
    pub(super) format: FormatBindingId,
    pub(super) dialect: Option<DialectBindingId>,
    pub(super) key: DocumentId,
    pub(super) generation: u64,
    pub(super) next_stage_generation: u64,
    pub(super) active_text_stage: Option<(u64, usize)>,
    pub(super) source_binding: Option<super::DocumentSourceBinding>,
    /// Keeps the finalizer's source lifetime tied to this builder without retaining source bytes during construction.
    pub(super) _source: PhantomData<&'source ()>,
    /// Farthest end offset a bound source span was admitted at while no seal authenticated it yet, in the coordinates
    /// of the seal that will.
    ///
    /// A single-pass codec decoding one value of an adjacent-value stream learns the extent its document names only
    /// when the value ENDS — the late-sealing order, see [`AccountedDocumentBuilder::bind_source`]. Retaining the
    /// farthest end lets [`AccountedDocumentBuilder::bind_source`] prove in constant time — without revisiting a node
    /// — that the arriving seal covers every span already admitted, which is the same containment the per-span range
    /// check proves when the seal is bound first. Binding clears it, so a builder that still carries one at finish
    /// admitted a span no seal ever authenticated and is rejected.
    pub(super) unsealed_source_span_end: Option<u32>,
    /// Call-local source view installed only by `poll_with_source`.
    ///
    /// The source guard clears this slot on every return from the guarded publication poll, including `Pending`, so it
    /// never survives the call that installed it. The pointer is a call-local view of already-borrowed bytes; as a raw
    /// pointer it makes the builder `!Send` (and `!Sync`), which is intended: a builder never crosses a thread
    /// mid-build.
    pub(super) finalization_source: Option<(*const u8, usize)>,
    pub(super) demanded_coverage: BuilderCoverage,
    pub(super) empty_families: AuthoritativeEmptyFamilies,
    pub(super) diagnostics: DiagnosticCoverage,
    pub(super) stored_text: String,
    /// Whether the staged text arena is currently DETACHED into a decoder's streaming window
    /// ([`Self::take_staged_text`]): the decoder appends decoded bytes directly and hands the arena back through
    /// [`Self::restore_staged_text`] before any other text operation. While set, every text-entry operation refuses, so
    /// the arena cannot fork.
    staged_text_detached: bool,
    pub(super) nodes: Vec<NodeRecord>,
    pub(super) wide: Vec<WidePayload>,
    pub(super) tags: Vec<IntrinsicTag>,
    pub(super) occurrences: Vec<OccurrenceRecord>,
    /// Dense per-role occurrence-position counters, indexed `[role][node]`: the cell is the count of `(node, role)`
    /// occurrences already stamped, which is the position the next one stores. Authoring scratch only — released at
    /// the first finalize poll (and at `finish`) before the finalizer's peak, so its 4 B/node per role never sits under
    /// the relationship-arena pass.
    pub(super) owner_positions: Vec<Vec<u32>>,
    /// Document-root-owned occurrence-position counters, dense by role id.
    pub(super) root_positions: Vec<u32>,
    pub(super) facts: Vec<StoredDocumentFact>,
    /// Format-owned reader installed by the decoder that commits container spans; `None` for every eagerly built
    /// document.
    pub(super) span_materializer: Option<&'static dyn super::LazySpanMaterializer>,
    /// How many container spans this build committed.
    pub(super) container_spans: u32,
    /// Last-wins Exact cache for committed container spans, in node order. Empties into
    /// [`super::storage::DocumentStorage::span_cache`] at finish.
    pub(super) span_cache: Vec<super::storage::SpanCache>,
    /// Authored source spans recorded out-of-band for scalars whose retained semantic carries no span (codec floats,
    /// decimals, booleans), in node order. Empties into [`super::storage::DocumentStorage::authored_spans`] at finish;
    /// a document with no such scalar pays nothing.
    pub(super) authored_spans: Vec<super::storage::AuthoredSpanRecord>,
    /// Per-element node records awaiting one sequential per-arena flush.
    ///
    /// The staging scratch: authoring pushes node/occurrence records here and flushes each arena's accumulated block as
    /// one write run, so at most two append streams are live at any instant instead of the interleaved per-element
    /// streams the virtualized guest degrades on (its stream handling collapses between four and eight live streams).
    /// The committed arena `len` (the physical length) advances only on flush, while ids, validation, and the public
    /// counts read the LOGICAL length (`len + staged`), so the staged window is invisible to every reader and no slot
    /// is ever unwritten-and-counted (the `Drop` path runs element destructors over `0..len`). The scratch is one arena
    /// like the rest, holds at most `STAGE_BLOCK` records, and drops with the builder when no flush remains.
    pub(super) staged_nodes: Vec<NodeRecord>,
    /// Per-element occurrence records awaiting one sequential per-arena flush; see [`Self::staged_nodes`] for the
    /// staging law.
    pub(super) staged_occurrences: Vec<OccurrenceRecord>,
}

impl DocumentSchemaPrototype {
    /// Starts one fresh accounted document using this immutable schema while retaining only the requested side-data
    /// coverage.
    ///
    /// Construction has no source bytes and does not take a resource context. Bind source later with
    /// [`AccountedDocumentBuilder::bind_source`].
    pub fn try_new_builder_with_coverage<'source>(
        &self,
        coverage: BuilderCoverage,
    ) -> Result<(AccountedDocumentBuilder<'source>, PreparedDocumentSchema), DataError> {
        AccountedDocumentBuilder::try_new_from_prototype(self, coverage)
    }
}

/// The staging block size in element records: how many node/occurrence records accumulate in the scratch before one
/// per-arena flush runs.
///
/// A measured sweep of staging sizes (32 -> 512 -> 1024, monotone) put the win in LONGER flush runs rather than in the
/// scratch's footprint, so the block is sized at the sweep's largest end; 1024 records of the two compact arenas (~96
/// KiB total scratch) still fit the M-series L1 alongside the decode's working set, and the 512 -> 1024 step measured a
/// further ~1-2% on the container decode-build lane.
const STAGE_BLOCK: usize = 1024;

/// Appends zero counters up to `required` for one role's table.
///
/// Depth-first authoring reaches a new owner only when it opens a container, so the gap closed here is the run of
/// leaves the previous container held — bounded, and paid once per container rather than once per node. Only the role
/// actually being stamped grows, so a document whose roles appear late never pays to extend tables it will not read.
#[inline(never)]
fn grow_position_table(table: &mut Vec<u32>, required: usize) -> Result<(), DataError> {
    let additional = required.saturating_sub(table.len());
    if additional > 0 {
        // The per-role position tables grow with the node count of their owner: reserve fallibly so a decode that would
        // cross the ceiling refuses at the growth instead of aborting.
        table
            .try_reserve(additional)
            .map_err(jqf_resource::ResourceError::from)?;
        table.resize(table.len() + additional, 0);
    }
    Ok(())
}

/// Stages one record in an L1-resident scratch block and reports whether the block has filled.
#[inline]
fn stage_record<T>(staged: &mut Vec<T>, record: T) -> Result<bool, DataError> {
    // The staged block is bounded in length by STAGE_BLOCK, but its Vec doubling can still cross a tight ceiling:
    // reserve fallibly so a decode that would cross refuses instead of aborting.
    staged.try_reserve(1).map_err(jqf_resource::ResourceError::from)?;
    staged.push(record);
    Ok(staged.len() >= STAGE_BLOCK)
}

/// Appends one staged block to its arena as a single sequential write run, leaving the block STAGED when the growth is
/// refused.
///
/// The order is the whole of it: the reserve is fallible and the staged records are moved only after it succeeds. Every
/// staged record already owns an id the caller is holding, so a block emptied on a refusal would leave a hole that the
/// next stage silently fills — every outstanding id would then name a different record.
fn flush_arena<T>(arena: &mut Vec<T>, staged: &mut Vec<T>) -> Result<(), DataError> {
    if staged.is_empty() {
        return Ok(());
    }
    arena
        .try_reserve(staged.len())
        .map_err(jqf_resource::ResourceError::from)?;
    // take moves the staging allocation into the arena (extend's IntoIter specialization), which also RELEASES the
    // scratch capacity: retaining it across flushes pins one ~2 MiB staging block per live builder and breaches
    // document-build rss ceilings. The next stage's scratch is cheap to grow.
    arena.extend(core::mem::take(staged));
    Ok(())
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "origin-bound text tokens and stages are deliberately consumed so stale authority cannot be reused"
)]
impl<'source> AccountedDocumentBuilder<'source> {
    /// Binds a validated static schema recipe and starts one accounted document that physically retains only the
    /// demanded side-data coverage.
    pub fn try_new_prepared_with_coverage(
        recipe: &DocumentSchemaRecipe<'_>,
        coverage: BuilderCoverage,
    ) -> Result<(Self, PreparedDocumentSchema), DataError> {
        let mut builder = Self::try_new_with_coverage(recipe.format(), recipe.dialect(), coverage)?;
        let prepared = PreparedDocumentSchema::try_new(
            builder.key,
            recipe,
            builder.schema.as_mut().ok_or(DataError::InvalidDocument)?,
        )?;
        builder.schema_prototype = Some(prepared.prototype_identity());
        #[cfg(feature = "benchmark-internals")]
        builder.schema_execution.bind_prepared(recipe.fingerprint());
        Ok((builder, prepared))
    }

    /// Start a document with no source bytes yet. Bind source later with `bind_source`. Construction does not take a
    /// resource context.
    pub fn try_new(format: &str, dialect: Option<&str>) -> Result<Self, DataError> {
        Self::try_new_with_coverage(format, dialect, BuilderCoverage::complete())
    }

    /// Start a document that keeps only the demanded side-data. Semantics are always kept; families excluded by
    /// `coverage` are not built.
    ///
    /// Construction has no source bytes. Bind source later with [`Self::bind_source`].
    pub fn try_new_with_coverage(
        format: &str,
        dialect: Option<&str>,
        coverage: BuilderCoverage,
    ) -> Result<Self, DataError> {
        crate::identity::validate(format).map_err(|_| DataError::InvalidDocument)?;
        if let Some(dialect) = dialect {
            crate::identity::validate(dialect).map_err(|_| DataError::InvalidDocument)?;
        }
        let mut schema = AccountedSchemaBuilder::new();
        let (format, dialect) = if let Some(dialect) = dialect {
            (schema.bind_format(format)?, Some(schema.bind_dialect(dialect)?))
        } else {
            (schema.bind_format(format)?, None)
        };
        let key = DocumentId::try_fresh().ok_or(DataError::ArithmeticOverflow)?;
        Self::fresh(Some(schema), format, dialect, key, coverage)
    }

    /// One fresh builder over the shared empty-arena tail. Prepared-route constructors pass `schema = None` so they do
    /// not carry a dead dynamic intern table.
    fn fresh(
        schema: Option<AccountedSchemaBuilder>,
        format: FormatBindingId,
        dialect: Option<DialectBindingId>,
        key: DocumentId,
        coverage: BuilderCoverage,
    ) -> Result<Self, DataError> {
        Ok(Self {
            schema,
            shared_schema: None,
            schema_prototype: None,
            #[cfg(feature = "benchmark-internals")]
            schema_execution: SchemaExecution::accounted_dynamic(),
            format,
            dialect,
            key,
            generation: super::fresh_builder_generation()?,
            next_stage_generation: 1,
            active_text_stage: None,
            source_binding: None,
            _source: PhantomData,
            unsealed_source_span_end: None,
            finalization_source: None,
            demanded_coverage: coverage,
            empty_families: AuthoritativeEmptyFamilies::none(),
            diagnostics: DiagnosticCoverage::NotRequested,
            stored_text: String::new(),
            staged_text_detached: false,
            nodes: Vec::new(),
            wide: Vec::new(),
            tags: Vec::new(),
            occurrences: Vec::new(),
            owner_positions: Vec::new(),
            root_positions: Vec::new(),
            facts: Vec::new(),
            span_materializer: None,
            container_spans: 0,
            span_cache: Vec::new(),
            authored_spans: Vec::new(),
            staged_nodes: Vec::new(),
            staged_occurrences: Vec::new(),
        })
    }

    fn try_new_from_prototype(
        prototype: &DocumentSchemaPrototype,
        coverage: BuilderCoverage,
    ) -> Result<(Self, PreparedDocumentSchema), DataError> {
        let key = DocumentId::try_fresh().ok_or(DataError::ArithmeticOverflow)?;
        let (shared_schema, prepared) = prototype.try_prepare_document(key);
        let mut builder = Self::fresh(
            None,
            prototype.format_binding(),
            prototype.dialect_binding(),
            key,
            coverage,
        )?;
        builder.shared_schema = Some(shared_schema);
        builder.schema_prototype = Some(prototype.identity());
        #[cfg(feature = "benchmark-internals")]
        builder.schema_execution.bind_prepared(prototype.recipe_fingerprint());
        Ok((builder, prepared))
    }

    /// Installs recycled authoring scratch from a previous document.
    ///
    /// CAPACITY ONLY: every installed vector is empty, so this build observes exactly what a fresh one does — the
    /// position tables still zero-fill from length 0. Call once, immediately after construction, while this builder's
    /// own scratch is still untouched; the vectors it hands back in exchange are the empty ones it was born with.
    pub fn install_transients(&mut self, spare: &mut super::DocumentTransients) {
        core::mem::swap(&mut self.owner_positions, &mut spare.owner_positions);
        core::mem::swap(&mut self.staged_nodes, &mut spare.staged_nodes);
        core::mem::swap(&mut self.staged_occurrences, &mut spare.staged_occurrences);
    }

    fn require_dynamic_schema(&self) -> Result<(), DataError> {
        if self.shared_schema.is_some() || self.schema.is_none() {
            Err(DataError::InvalidDocument)
        } else {
            Ok(())
        }
    }

    fn dynamic_schema_mut(&mut self) -> Result<&mut AccountedSchemaBuilder, DataError> {
        self.schema.as_mut().ok_or(DataError::InvalidDocument)
    }

    /// Returns this document's identity.
    #[must_use]
    pub const fn key(&self) -> DocumentId {
        self.key
    }

    /// Declares zero-allocation authoritative absence for native concept families which cannot occur in every product
    /// this builder publishes.
    pub fn set_authoritative_empty_families(&mut self, families: AuthoritativeEmptyFamilies) {
        self.empty_families = families;
    }

    /// Sets successful-diagnostic authority for this published document.
    pub fn set_diagnostic_coverage(&mut self, diagnostics: DiagnosticCoverage) {
        self.diagnostics = diagnostics;
    }

    fn require_attached_facts(&self) -> Result<(), DataError> {
        if self.demanded_coverage.attached_facts() {
            Ok(())
        } else {
            Err(DataError::CapabilityUnavailable {
                capability: super::DocumentCapability::AttachedFacts,
            })
        }
    }

    /// Returns the number of nodes authored so far.
    ///
    /// Codecs use this with the number of source bytes already consumed to project a large document's final table sizes
    /// and reserve them in one step, avoiding repeated amortized-doubling growth of the node arena.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.logical_node_count()
    }

    /// Returns the number of occurrences authored so far.
    ///
    /// The occurrence-count companion to [`Self::node_count`] for the same source-density capacity projection.
    #[must_use]
    pub fn occurrence_count(&self) -> usize {
        self.logical_occurrence_count()
    }

    /// Reserves exact additional capacity for the tables a builder charges directly: nodes, occurrences, stored text
    /// bytes, and facts.
    ///
    /// The `wide`, `tags`, and per-role position tables are not reserved here; their capacity is bounded independently
    /// (see the `owner_positions` note below).
    pub fn try_reserve(
        &mut self,
        additional: DocumentCapacity,
        _resources: &ResourceContext<'_>,
    ) -> Result<(), DataError> {
        // The codec's projected `additional` is relative to the LOGICAL counts (`node_count()`/`occurrence_count()`),
        // so any staged records must reach the physical arenas first or the reserve under-admits by exactly the staged
        // block — a late growth where the projection promised admission.
        self.flush_staged()?;
        self.nodes
            .try_reserve_exact(additional.nodes)
            .map_err(jqf_resource::ResourceError::from)?;
        // `owner_positions` is the per-role outer table (`Vec<Vec<u32>>`, indexed `[role][node]`): its outer length is
        // bounded by the schema's occurrence-role count, never by the node count, so co-reserving node-count slots here
        // would over-admit against a table that holds one inner vector per role. The inner `[node]` tables still
        // amortize independently inside `grow_position_table`.
        self.occurrences
            .try_reserve_exact(additional.occurrences)
            .map_err(jqf_resource::ResourceError::from)?;
        self.stored_text
            .try_reserve_exact(additional.stored_text_bytes)
            .map_err(jqf_resource::ResourceError::from)?;
        self.facts
            .try_reserve_exact(additional.facts)
            .map_err(jqf_resource::ResourceError::from)?;
        Ok(())
    }

    /// Append decoded UTF-8 into compact text storage.
    pub fn store_text(&mut self, value: &str, _resources: &ResourceContext<'_>) -> Result<DocumentTextId, DataError> {
        if self.staged_text_detached {
            return Err(DataError::InvalidDocument);
        }
        let prepared = prepare_stored_text(&mut self.stored_text, value, self.key, self.generation)?;
        Ok(prepared.commit())
    }

    /// Begins one text entry whose bytes may be appended across cooperative polls.
    pub fn begin_text(&mut self, _resources: &ResourceContext<'_>) -> Result<AccountedTextStage, DataError> {
        if self.active_text_stage.is_some() || self.staged_text_detached {
            return Err(DataError::InvalidDocument);
        }
        let stage_generation = self.next_stage_generation;
        self.next_stage_generation = self
            .next_stage_generation
            .checked_add(1)
            .ok_or(DataError::ArithmeticOverflow)?;
        let start = self.stored_text.len();
        self.active_text_stage = Some((stage_generation, start));
        Ok(AccountedTextStage {
            key: self.key,
            builder_generation: self.generation,
            stage_generation,
            start,
            cursor: start,
        })
    }

    /// The staged-text identity guard shared by `append_text`, `take_staged_text`, and `finish_text`: the stage must be
    /// this builder's live, attached, fully-consumed stage. One copy, so the five-clause law cannot drift between the
    /// three callers. (`restore_staged_text` deliberately checks a different set — it reattaches a DETACHED stage.)
    fn ensure_live_text_stage(&self, stage: &AccountedTextStage) -> Result<(), DataError> {
        if self.staged_text_detached
            || stage.key != self.key
            || stage.builder_generation != self.generation
            || self.active_text_stage != Some((stage.stage_generation, stage.start))
            || stage.cursor != self.stored_text.len()
        {
            return Err(DataError::InvalidDocument);
        }
        Ok(())
    }

    /// Appends one already-admitted UTF-8 chunk to the active staged text entry.
    pub fn append_text(
        &mut self,
        stage: &mut AccountedTextStage,
        value: &str,
        _resources: &ResourceContext<'_>,
    ) -> Result<(), DataError> {
        self.ensure_live_text_stage(stage)?;
        let next_cursor = stage
            .cursor
            .checked_add(value.len())
            .ok_or(DataError::ArithmeticOverflow)?;
        // The staged path is an input-driven accumulation surface like `prepare_stored_text`: reserve fallibly so a
        // decode that would cross the ceiling refuses instead of aborting.
        self.stored_text
            .try_reserve(value.len())
            .map_err(jqf_resource::ResourceError::from)?;
        self.stored_text.push_str(value);
        stage.cursor = next_cursor;
        Ok(())
    }

    /// Detaches the staged text arena into a decoder's streaming window.
    ///
    /// A string decoder that appends many small chunks per staged entry pays [`Self::append_text`]'s stage-identity
    /// validation on every chunk. This validates ONCE, moves the arena out (three words — the bytes never copy), and
    /// lets the decoder push into it directly with its own fallible reserves; the window closes through
    /// [`Self::restore_staged_text`], which settles the stage cursor. While detached, every text-entry operation
    /// refuses, so the arena cannot fork; the decoder restores on every exit from its window, including errors.
    #[inline]
    pub fn take_staged_text(&mut self, stage: &AccountedTextStage) -> Result<String, DataError> {
        self.ensure_live_text_stage(stage)?;
        self.staged_text_detached = true;
        Ok(core::mem::take(&mut self.stored_text))
    }

    /// Reattaches the staged text arena a decoder streamed into and settles the stage cursor at the arena's new end.
    /// The arena may only have GROWN: a shorter return would move text an earlier entry already names, so it is
    /// refused.
    #[inline]
    pub fn restore_staged_text(&mut self, stage: &mut AccountedTextStage, text: String) -> Result<(), DataError> {
        if !self.staged_text_detached
            || !self.stored_text.is_empty()
            || stage.key != self.key
            || stage.builder_generation != self.generation
            || self.active_text_stage != Some((stage.stage_generation, stage.start))
            || text.len() < stage.cursor
        {
            return Err(DataError::InvalidDocument);
        }
        stage.cursor = text.len();
        self.stored_text = text;
        self.staged_text_detached = false;
        Ok(())
    }

    /// Completes one staged text entry and returns its stable document-local identity.
    #[inline]
    pub fn finish_text(
        &mut self,
        stage: AccountedTextStage,
        _resources: &ResourceContext<'_>,
    ) -> Result<DocumentTextId, DataError> {
        self.ensure_live_text_stage(&stage)?;
        let range = jqf_source::Span::try_new(
            u32::try_from(stage.start).map_err(|_| DataError::ArithmeticOverflow)?,
            u32::try_from(stage.cursor).map_err(|_| DataError::ArithmeticOverflow)?,
        )
        .ok_or(DataError::ArithmeticOverflow)?;
        self.active_text_stage = None;
        Ok(DocumentTextId::new_accounted(range, self.key, self.generation))
    }

    /// Binds a cooperatively precomputed source seal for source text insertion.
    ///
    /// A codec may bind before it inserts any source text — the whole-document case, whose sealed extent is known
    /// from the first byte — or after it has inserted some, which is the only order available to a codec that learns
    /// the extent its document names when its root value ends. Neither order lets a span outlive publication
    /// unauthenticated: a seal that does not cover every span already admitted is rejected here.
    pub fn bind_source(&mut self, binding: super::DocumentSourceBinding) -> Result<(), DataError> {
        if self.source_binding.is_some_and(|current| current != binding) {
            return Err(DataError::InvalidDocument);
        }
        if self
            .unsealed_source_span_end
            .is_some_and(|end| u64::from(end) > binding.seal().byte_length())
        {
            return Err(DataError::InvalidDocument);
        }
        self.source_binding = Some(binding);
        self.unsealed_source_span_end = None;
        Ok(())
    }

    /// Adds a string node through a request-bound prepared node-kind handle.
    pub fn add_prepared_stored_string_node(
        &mut self,
        schema: &PreparedDocumentSchema,
        kind: PreparedNodeKind,
        text: DocumentTextId,
        _resources: &ResourceContext<'_>,
    ) -> Result<NodeId, DataError> {
        let text = text.resolve_accounted(self.key, self.generation)?;
        let kind = schema.verify_node_kind(kind, self.schema_prototype, self.key)?;
        let result = self.add_ready_node_id(kind, StoredSemanticNode::Text(text));
        if result.is_ok() {
            #[cfg(feature = "benchmark-internals")]
            self.schema_execution.record_prepared();
        }
        result
    }

    /// Consumes an origin-bound stored-text token into its compact span for a codec continuation that retains this
    /// exact builder.
    ///
    /// # Safety
    ///
    /// The returned span must be used only with this builder and only through the corresponding bound-span insertion
    /// APIs before publication.
    #[doc(hidden)]
    pub unsafe fn consume_bound_stored_text_span(
        &self,
        text: DocumentTextId,
        _resources: &ResourceContext<'_>,
    ) -> Result<jqf_source::Span, DataError> {
        let super::TextRef::Stored(span) = text.resolve_accounted(self.key, self.generation)? else {
            return Err(DataError::InvalidDocument);
        };
        self.stored_text
            .as_str()
            .get(span.start() as usize..span.end() as usize)
            .ok_or(DataError::InvalidDocument)?;
        Ok(span)
    }

    /// Reads the text of a span produced by [`Self::consume_bound_stored_text_span`] on this builder, without consuming
    /// anything — the JSON codec's prune lookup reads the pending object key before deciding whether the member's
    /// value is built.
    ///
    /// # Safety
    ///
    /// `span` must have come from [`Self::consume_bound_stored_text_span`] on THIS builder, unmodified.
    #[doc(hidden)]
    #[must_use]
    pub unsafe fn bound_stored_text(&self, span: jqf_source::Span) -> Option<&str> {
        self.stored_text
            .as_str()
            .get(span.start() as usize..span.end() as usize)
    }

    /// Installs the format-owned reader that materializes this build's container spans, and is required before any of
    /// them is committed.
    ///
    /// The reader is stateless and `'static`: it turns one validated source text into an owned value and holds nothing
    /// between calls, which is what keeps [`Document`](super::Document) format-neutral while the codec keeps its format
    /// facts.
    #[doc(hidden)]
    pub fn bind_span_materializer(&mut self, materializer: &'static dyn super::LazySpanMaterializer) {
        self.span_materializer = Some(materializer);
    }

    /// Adds a container node whose subtree was deliberately NOT built, naming the validated source extent the container
    /// occupies instead.
    ///
    /// The node costs one charged node record; the subtree's nodes, occurrences, and relationship arenas are never
    /// built. It carries NO occurrences, so every borrowed container projection over it fails closed
    /// ([`DataError::UnmaterializedContainerSpan`]) rather than reading as an empty container.
    ///
    /// # Errors
    ///
    /// Returns [`DataError::InvalidDocument`] when no span materializer is bound, or when the span is not contained in
    /// the sealed source, and the ordinary allocation/resource errors of a node push.
    ///
    /// # Safety
    ///
    /// `span` must name one complete, already-validated container text inside the exact immutable source authority this
    /// builder is bound to, or will be bound to before it publishes, and the caller must retain that authority
    /// unchanged through publication. The containment contract is
    /// [`add_prepared_bound_source_string_node`](Self::add_prepared_bound_source_string_node)'s exactly; what this call
    /// adds is that the extent is a whole value of the document's format, which only the decoder that scanned it can
    /// know.
    #[doc(hidden)]
    pub unsafe fn add_prepared_bound_container_span_node(
        &mut self,
        schema: &PreparedDocumentSchema,
        kind: PreparedNodeKind,
        span: jqf_source::Span,
        container: super::ContainerSpanKind,
        _resources: &ResourceContext<'_>,
    ) -> Result<NodeId, DataError> {
        if self.span_materializer.is_none() {
            return Err(DataError::InvalidDocument);
        }
        let text = self.validate_bound_source_span(span)?;
        let kind = schema.verify_node_kind(kind, self.schema_prototype, self.key)?;
        // Commit discipline: settle the counter BEFORE the node lands, so an overflow refuses without leaving a
        // committed span node behind it.
        self.container_spans = self
            .container_spans
            .checked_add(1)
            .ok_or(DataError::ArithmeticOverflow)?;
        let node = self.add_ready_node_id(kind, StoredSemanticNode::ContainerSpan { text, container })?;
        #[cfg(feature = "benchmark-internals")]
        self.schema_execution.record_prepared();
        Ok(node)
    }

    /// Records array cardinality and/or collect-filter hits the decoder counted while proving `node`'s container span.
    ///
    /// Last-wins Exact updates this row when a later duplicate key wins: the caller records the winner's counts on the
    /// latest winning span's slot during the proving pass. Both `None` is a no-op. A non-span node, or a record for an
    /// earlier node, is [`DataError::InvalidDocument`].
    pub fn set_container_span_counts(
        &mut self,
        node: NodeId,
        child_count: Option<u64>,
        filter_count: Option<u64>,
    ) -> Result<(), DataError> {
        if child_count.is_none() && filter_count.is_none() {
            return Ok(());
        }
        self.validate_node(node)?;
        let index = node.index();
        let semantic = match self.nodes.get(index) {
            Some(record) => &record.semantic,
            None => {
                &self
                    .staged_nodes
                    .get(index - self.nodes.len())
                    .ok_or(DataError::InvalidNode)?
                    .semantic
            }
        };
        if !matches!(semantic, NodeSemantic::ContainerSpan { .. }) {
            return Err(DataError::InvalidDocument);
        }
        let slot = self.span_cache_slot(node)?;
        slot.child_count = child_count;
        slot.filter_count = filter_count;
        Ok(())
    }

    fn span_cache_slot(&mut self, node: NodeId) -> Result<&mut super::storage::SpanCache, DataError> {
        self.validate_node(node)?;
        match self.span_cache.last() {
            Some(last) if last.node == node => {}
            Some(last) if last.node.index() >= node.index() => return Err(DataError::InvalidDocument),
            Some(_) | None => {
                self.span_cache
                    .try_reserve(1)
                    .map_err(jqf_resource::ResourceError::from)?;
                self.span_cache.push(super::storage::SpanCache::empty(node));
            }
        }
        self.span_cache.last_mut().ok_or(DataError::InvalidDocument)
    }

    /// Records a collect-probe tally the decoder counted while proving `node`'s container span.
    pub fn set_container_span_probe_count(&mut self, node: NodeId, probe_count: Option<u64>) -> Result<(), DataError> {
        let Some(probe_count) = probe_count else {
            return Ok(());
        };
        self.span_cache_slot(node)?.probe_count = Some(probe_count);
        Ok(())
    }

    /// Records last-wins `has(LITERAL)` presence the decoder proved while walking `node`'s container span.
    pub fn set_container_span_has(&mut self, node: NodeId, has_present: Option<bool>) -> Result<(), DataError> {
        let Some(has_present) = has_present else {
            return Ok(());
        };
        self.span_cache_slot(node)?.has_present = Some(has_present);
        Ok(())
    }

    /// Records last-wins object key names the decoder collected while proving `node`'s container span.
    pub fn set_container_span_keys(
        &mut self,
        node: NodeId,
        keys: Option<alloc::vec::Vec<alloc::string::String>>,
    ) -> Result<(), DataError> {
        let Some(keys) = keys else {
            return Ok(());
        };
        self.span_cache_slot(node)?.keys = Some(keys);
        Ok(())
    }

    /// Records `FanOut` probe/construct values the decoder captured while proving `node`'s container span.
    pub fn set_container_span_values(
        &mut self,
        node: NodeId,
        values: Option<alloc::vec::Vec<crate::Value>>,
    ) -> Result<(), DataError> {
        let Some(values) = values else {
            return Ok(());
        };
        self.span_cache_slot(node)?.values = Some(values);
        Ok(())
    }

    /// Records the last-wins `min`/`max` winner the decoder proved while walking `node`'s container span.
    pub fn set_container_span_minmax(&mut self, node: NodeId, winner: Option<crate::Value>) -> Result<(), DataError> {
        let Some(winner) = winner else {
            return Ok(());
        };
        self.span_cache_slot(node)?.minmax = Some(winner);
        Ok(())
    }

    /// Adds a source-backed string from a span already validated by the owning codec session, without retaining or
    /// copying its full public source token.
    ///
    /// # Safety
    ///
    /// `span` must name UTF-8 inside the exact immutable source authority this builder is bound to, or will be bound to
    /// before it publishes, and the caller must retain that authority unchanged through publication.
    ///
    /// The builder proves CONTAINMENT and nothing more: against the seal when one is already bound, and otherwise by
    /// retaining the widest admitted end for [`bind_source`](Self::bind_source) to cover, with publication refused
    /// while any admitted extent is still unsealed. Which authority the bytes belong to, and that they are UTF-8, stay
    /// the caller's to guarantee — admission holds the seal, not the bytes.
    #[doc(hidden)]
    pub unsafe fn add_prepared_bound_source_string_node(
        &mut self,
        schema: &PreparedDocumentSchema,
        kind: PreparedNodeKind,
        span: jqf_source::Span,
        _resources: &ResourceContext<'_>,
    ) -> Result<NodeId, DataError> {
        let text = self.validate_bound_source_span(span)?;
        let kind = schema.verify_node_kind(kind, self.schema_prototype, self.key)?;
        let node = self.add_ready_node_id(kind, StoredSemanticNode::Text(text))?;
        #[cfg(feature = "benchmark-internals")]
        self.schema_execution.record_prepared();
        Ok(node)
    }

    /// Adds a canonical integer whose spelling IS a span of the sealed source, so the document names those bytes
    /// instead of owning a copy.
    ///
    /// The number counterpart of
    /// [`add_prepared_bound_source_string_node`](Self::add_prepared_bound_source_string_node); nothing downstream can
    /// tell the two apart — [`TextRef`](super::TextRef) resolves a source span and a stored span through the same
    /// accessor. The node carries no text residency: its bytes belong to the input buffer the document already borrows
    /// and holds live, so charging them would bill the same buffer twice.
    ///
    /// # Safety
    ///
    /// `span` must lie inside the exact immutable source authority this builder is bound to, or will be bound to before
    /// it publishes, and the bytes it names must ALREADY be jqf's canonical integer spelling — the same text
    /// [`add_prepared_stored_integer_node`](Self::add_prepared_stored_integer_node) would have validated; the builder
    /// holds the seal, not the bytes. The caller must retain that authority unchanged through publication. Admission
    /// proves CONTAINMENT alone, on the same terms as
    /// [`add_prepared_bound_source_string_node`](Self::add_prepared_bound_source_string_node).
    #[doc(hidden)]
    pub unsafe fn add_prepared_bound_source_integer_node(
        &mut self,
        schema: &PreparedDocumentSchema,
        kind: PreparedNodeKind,
        span: jqf_source::Span,
        _resources: &ResourceContext<'_>,
    ) -> Result<NodeId, DataError> {
        let text = self.validate_bound_source_span(span)?;
        let kind = schema.verify_node_kind(kind, self.schema_prototype, self.key)?;
        let node = self.add_ready_node_id(kind, StoredSemanticNode::StoredInteger(text))?;
        #[cfg(feature = "benchmark-internals")]
        self.schema_execution.record_prepared();
        Ok(node)
    }

    /// Adds one authored source span for a scalar whose retained semantic carries no span of its own (a codec float,
    /// decimal, or boolean).
    ///
    /// The span names the scalar's COMPLETE authored token in the source this builder is bound to (or will be bound to
    /// before it publishes) — the bytes the edit lane echoes verbatim when the value is unchanged or replaces when it
    /// is patched. The semantic is stored exactly as without the span; the span is an addressing channel, never a
    /// second value.
    ///
    /// Records must be authored in node order (the codec builds nodes strictly sequentially), which keeps the table
    /// sorted for the binary search [`super::storage::AuthoredSpanRecord::find`] performs.
    ///
    /// # Safety
    ///
    /// `span` must name UTF-8 inside the exact immutable source authority this builder is bound to, or will be bound to
    /// before it publishes, and the bytes it names must re-resolve to the node's stored semantic. The caller must
    /// retain that authority unchanged through publication.
    #[doc(hidden)]
    pub unsafe fn record_authored_span(
        &mut self,
        node: NodeId,
        span: jqf_source::Span,
        _resources: &ResourceContext<'_>,
    ) -> Result<(), DataError> {
        // Enforce the documented record order: the table stays sorted only if every recorded node strictly follows its
        // predecessor, and a duplicate would make `AuthoredSpanRecord::find`'s binary search ambiguous. A codec that
        // builds nodes sequentially can never hit this; the guard turns the documented contract into an enforced one.
        if self
            .authored_spans
            .as_slice()
            .last()
            .is_some_and(|last| last.node.index() >= node.index())
        {
            return Err(DataError::InvalidDocument);
        }
        // A span addresses one existing node: validate the id, and refuse a node whose semantic already names its own
        // source bytes (the bound-source adds store the span in place) — a second record would be a duplicate
        // addressing channel, the node twin of `record_fact_authored_span`'s double-span rejection.
        self.validate_node(node)?;
        let index = node.index();
        let semantic = match self.nodes.get(index) {
            Some(record) => &record.semantic,
            None => {
                &self
                    .staged_nodes
                    .get(index - self.nodes.len())
                    .ok_or(DataError::InvalidNode)?
                    .semantic
            }
        };
        if matches!(
            semantic,
            NodeSemantic::Text(super::TextRef::Source(_))
                | NodeSemantic::StoredInteger(super::TextRef::Source(_))
                | NodeSemantic::ContainerSpan {
                    text: super::TextRef::Source(_),
                    ..
                }
        ) {
            return Err(DataError::InvalidDocument);
        }
        // Admission is the shared seal-or-retain law; the record needs no text reference back.
        self.validate_bound_source_span(span)?;
        let record = super::storage::AuthoredSpanRecord { node, span };
        // The authored-span table grows with recorded scalar tokens: reserve fallibly so a decode that would cross the
        // ceiling refuses instead of aborting.
        self.authored_spans
            .try_reserve(1)
            .map_err(jqf_resource::ResourceError::from)?;
        self.authored_spans.push(record);
        Ok(())
    }

    /// Records one authored source span on an attached fact: the edit lane's addressing channel for facts that are not
    /// nodes (markup attribute quoted values). The payload is stored exactly as without the span.
    ///
    /// # Safety
    ///
    /// `span` must name UTF-8 inside the exact immutable source authority this builder is bound to, or will be bound to
    /// before it publishes, and the bytes it names must re-resolve to the fact's stored payload. The caller must retain
    /// that authority unchanged through publication.
    #[doc(hidden)]
    pub unsafe fn record_fact_authored_span(
        &mut self,
        fact: FactId,
        span: jqf_source::Span,
        _resources: &ResourceContext<'_>,
    ) -> Result<(), DataError> {
        // Admission runs before the fact borrow: it needs `&mut self`, and an unadmittable span refuses either way.
        self.validate_bound_source_span(span)?;
        let stored = self.facts.get_mut(fact.index()).ok_or(DataError::InvalidFact)?;
        if stored.source_span().is_some() {
            return Err(DataError::InvalidDocument);
        }
        stored.set_source_span(span);
        Ok(())
    }

    /// Adds compact stored integer text through a prepared node-kind handle.
    pub fn add_prepared_stored_integer_node(
        &mut self,
        schema: &PreparedDocumentSchema,
        kind: PreparedNodeKind,
        value: DocumentTextId,
        _resources: &ResourceContext<'_>,
    ) -> Result<NodeId, DataError> {
        let text_ref = value.resolve_accounted(self.key, self.generation)?;
        let text = self.resolve_stored_ref(text_ref)?;
        crate::Integer::validate_canonical(text).map_err(|_| DataError::InvalidDocument)?;
        let kind = schema.verify_node_kind(kind, self.schema_prototype, self.key)?;
        let result = self.add_ready_node_id(kind, StoredSemanticNode::StoredInteger(text_ref));
        if result.is_ok() {
            #[cfg(feature = "benchmark-internals")]
            self.schema_execution.record_prepared();
        }
        result
    }

    /// Adds compact stored decimal text through a prepared node-kind handle.
    pub fn add_prepared_stored_decimal_node(
        &mut self,
        schema: &PreparedDocumentSchema,
        kind: PreparedNodeKind,
        coefficient: DocumentTextId,
        scale: i64,
        _resources: &ResourceContext<'_>,
    ) -> Result<NodeId, DataError> {
        let coefficient_ref = coefficient.resolve_accounted(self.key, self.generation)?;
        let text = self.resolve_stored_ref(coefficient_ref)?;
        // Digit grammar only. Stored decimals keep trailing zeroes and a zero coefficient at nonzero scale so a codec
        // can render the authored spelling (`1.50`, `0e5`). Semantic `Decimal` construction still goes through
        // [`Decimal::validate_canonical_parts`].
        crate::Integer::validate_canonical(text).map_err(|_| DataError::InvalidDocument)?;
        let kind = schema.verify_node_kind(kind, self.schema_prototype, self.key)?;
        let result = self.add_ready_node_id(
            kind,
            StoredSemanticNode::StoredDecimal {
                coefficient: coefficient_ref,
                scale,
            },
        );
        if result.is_ok() {
            #[cfg(feature = "benchmark-internals")]
            self.schema_execution.record_prepared();
        }
        result
    }

    /// Adds a prepared scalar or container node without textual schema lookup.
    pub fn add_prepared_node(
        &mut self,
        schema: &PreparedDocumentSchema,
        kind: PreparedNodeKind,
        semantic: PreparedSemanticNode,
        _resources: &ResourceContext<'_>,
    ) -> Result<NodeId, DataError> {
        let kind = schema.verify_node_kind(kind, self.schema_prototype, self.key)?;
        let semantic = match semantic {
            PreparedSemanticNode::Null => StoredSemanticNode::Null,
            PreparedSemanticNode::Bool(value) => StoredSemanticNode::Bool(value),
            PreparedSemanticNode::Float(value) => StoredSemanticNode::AccountedFloat(value),
            PreparedSemanticNode::Array(role) => StoredSemanticNode::Array {
                item_role: schema.verify_occurrence_role(role, self.schema_prototype, self.key)?,
            },
            PreparedSemanticNode::Object(role) => StoredSemanticNode::Object {
                member_role: schema.verify_occurrence_role(role, self.schema_prototype, self.key)?,
            },
        };
        let result = self.add_ready_node_id(kind, semantic);
        if result.is_ok() {
            #[cfg(feature = "benchmark-internals")]
            self.schema_execution.record_prepared();
        }
        result
    }

    /// The logical node count during authoring: committed arena records plus records staged but not yet flushed. Every
    /// authoring-time id, validation, and count reads this so the staged window is invisible (see
    /// [`Self::staged_nodes`]).
    #[inline]
    fn logical_node_count(&self) -> usize {
        self.nodes.len() + self.staged_nodes.len()
    }

    /// The logical occurrence count during authoring; see [`Self::logical_node_count`].
    #[inline]
    fn logical_occurrence_count(&self) -> usize {
        self.occurrences.len() + self.staged_occurrences.len()
    }

    /// Appends every staged record to its arena as one sequential write run per arena (nodes run, then occurrences
    /// run), holding at most two append streams live at any instant instead of the interleaved per-element streams the
    /// guest collapses on.
    ///
    /// # Errors
    ///
    /// Returns a resource error when an arena growth would cross the memory ceiling: the staged records STAY staged
    /// with the error, so a refused decode refuses cleanly instead of aborting at an infallible extend (the document
    /// arenas are the input-driven accumulation surface).
    pub(super) fn flush_staged(&mut self) -> Result<(), DataError> {
        flush_arena(&mut self.nodes, &mut self.staged_nodes)?;
        flush_arena(&mut self.occurrences, &mut self.staged_occurrences)
    }

    #[inline]
    fn add_ready_node_id(
        &mut self,
        kind: super::NodeKindBindingId,
        semantic: StoredSemanticNode,
    ) -> Result<NodeId, DataError> {
        let id = NodeId::try_from_index(self.logical_node_count()).ok_or(DataError::ArithmeticOverflow)?;
        let semantic = prepare_node_semantic(&mut self.wide, semantic)?;
        let node = NodeRecord {
            semantic,
            kind,
            intrinsic_tag: IntrinsicTagRef::NONE,
            occurrence_range: StorageRange::default(),
            projection_range: StorageRange::default(),
        };
        let full = stage_record(&mut self.staged_nodes, node)?;
        if full {
            self.flush_staged()?;
        }
        Ok(id)
    }

    /// Adds one semantic/topology node from borrowed, transitively copied input.
    #[allow(
        clippy::too_many_lines,
        reason = "one staged transaction keeps validation, input preparation, and schema binding visibly ordered"
    )]
    pub fn add_node(
        &mut self,
        kind: &str,
        semantic: AccountedSemanticNode<'_>,
        intrinsic_tag: Option<AccountedIntrinsicTag<'_>>,
        _resources: &ResourceContext<'_>,
    ) -> Result<NodeId, DataError> {
        self.require_dynamic_schema()?;
        let result = self.add_node_transaction(kind, semantic, intrinsic_tag);
        if result.is_ok() {
            #[cfg(feature = "benchmark-internals")]
            self.schema_execution.record_dynamic();
        }
        result
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the complete staged node transaction stays visible in commit order"
    )]
    fn add_node_transaction(
        &mut self,
        kind: &str,
        semantic: AccountedSemanticNode<'_>,
        intrinsic_tag: Option<AccountedIntrinsicTag<'_>>,
    ) -> Result<NodeId, DataError> {
        let tag_text = intrinsic_tag.map(|tag| match tag {
            AccountedIntrinsicTag::Core { tag, .. } | AccountedIntrinsicTag::Tagged(tag) => tag,
        });
        if tag_text.is_some_and(str::is_empty) {
            return Err(DataError::InvalidDocument);
        }
        // A `Core` tag must agree with the node's projected category; a `Tagged` tag wraps a semantic node (its payload
        // is the node's own value) OR a kindless layer node whose payload is one owned occurrence child. The layer
        // shape is what binary-tag codecs need for tag chains, and the materializer enforces the exactly-one-payload
        // law on it; a `Core` tag never attaches to a kindless node.
        if let Some(tag) = intrinsic_tag
            && matches!(tag, AccountedIntrinsicTag::Core { kind, .. } if semantic.kind() != Some(kind))
        {
            return Err(DataError::InvalidDocument);
        }
        if intrinsic_tag.is_none()
            && let Some((kind_id, role_id)) = self.dynamic_schema_mut()?.existing_node_bindings(kind, semantic.role())
        {
            let ready = match semantic {
                AccountedSemanticNode::Null => Some(StoredSemanticNode::Null),
                AccountedSemanticNode::Bool(value) => Some(StoredSemanticNode::Bool(value)),
                AccountedSemanticNode::Float(value) => Some(StoredSemanticNode::AccountedFloat(value)),
                AccountedSemanticNode::LocalDate(value) => Some(StoredSemanticNode::LocalDate(value)),
                AccountedSemanticNode::Array { .. } => Some(StoredSemanticNode::Array {
                    item_role: role_id.ok_or(DataError::InvalidDocument)?,
                }),
                AccountedSemanticNode::Object { .. } => Some(StoredSemanticNode::Object {
                    member_role: role_id.ok_or(DataError::InvalidDocument)?,
                }),
                AccountedSemanticNode::Unrepresentable => Some(StoredSemanticNode::Unrepresentable),
                _ => None,
            };
            if let Some(ready) = ready {
                let id = self.add_ready_node_id(kind_id, ready)?;
                #[cfg(feature = "benchmark-internals")]
                self.schema_execution.record_dynamic_schema_route(true);
                return Ok(id);
            }
        }
        let id = NodeId::try_from_index(self.logical_node_count()).ok_or(DataError::ArithmeticOverflow)?;
        let prepared_semantic = match semantic {
            AccountedSemanticNode::Null => PreparedNodeSemantic::Ready(StoredSemanticNode::Null),
            AccountedSemanticNode::Bool(value) => PreparedNodeSemantic::Ready(StoredSemanticNode::Bool(value)),
            AccountedSemanticNode::Integer(value) => {
                crate::Integer::validate_canonical(value).map_err(|_| DataError::InvalidDocument)?;
                PreparedNodeSemantic::StoredInteger
            }
            AccountedSemanticNode::Decimal { coefficient, scale } => {
                crate::Decimal::validate_canonical_parts(coefficient, scale).map_err(|_| DataError::InvalidDocument)?;
                PreparedNodeSemantic::StoredDecimal { scale }
            }
            AccountedSemanticNode::Float(value) => {
                PreparedNodeSemantic::Ready(StoredSemanticNode::AccountedFloat(value))
            }
            AccountedSemanticNode::String(_) => PreparedNodeSemantic::StoredString,
            AccountedSemanticNode::Bytes(value) => {
                let mut bytes = Vec::new();
                bytes
                    .try_reserve_exact(value.len())
                    .map_err(jqf_resource::ResourceError::from)?;
                bytes.extend_from_slice(value);
                PreparedNodeSemantic::Ready(StoredSemanticNode::AccountedBytes(bytes))
            }
            AccountedSemanticNode::SourceString(value) => PreparedNodeSemantic::Ready(StoredSemanticNode::Text(
                self.validate_attached_or_detached_source_text(value)?,
            )),
            AccountedSemanticNode::LocalDate(value) => {
                PreparedNodeSemantic::Ready(StoredSemanticNode::LocalDate(value))
            }
            AccountedSemanticNode::LocalTime(value) => PreparedNodeSemantic::Ready(
                StoredSemanticNode::AccountedLocalTime(copy_accounted_local_time(value)?),
            ),
            AccountedSemanticNode::LocalDateTime(value) => PreparedNodeSemantic::Ready(
                StoredSemanticNode::AccountedLocalDateTime(copy_accounted_local_date_time(value)?),
            ),
            AccountedSemanticNode::OffsetDateTime(value) => PreparedNodeSemantic::Ready(
                StoredSemanticNode::AccountedOffsetDateTime(copy_accounted_offset_date_time(value)?),
            ),
            AccountedSemanticNode::Array { .. } => PreparedNodeSemantic::Array,
            AccountedSemanticNode::Object { .. } => PreparedNodeSemantic::Object,
            AccountedSemanticNode::Unrepresentable => PreparedNodeSemantic::Ready(StoredSemanticNode::Unrepresentable),
        };

        // Schema resolution binds BEFORE the text-arena append below so a refused binding strands no
        // charged-but-unaddressed arena bytes.
        let mut schema = self.dynamic_schema_mut()?.resolve_or_prepare_bindings(
            Some(kind),
            semantic.role(),
            None,
            None,
            tag_text,
        )?;
        let kind_identity = schema.node_kind().ok_or(DataError::InvalidDocument)?;
        #[cfg(feature = "benchmark-internals")]
        let existing_schema_fast = schema.is_existing();
        let role_identity = schema.occurrence_role();
        let tag_identity = schema.take_tag_text();
        let staged = match semantic {
            AccountedSemanticNode::String(value) | AccountedSemanticNode::Integer(value) => Some(value),
            AccountedSemanticNode::Decimal { coefficient, .. } => Some(coefficient),
            _ => None,
        };
        let stored_text = staged
            .map(|value| prepare_stored_text(&mut self.stored_text, value, self.key, self.generation))
            .transpose()?;
        let semantic = match prepared_semantic {
            PreparedNodeSemantic::Ready(value) => value,
            PreparedNodeSemantic::StoredString => StoredSemanticNode::Text(
                stored_text
                    .as_ref()
                    .map(PreparedStoredText::text_ref)
                    .ok_or(DataError::InvalidDocument)?,
            ),
            PreparedNodeSemantic::StoredInteger => StoredSemanticNode::StoredInteger(
                stored_text
                    .as_ref()
                    .map(PreparedStoredText::text_ref)
                    .ok_or(DataError::InvalidDocument)?,
            ),
            PreparedNodeSemantic::StoredDecimal { scale } => StoredSemanticNode::StoredDecimal {
                coefficient: stored_text
                    .as_ref()
                    .map(PreparedStoredText::text_ref)
                    .ok_or(DataError::InvalidDocument)?,
                scale,
            },
            PreparedNodeSemantic::Array => StoredSemanticNode::Array {
                item_role: role_identity.ok_or(DataError::InvalidDocument)?,
            },
            PreparedNodeSemantic::Object => StoredSemanticNode::Object {
                member_role: role_identity.ok_or(DataError::InvalidDocument)?,
            },
        };
        let intrinsic_tag = match intrinsic_tag {
            Some(AccountedIntrinsicTag::Core { .. }) => Some(IntrinsicTag::core(crate::TagId::from_accounted(
                tag_identity.ok_or(DataError::InvalidDocument)?,
            ))),
            Some(AccountedIntrinsicTag::Tagged(_)) => Some(IntrinsicTag::tagged(crate::TagId::from_accounted(
                tag_identity.ok_or(DataError::InvalidDocument)?,
            ))),
            None => None,
        };
        let semantic = prepare_node_semantic(&mut self.wide, semantic)?;
        let intrinsic_tag = prepare_intrinsic_tag(&mut self.tags, intrinsic_tag)?;
        let node = NodeRecord {
            semantic,
            kind: kind_identity,
            intrinsic_tag,
            occurrence_range: StorageRange::default(),
            projection_range: StorageRange::default(),
        };
        let full = stage_record(&mut self.staged_nodes, node)?;
        if full {
            self.flush_staged()?;
        }
        #[cfg(feature = "benchmark-internals")]
        self.schema_execution.record_dynamic_schema_route(existing_schema_fast);
        Ok(id)
    }

    /// Adds one ordered topology occurrence from borrowed copied input.
    pub fn add_occurrence(
        &mut self,
        owner: LocalOwnerRef,
        role: &str,
        key: Option<AccountedOccurrenceKey<'_>>,
        target: NodeId,
        _resources: &ResourceContext<'_>,
    ) -> Result<OccurrenceId, DataError> {
        self.require_dynamic_schema()?;
        let result = self.add_occurrence_transaction(owner, role, key, target);
        if result.is_ok() {
            #[cfg(feature = "benchmark-internals")]
            self.schema_execution.record_dynamic();
        }
        result
    }

    fn add_occurrence_transaction(
        &mut self,
        owner: LocalOwnerRef,
        role: &str,
        key: Option<AccountedOccurrenceKey<'_>>,
        target: NodeId,
    ) -> Result<OccurrenceId, DataError> {
        self.validate_occurrence_owner(owner)?;
        self.validate_node(target)?;
        if key.is_none()
            && let Some(stored_role) = self.dynamic_schema_mut()?.existing_occurrence_role(role)
        {
            let id = self.commit_occurrence(owner, stored_role, None, target)?;
            #[cfg(feature = "benchmark-internals")]
            self.schema_execution.record_dynamic_schema_route(true);
            return Ok(id);
        }
        let key = self.prepare_occurrence_key(key)?;
        let schema = self
            .dynamic_schema_mut()?
            .resolve_or_prepare_bindings(None, Some(role), None, None, None)?;
        let stored_role = schema.occurrence_role().ok_or(DataError::InvalidDocument)?;
        #[cfg(feature = "benchmark-internals")]
        let existing_schema_fast = schema.is_existing();
        let id = self.commit_occurrence(owner, stored_role, key, target)?;
        #[cfg(feature = "benchmark-internals")]
        self.schema_execution.record_dynamic_schema_route(existing_schema_fast);
        Ok(id)
    }

    /// Converts one borrowed occurrence-key input into its stored form, copying borrowed text into the compact text
    /// arena.
    fn prepare_occurrence_key(
        &mut self,
        key: Option<AccountedOccurrenceKey<'_>>,
    ) -> Result<Option<StoredOccurrenceKey>, DataError> {
        Ok(match key {
            Some(AccountedOccurrenceKey::Text(value)) => {
                let prepared = prepare_stored_text(&mut self.stored_text, value, self.key, self.generation)?;
                Some(prepared.text_ref())
            }
            Some(AccountedOccurrenceKey::SourceText(value)) => {
                Some(self.validate_attached_or_detached_source_text(value)?)
            }
            Some(AccountedOccurrenceKey::StoredText(value)) => {
                Some(value.resolve_accounted(self.key, self.generation)?)
            }
            None => None,
        })
    }

    /// Shared tail of every occurrence admission: assigns the id, reserves position-ledger capacity, stamps the
    /// position, stages the record, and flushes when the stage fills.
    fn commit_occurrence(
        &mut self,
        owner: LocalOwnerRef,
        role: OccurrenceRoleBindingId,
        key: Option<StoredOccurrenceKey>,
        target: NodeId,
    ) -> Result<OccurrenceId, DataError> {
        let id = OccurrenceId::try_from_index(self.logical_occurrence_count()).ok_or(DataError::ArithmeticOverflow)?;
        let (position, position_commit) =
            prepare_owner_position(owner, role, &mut self.owner_positions, &mut self.root_positions)?;
        let occurrence = OccurrenceRecord {
            owner,
            role,
            position,
            key,
            target,
        };
        let full = stage_record(&mut self.staged_occurrences, occurrence)?;
        position_commit.commit();
        if full {
            self.flush_staged()?;
        }
        Ok(id)
    }

    /// Adds one occurrence through a request-bound prepared role handle.
    pub fn add_prepared_occurrence(
        &mut self,
        schema: &PreparedDocumentSchema,
        owner: LocalOwnerRef,
        role: PreparedOccurrenceRole,
        key: Option<AccountedOccurrenceKey<'_>>,
        target: NodeId,
        _resources: &ResourceContext<'_>,
    ) -> Result<OccurrenceId, DataError> {
        let stored_role = schema.verify_occurrence_role(role, self.schema_prototype, self.key)?;
        self.validate_occurrence_owner(owner)?;
        self.validate_node(target)?;
        let key = self.prepare_occurrence_key(key)?;
        let id = self.commit_occurrence(owner, stored_role, key, target)?;
        #[cfg(feature = "benchmark-internals")]
        self.schema_execution.record_prepared();
        Ok(id)
    }

    /// Adds a prepared occurrence whose source key span was already validated by the owning codec session.
    ///
    /// # Safety
    ///
    /// `span` must name UTF-8 in the exact immutable source authority this builder is bound to, or will be bound to
    /// before it publishes, retained unchanged through publish. Admission proves containment alone, on the terms
    /// [`add_prepared_bound_source_string_node`](Self::add_prepared_bound_source_string_node) states.
    #[doc(hidden)]
    pub unsafe fn add_prepared_bound_source_occurrence(
        &mut self,
        schema: &PreparedDocumentSchema,
        owner: LocalOwnerRef,
        role: PreparedOccurrenceRole,
        span: jqf_source::Span,
        target: NodeId,
        _resources: &ResourceContext<'_>,
    ) -> Result<OccurrenceId, DataError> {
        let key = self.validate_bound_source_span(span)?;
        self.add_prepared_bound_occurrence_key(schema, owner, role, key, target)
    }

    /// Adds a prepared occurrence using a compact stored-text span previously consumed from this exact builder.
    ///
    /// # Safety
    ///
    /// `span` must be the unchanged result of [`Self::consume_bound_stored_text_span`] on this builder.
    #[doc(hidden)]
    pub unsafe fn add_prepared_bound_stored_occurrence(
        &mut self,
        schema: &PreparedDocumentSchema,
        owner: LocalOwnerRef,
        role: PreparedOccurrenceRole,
        span: jqf_source::Span,
        target: NodeId,
        _resources: &ResourceContext<'_>,
    ) -> Result<OccurrenceId, DataError> {
        self.stored_text
            .as_str()
            .get(span.start() as usize..span.end() as usize)
            .ok_or(DataError::InvalidDocument)?;
        self.add_prepared_bound_occurrence_key(schema, owner, role, super::TextRef::Stored(span), target)
    }

    fn add_prepared_bound_occurrence_key(
        &mut self,
        schema: &PreparedDocumentSchema,
        owner: LocalOwnerRef,
        role: PreparedOccurrenceRole,
        key: StoredOccurrenceKey,
        target: NodeId,
    ) -> Result<OccurrenceId, DataError> {
        let stored_role = schema.verify_occurrence_role(role, self.schema_prototype, self.key)?;
        self.validate_occurrence_owner(owner)?;
        self.validate_node(target)?;
        let id = self.commit_occurrence(owner, stored_role, Some(key), target)?;
        #[cfg(feature = "benchmark-internals")]
        self.schema_execution.record_prepared();
        Ok(id)
    }

    /// Adds one globally ordered attached fact after recursively copying its complete payload allocation graph into
    /// owned storage.
    #[allow(
        clippy::too_many_arguments,
        reason = "the short resource borrow is explicit in addition to the existing complete fact schema"
    )]
    pub fn add_fact(
        &mut self,
        owner: LocalOwnerRef,
        role: &str,
        kind: &str,
        schema_version: u32,
        payload: &FactPayload,
        resources: &ResourceContext<'_>,
    ) -> Result<FactId, DataError> {
        self.require_dynamic_schema()?;
        self.require_attached_facts()?;
        let result = self.add_fact_transaction(owner, role, kind, schema_version, payload, resources);
        if result.is_ok() {
            #[cfg(feature = "benchmark-internals")]
            self.schema_execution.record_dynamic();
        }
        result
    }

    fn add_fact_transaction(
        &mut self,
        owner: LocalOwnerRef,
        role: &str,
        kind: &str,
        schema_version: u32,
        payload: &FactPayload,
        resources: &ResourceContext<'_>,
    ) -> Result<FactId, DataError> {
        self.validate_owner(owner)?;
        let id = FactId::try_from_index(self.facts.len()).ok_or(DataError::ArithmeticOverflow)?;
        // Schema resolution binds BEFORE the accounted payload copy below so a refused fact role/kind strands no
        // charged payload bytes.
        let schema =
            self.dynamic_schema_mut()?
                .resolve_or_prepare_bindings(None, None, Some(kind), Some(role), None)?;
        let role = schema.fact_role().ok_or(DataError::InvalidDocument)?;
        let kind = schema.fact_kind().ok_or(DataError::InvalidDocument)?;
        #[cfg(feature = "benchmark-internals")]
        let existing_schema_fast = schema.is_existing();
        let payload = StoredFactPayload::try_accounted_copy(payload, resources)?;
        // The facts arena grows with fact count: reserve fallibly so a decode that would cross the ceiling refuses
        // instead of aborting.
        self.facts.try_reserve(1).map_err(jqf_resource::ResourceError::from)?;
        self.facts
            .push(StoredDocumentFact::new(id, owner, role, kind, schema_version, payload));
        #[cfg(feature = "benchmark-internals")]
        self.schema_execution.record_dynamic_schema_route(existing_schema_fast);
        Ok(id)
    }

    /// Validates the authored state and returns the cooperative finalizer that publishes it; see [`Self::finish`] for
    /// the one-shot sibling.
    pub fn begin_finish(
        self,
        root: NodeId,
        _resources: &ResourceContext<'_>,
    ) -> Result<AccountedDocumentFinalizer<'source>, DataError> {
        self.validate_node(root)?;
        if self.active_text_stage.is_some() {
            return Err(DataError::InvalidDocument);
        }
        // Every admitted source span must be covered by a bound seal.
        if self.unsealed_source_span_end.is_some() {
            return Err(DataError::InvalidDocument);
        }
        Ok(AccountedDocumentFinalizer {
            builder: Some(self),
            root,
            phase: FinalizationPhase::Reserve { credited: 0 },
            adjacency: None,
            counts: Vec::new(),
            owner_occurrences: Vec::new(),
            fact_owner_nodes: Vec::new(),
            fact_owner_ranges: Vec::new(),
            fact_owner_ids: Vec::new(),
            spare: super::DocumentTransients::new(),
        })
    }

    /// Validates and publishes immutable document storage.
    #[allow(
        clippy::too_many_lines,
        reason = "one-shot publish: coverage, arenas, schema, fact-owner index, then storage"
    )]
    pub fn finish(mut self, root: NodeId, _resources: &ResourceContext<'_>) -> Result<Document<'source>, DataError> {
        // Publish every staged record before any arena reader (coverage, adjacency, arena emit) runs, so the physical
        // arenas hold the logical document.
        self.flush_staged()?;
        self.validate_node(root)?;
        if self.active_text_stage.is_some() {
            return Err(DataError::InvalidDocument);
        }
        // Every admitted source span must be covered by a bound seal; a leftover extent means one was committed against
        // a seal that never arrived.
        if self.unsealed_source_span_end.is_some() {
            return Err(DataError::InvalidDocument);
        }
        let coverage = published_coverage(
            self.demanded_coverage,
            self.empty_families,
            self.diagnostics,
            &CoverageEvidence {
                nodes: self.nodes.as_slice(),
                wide: self.wide.as_slice(),
                occurrences: self.occurrences.as_slice(),
                facts: self.facts.as_slice(),
                authored_spans: self.authored_spans.as_slice(),
            },
        )?;
        let build_topology = self.demanded_coverage.topology();
        let owner_occurrences = self.build_owner_occurrences()?;
        // `owner_positions` is authoring scratch: `add_occurrence` reads and bumps each owner's role-position counter
        // to stamp the occurrence's stored `position`, and nothing past authoring reads it — neither the adjacency
        // derivation above nor the relationship-arena emit below touches it. It is the dense per-role ledger — 4
        // bytes per (role, node) cell over the owner prefix, the largest single transient of a whole-document build —
        // so release it here rather than hold it live under the arena pass's peak until the builder drops, matching the
        // cooperative finalizer's equivalent release in publish.rs.
        self.owner_positions = Vec::new();
        // Move the node table out so the arena pass can mutate its `projection_range` while `&self` still resolves
        // member-key text.
        let mut nodes = core::mem::take(&mut self.nodes);
        let arenas = super::storage::emit_relationship_arenas_accounted(
            nodes.as_mut_slice(),
            self.occurrences.as_slice(),
            owner_occurrences.as_slice(),
            build_topology,
            &self,
        )?;
        let (shared_schema, inline_schema) = match self.shared_schema {
            Some(schema) => (Some(schema), None),
            None => (
                None,
                Some(self.schema.take().ok_or(DataError::InvalidDocument)?.finish()),
            ),
        };
        match (&shared_schema, &inline_schema) {
            (Some(schema), None)
                if schema.validate_published_records(
                    nodes.as_slice(),
                    self.occurrences.as_slice(),
                    self.facts.as_slice(),
                ) => {}
            (None, Some(schema))
                if schema.validate()
                    && schema.validate_published_records(
                        nodes.as_slice(),
                        self.occurrences.as_slice(),
                        self.facts.as_slice(),
                    ) => {}
            (Some(_) | None, None | Some(_)) => {
                return Err(DataError::InvalidDocument);
            }
        }
        // Amortized append growth may have retained more capacity than the published text needs; release that headroom.
        self.stored_text.shrink_to_fit();
        self.wide.shrink_to_fit();
        self.tags.shrink_to_fit();
        self.facts.shrink_to_fit();
        nodes.shrink_to_fit();
        let fact_owner_index = super::storage::FactOwnerIndex::build(self.facts.as_slice(), nodes.len())?;
        let storage = DocumentStorage {
            #[cfg(feature = "benchmark-internals")]
            schema_execution: self.schema_execution,
            format: self.format,
            dialect: self.dialect,
            key: self.key,
            root,
            coverage,
            text: super::DocumentTextStorage::new(self.source_binding, self.stored_text),
            _source: PhantomData,
            nodes,
            wide: self.wide,
            tags: self.tags,
            facts: self.facts,
            fact_owner_index,
            fact_owner_indexed: true,
            edges: arenas.edges,
            sidecars: arenas.sidecars,
            keys: arenas.keys,
            edge_refs: arenas.edge_refs,
            winners: arenas.winners,
            lookup: arenas.lookup,
            relationship_total: arenas.relationship_total,
            span_materializer: self.span_materializer,
            container_spans: self.container_spans,
            span_cache: self.span_cache,
            authored_spans: self.authored_spans,
        };
        let storage = match (shared_schema, inline_schema) {
            (Some(schema), None) => DocumentStorageOwner::new_accounted_shared(schema, storage),
            (None, Some(schema)) => DocumentStorageOwner::new_accounted_inline(schema, storage),
            (Some(_), Some(_)) | (None, None) => return Err(DataError::InvalidDocument),
        };
        Ok(Document {
            storage,
            borrowed_source: None,
            trusted_session_source_attachment: false,
            source_canonical: false,
        })
    }

    fn validate_attached_or_detached_source_text(
        &self,
        value: DocumentSourceText,
    ) -> Result<super::TextRef, DataError> {
        if self.source_binding.map(super::DocumentSourceBinding::seal) != Some(value.seal()) {
            return Err(DataError::InvalidDocument);
        }
        Ok(super::TextRef::Source(value.span()))
    }

    /// Admits one span into the sealed segment, either against the seal that already authenticates it or — when the
    /// codec seals its extent only after the last span is known — by retaining the extent [`Self::bind_source`] must
    /// then cover.
    fn validate_bound_source_span(&mut self, span: jqf_source::Span) -> Result<super::TextRef, DataError> {
        match self.source_binding.map(super::DocumentSourceBinding::seal) {
            Some(seal) => {
                seal.local_range(span).ok_or(DataError::InvalidDocument)?;
            }
            // A `Span` cannot be constructed with `start > end`, so its end alone carries the containment the range
            // check would prove.
            None => {
                self.unsealed_source_span_end = Some(self.unsealed_source_span_end.unwrap_or(0).max(span.end()));
            }
        }
        Ok(super::TextRef::Source(span))
    }

    fn resolve_stored_ref(&self, text: super::TextRef) -> Result<&str, DataError> {
        let super::TextRef::Stored(span) = text else {
            return Err(DataError::InvalidDocument);
        };
        self.stored_text
            .as_str()
            .get(span.start() as usize..span.end() as usize)
            .ok_or(DataError::InvalidDocument)
    }

    fn validate_node(&self, node: NodeId) -> Result<(), DataError> {
        if node.index() < self.logical_node_count() {
            Ok(())
        } else {
            Err(DataError::InvalidNode)
        }
    }

    fn validate_occurrence(&self, occurrence: OccurrenceId) -> Result<(), DataError> {
        if occurrence.index() < self.logical_occurrence_count() {
            Ok(())
        } else {
            Err(DataError::InvalidOccurrence)
        }
    }

    fn validate_owner(&self, owner: LocalOwnerRef) -> Result<(), DataError> {
        match owner {
            LocalOwnerRef::DocumentRoot => Ok(()),
            LocalOwnerRef::Node(node) => self.validate_node(node),
            LocalOwnerRef::Occurrence(occurrence) => self.validate_occurrence(occurrence),
        }
    }

    fn validate_occurrence_owner(&self, owner: LocalOwnerRef) -> Result<(), DataError> {
        match owner {
            LocalOwnerRef::DocumentRoot => Ok(()),
            LocalOwnerRef::Node(node) => self.validate_node(node),
            LocalOwnerRef::Occurrence(_) => Err(DataError::InvalidDocument),
        }
    }

    fn build_owner_occurrences(&mut self) -> Result<Vec<OccurrenceId>, DataError> {
        // Depth-first authoring (every codec) derives the adjacency in one stack pass; only order-breaking authoring
        // falls through to the counting-sort.
        if let Some(output) =
            super::adjacency::derive_accounted(self.nodes.as_mut_slice(), self.occurrences.as_slice())?
        {
            return Ok(output);
        }
        let mut counts = Vec::new();
        counts
            .try_reserve_exact(self.nodes.len())
            .map_err(jqf_resource::ResourceError::from)?;
        counts.resize(self.nodes.len(), 0usize);
        for occurrence in self.occurrences.as_slice() {
            match occurrence.owner {
                LocalOwnerRef::DocumentRoot => {}
                LocalOwnerRef::Node(node) => {
                    let count = counts
                        .as_mut_slice()
                        .get_mut(node.index())
                        .ok_or(DataError::InvalidNode)?;
                    *count = count.checked_add(1).ok_or(DataError::ArithmeticOverflow)?;
                }
                LocalOwnerRef::Occurrence(_) => return Err(DataError::InvalidDocument),
            }
        }
        let mut total = 0usize;
        for (node, count) in self.nodes.as_mut_slice().iter_mut().zip(counts.as_slice()) {
            node.occurrence_range = StorageRange::try_new(total, *count)?;
            total = total.checked_add(*count).ok_or(DataError::ArithmeticOverflow)?;
        }
        let mut output = Vec::new();
        output
            .try_reserve_exact(total)
            .map_err(jqf_resource::ResourceError::from)?;
        let zero = OccurrenceId::try_from_index(0).ok_or(DataError::ArithmeticOverflow)?;
        output.resize(total, zero);
        counts.as_mut_slice().fill(0);
        for (index, occurrence) in self.occurrences.as_slice().iter().enumerate() {
            if let LocalOwnerRef::Node(node) = occurrence.owner {
                let node_index = node.index();
                let cursor = counts
                    .as_mut_slice()
                    .get_mut(node_index)
                    .ok_or(DataError::InvalidNode)?;
                let range = self
                    .nodes
                    .as_slice()
                    .get(node_index)
                    .ok_or(DataError::InvalidNode)?
                    .occurrence_range;
                let destination = (range.start as usize)
                    .checked_add(*cursor)
                    .ok_or(DataError::ArithmeticOverflow)?;
                output.as_mut_slice()[destination] =
                    OccurrenceId::try_from_index(index).ok_or(DataError::ArithmeticOverflow)?;
                *cursor = cursor.checked_add(1).ok_or(DataError::ArithmeticOverflow)?;
            }
        }
        Ok(output)
    }

    pub(super) fn resolve_text_ref(&self, text: super::TextRef) -> Option<&str> {
        match text {
            super::TextRef::Stored(span) => self
                .stored_text
                .as_str()
                .get(span.start() as usize..span.end() as usize),
            super::TextRef::Source(span) => {
                // The source authority exists ONLY under the finalizer's `poll_with_source` (installed as
                // `finalization_source`); a plain `finish`/`poll` build has no live source bytes, so a source-backed
                // key misses here and the winner computation reports it as unrepresentable in THIS build.
                // Source-bearing codecs must use the cooperative `poll_with_source` path.
                let (pointer, length) = self.finalization_source?;
                // SAFETY: `poll_with_source` requires one immutable authority to
                // remain live for the complete call and installs this call-local view.
                let bytes = unsafe { core::slice::from_raw_parts(pointer, length) };
                let value = bytes.get(span.start() as usize..span.end() as usize)?;
                // SAFETY: a retained source span is UTF-8 either because its
                // DocumentSourceText token was validated when it was minted, or
                // because the caller of the bound-span admission API guaranteed
                // it there. Its containment in the sealed extent was proved at
                // admission when a seal was already bound and by bind_source
                // otherwise, which publication requires; poll_with_source proves
                // the bytes above are that same immutable authority.
                Some(unsafe { core::str::from_utf8_unchecked(value) })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    #[test]
    fn prepared_stored_decimal_keeps_authored_coefficient_and_scale() {
        let limits = jqf_resource::ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
        let control = jqf_resource::ContinueControl;
        let account = jqf_resource::RequestAccount::try_new(limits).expect("account allocates");
        let work = jqf_resource::WorkMeter::try_new_v1(4096).expect("work starts");
        let resources = jqf_resource::ResourceContext::new(account, &control, work).expect("context starts");
        let recipe =
            DocumentSchemaRecipe::try_new("test", None, &["test.decimal"], &[], &[], &[]).expect("recipe is valid");
        let (mut builder, prepared) =
            AccountedDocumentBuilder::try_new_prepared_with_coverage(&recipe, BuilderCoverage::complete())
                .expect("prepared builder starts");
        let kind = prepared.node_kind(0).expect("decimal kind exists");

        // Stored decimals keep trailing zeroes and a zero coefficient at nonzero scale so a codec can render the
        // authored spelling.
        let zero = builder.store_text("0", &resources).expect("coefficient stored");
        builder
            .add_prepared_stored_decimal_node(&prepared, kind, zero, 5, &resources)
            .expect("zero at nonzero scale is stored");
        let trailing_zero = builder.store_text("50", &resources).expect("coefficient stored");
        builder
            .add_prepared_stored_decimal_node(&prepared, kind, trailing_zero, 1, &resources)
            .expect("trailing-zero coefficient is stored");

        let not_digits = builder.store_text("1.5", &resources).expect("text stored");
        assert_eq!(
            builder.add_prepared_stored_decimal_node(&prepared, kind, not_digits, 1, &resources),
            Err(DataError::InvalidDocument)
        );
    }

    #[test]
    fn dynamic_schema_bindings_produce_correct_counts_without_staging() {
        let limits = jqf_resource::ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
        let control = jqf_resource::ContinueControl;
        let account = jqf_resource::RequestAccount::try_new(limits).expect("account allocates");
        let work = jqf_resource::WorkMeter::try_new_v1(4096).expect("work starts");
        let resources = jqf_resource::ResourceContext::new(account, &control, work).expect("context starts");
        let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder starts");
        let root = builder
            .add_node("test.root", AccountedSemanticNode::Null, None, &resources)
            .expect("root is added");
        let child = builder
            .add_node(
                "test.child",
                AccountedSemanticNode::Object {
                    member_role: "test.container",
                },
                Some(AccountedIntrinsicTag::Tagged("test.tag")),
                &resources,
            )
            .expect("child is added");
        builder
            .add_occurrence(LocalOwnerRef::DocumentRoot, "test.member", None, child, &resources)
            .expect("occurrence is added");
        builder
            .add_fact(
                LocalOwnerRef::Node(root),
                "test.fact.role",
                "test.fact.kind",
                1,
                &FactPayload::Null,
                &resources,
            )
            .expect("fact is added");

        let document = builder.finish(root, &resources).expect("document publishes");
        assert_eq!(document.storage.schema().counts(), (8, 2, 2, 1, 1));
    }
    #[test]
    fn staged_blocks_flush_at_boundaries_and_preserve_authoring_order() {
        // The staging scratch defers arena writes to per-block flushes: the PHYSICAL arena holds only flushed blocks
        // while the LOGICAL count (the public `node_count`/`occurrence_count`) reads the staged window, and finish
        // publishes every record in authoring order. The block boundary is the load-bearing row: crossing `STAGE_BLOCK`
        // must flush exactly one block and leave the trailing partial staged.
        let limits = jqf_resource::ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
        let control = jqf_resource::ContinueControl;
        let account = jqf_resource::RequestAccount::try_new(limits).expect("account allocates");
        let work = jqf_resource::WorkMeter::try_new_v1(4096).expect("work starts");
        let resources = jqf_resource::ResourceContext::new(account, &control, work).expect("context starts");
        let recipe = DocumentSchemaRecipe::try_new("test", None, &["test.item"], &["test.items"], &[], &[])
            .expect("recipe is valid");
        let (mut builder, prepared) =
            AccountedDocumentBuilder::try_new_prepared_with_coverage(&recipe, BuilderCoverage::complete())
                .expect("prepared builder starts");
        let root = builder
            .add_prepared_node(
                &prepared,
                prepared.node_kind(0).expect("null kind exists"),
                PreparedSemanticNode::Null,
                &resources,
            )
            .expect("root is added");
        let array = builder
            .add_prepared_node(
                &prepared,
                prepared.node_kind(0).expect("null kind exists"),
                PreparedSemanticNode::Array(prepared.occurrence_role(0).expect("role exists")),
                &resources,
            )
            .expect("array is added");
        let kind = prepared.node_kind(0).expect("null kind exists");
        let role = prepared.occurrence_role(0).expect("role exists");
        let extra = STAGE_BLOCK * 3 + 17;
        for _ in 0..extra {
            let leaf = builder
                .add_prepared_node(&prepared, kind, PreparedSemanticNode::Null, &resources)
                .expect("leaf is added");
            builder
                .add_prepared_occurrence(&prepared, LocalOwnerRef::Node(array), role, None, leaf, &resources)
                .expect("occurrence is added");
        }
        let logical_nodes = 2 + extra;
        let logical_occurrences = extra;
        assert_eq!(builder.node_count(), logical_nodes);
        assert_eq!(builder.occurrence_count(), logical_occurrences);
        // The staging law: physical + staged == logical on both arenas, the staged remainder is always below one block,
        // and more than one block's worth was flushed (1553 records cannot sit in one scratch). Flush sizes are 512 or
        // 511: the trigger fires when a scratch reaches `STAGE_BLOCK`, and the node-before-occurrence push order drains
        // the trailing scratch at 511, so neither arena's length is asserted to a multiple.
        assert_eq!(builder.nodes.len() + builder.staged_nodes.len(), logical_nodes);
        assert_eq!(
            builder.occurrences.len() + builder.staged_occurrences.len(),
            logical_occurrences
        );
        assert!(builder.staged_nodes.len() < STAGE_BLOCK);
        assert!(builder.staged_occurrences.len() < STAGE_BLOCK);
        assert!(builder.nodes.len() >= STAGE_BLOCK);
        assert!(builder.occurrences.len() >= STAGE_BLOCK);
        let document = builder.finish(root, &resources).expect("document publishes");
        // The final flush published every staged record in authoring order: the node arena holds the logical node
        // count, and the array node's published occurrence range covers every occurrence the build staged (the ranges
        // are derived from the occurrence arena at finalize, so this proves the occurrences flushed too).
        assert_eq!(document.storage.nodes.len(), logical_nodes);
        assert_eq!(
            document.storage.nodes[array.index()].occurrence_range.len as usize,
            logical_occurrences
        );
    }

    #[cfg(feature = "benchmark-internals")]
    #[test]
    fn prepared_route_proof_distinguishes_dynamic_and_mixed_routes() {
        let limits = jqf_resource::ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
        let control = jqf_resource::ContinueControl;

        let dynamic_account = jqf_resource::RequestAccount::try_new(limits).expect("dynamic account allocates");
        let dynamic_work = jqf_resource::WorkMeter::try_new_v1(4096).expect("dynamic work starts");
        let dynamic_resources = jqf_resource::ResourceContext::new(dynamic_account, &control, dynamic_work)
            .expect("dynamic context starts");
        let mut dynamic = AccountedDocumentBuilder::try_new("test", None).expect("dynamic builder starts");
        let dynamic_root = dynamic
            .add_node("test.null", AccountedSemanticNode::Null, None, &dynamic_resources)
            .expect("dynamic root is added");
        dynamic
            .add_node("test.null", AccountedSemanticNode::Null, None, &dynamic_resources)
            .expect("existing-schema fast append succeeds");
        let dynamic_document = dynamic
            .finish(dynamic_root, &dynamic_resources)
            .expect("dynamic document publishes");
        let dynamic_stats = dynamic_document.benchmark_storage_layout_stats();
        assert!(!dynamic_stats.prepared_schema_only);
        assert_eq!(dynamic_stats.prepared_schema_recipe_fingerprint, None);
        assert_eq!(dynamic_stats.prepared_append_count, 0);
        assert_eq!(dynamic_stats.dynamic_append_count, 2);
        assert_eq!(dynamic_stats.dynamic_existing_schema_fast_append_count, 1);
        assert_eq!(dynamic_stats.dynamic_schema_transaction_append_count, 1);

        let recipe = DocumentSchemaRecipe::try_new("test", None, &["test.null", "test.bool"], &[], &[], &[])
            .expect("recipe is valid");
        let mixed_account = jqf_resource::RequestAccount::try_new(limits).expect("mixed account allocates");
        let mixed_work = jqf_resource::WorkMeter::try_new_v1(4096).expect("mixed work starts");
        let mixed_resources =
            jqf_resource::ResourceContext::new(mixed_account, &control, mixed_work).expect("mixed context starts");
        let (mut mixed, prepared) =
            AccountedDocumentBuilder::try_new_prepared_with_coverage(&recipe, BuilderCoverage::complete())
                .expect("prepared builder starts");
        let mixed_root = mixed
            .add_prepared_node(
                &prepared,
                prepared.node_kind(0).expect("prepared null kind exists"),
                PreparedSemanticNode::Null,
                &mixed_resources,
            )
            .expect("prepared root is added");
        mixed
            .add_node("test.bool", AccountedSemanticNode::Bool(true), None, &mixed_resources)
            .expect("dynamic append succeeds on a one-shot prepared builder");
        let mixed_document = mixed
            .finish(mixed_root, &mixed_resources)
            .expect("mixed document publishes");
        let mixed_stats = mixed_document.benchmark_storage_layout_stats();
        assert!(!mixed_stats.prepared_schema_only);
        assert_eq!(
            mixed_stats.prepared_schema_recipe_fingerprint,
            Some(recipe.fingerprint())
        );
        assert_eq!(mixed_stats.prepared_append_count, 1);
        assert_eq!(mixed_stats.dynamic_append_count, 1);
        assert!(mixed_stats.prepared_builder_accounted);
    }
    #[test]
    fn prepared_handles_are_document_bound_before_append() {
        let recipe = DocumentSchemaRecipe::try_new("test", None, &["test.null"], &[], &[], &[])
            .expect("fixture recipe is valid");
        let limits = jqf_resource::ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
        let control = jqf_resource::ContinueControl;
        let first_account = jqf_resource::RequestAccount::try_new(limits).expect("account allocates");
        let first_work = jqf_resource::WorkMeter::try_new_v1(4096).expect("work starts");
        let first_resources =
            jqf_resource::ResourceContext::new(first_account, &control, first_work).expect("context starts");
        let (mut first, first_schema) =
            AccountedDocumentBuilder::try_new_prepared_with_coverage(&recipe, BuilderCoverage::complete())
                .expect("first builder starts");
        let first_kind = first_schema.node_kind(0).expect("kind exists");

        let (mut second, second_schema) =
            AccountedDocumentBuilder::try_new_prepared_with_coverage(&recipe, BuilderCoverage::complete())
                .expect("second builder starts");

        assert!(
            second
                .add_prepared_node(&second_schema, first_kind, PreparedSemanticNode::Null, &first_resources,)
                .is_err(),
            "a handle from another prepared set must be rejected"
        );
        let root = first
            .add_prepared_node(&first_schema, first_kind, PreparedSemanticNode::Null, &first_resources)
            .expect("valid handle remains appendable at index zero");
        assert_eq!(root.index(), 0);
    }
    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one ownership test keeps the complete prototype, document-key, lifetime, and teardown law visible"
    )]
    fn schema_prototype_shares_only_immutable_schema_across_fresh_documents() {
        let recipe = DocumentSchemaRecipe::try_new("test", Some("shared"), &["test.null"], &[], &[], &[])
            .expect("fixture recipe is valid");
        let limits = jqf_resource::ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
        let control = jqf_resource::ContinueControl;
        let account = jqf_resource::RequestAccount::try_new(limits).expect("account allocates");
        let work = jqf_resource::WorkMeter::try_new_v1(4096).expect("work starts");
        let mut resources = jqf_resource::ResourceContext::new(account, &control, work).expect("context starts");
        let live_account_baseline = resources.snapshot();
        let prototype = crate::DocumentSchemaPrototype::try_new(&recipe).expect("prototype is prepared once");

        let (mut immutable, immutable_schema) = prototype
            .try_new_builder_with_coverage(BuilderCoverage::complete())
            .expect("prototype-backed builder starts");
        assert_eq!(
            immutable
                .add_node("test.dynamic", AccountedSemanticNode::Null, None, &resources,)
                .expect_err("a shared immutable schema cannot grow"),
            DataError::InvalidDocument
        );
        assert_eq!(immutable.node_count(), 0);
        let _ = immutable_schema;
        let _ = immutable;

        let (mut first, first_schema) = prototype
            .try_new_builder_with_coverage(BuilderCoverage::complete())
            .expect("first document builder starts");
        let first_kind = first_schema.node_kind(0).expect("null kind exists");
        let first_root = first
            .add_prepared_node(&first_schema, first_kind, PreparedSemanticNode::Null, &resources)
            .expect("first root appends");
        let first = first.finish(first_root, &resources).expect("first document publishes");

        let (mut second, second_schema) = prototype
            .try_new_builder_with_coverage(BuilderCoverage::complete())
            .expect("second document builder starts");
        assert_eq!(
            second
                .add_prepared_node(&first_schema, first_kind, PreparedSemanticNode::Null, &resources,)
                .expect_err("a prepared handle cannot cross document keys"),
            DataError::InvalidDocument
        );
        assert_eq!(second.node_count(), 0);
        let second_root = second
            .add_prepared_node(
                &second_schema,
                second_schema.node_kind(0).expect("null kind exists"),
                PreparedSemanticNode::Null,
                &resources,
            )
            .expect("second root appends");
        let second = second
            .finish(second_root, &resources)
            .expect("second document publishes");
        let _ = second_schema;

        let other_prototype = crate::DocumentSchemaPrototype::try_new(&recipe).expect("second prototype is prepared");
        let (mut foreign, foreign_schema) = other_prototype
            .try_new_builder_with_coverage(BuilderCoverage::complete())
            .expect("foreign-prototype builder starts");
        assert_eq!(
            foreign
                .add_prepared_node(&first_schema, first_kind, PreparedSemanticNode::Null, &resources,)
                .expect_err("a prepared handle cannot cross prototype identities"),
            DataError::InvalidDocument
        );
        assert_eq!(foreign.node_count(), 0);
        let _ = foreign_schema;
        let _ = foreign;
        let _ = other_prototype;
        let _ = first_schema;
        let _ = prototype;

        assert_ne!(first.key(), second.key());
        assert_ne!(first.root_handle(), second.root_handle());
        assert!(
            first.storage.shares_schema_allocation_with(&second.storage),
            "documents from one prototype must retain one immutable schema allocation"
        );
        assert!(matches!(
            first.materialize_root(&mut resources).expect("first remains readable"),
            crate::Value::Null
        ));
        assert!(matches!(
            second
                .materialize_root(&mut resources)
                .expect("second remains readable"),
            crate::Value::Null
        ));
        drop(first);
        drop(second);
        assert_eq!(
            resources.snapshot().memory_current_bytes(),
            live_account_baseline.memory_current_bytes(),
            "dropping every document and prototype owner returns to the live account baseline"
        );
    }

    /// A prototype is bound to no request account. Construction takes only the coverage demand; the ambient allocator
    /// charges a builder's bytes wherever they land.
    #[test]
    fn a_prototype_instantiates_a_builder() {
        let recipe = DocumentSchemaRecipe::try_new("test", None, &["test.null"], &[], &[], &[])
            .expect("fixture recipe is valid");
        let prototype = crate::DocumentSchemaPrototype::try_new(&recipe).expect("prototype is prepared");
        prototype
            .try_new_builder_with_coverage(BuilderCoverage::complete())
            .expect("prototype instantiates a builder");
    }
    /// A duplicate-heavy object must not retain discarded topology capacity: `emit_relationship_arenas_accounted`
    /// reserves `edge_refs`/`winners`/`lookup` at the projection upper bound (every owned occurrence, including every
    /// duplicate object member before winner grouping collapses them), so the arenas keep that slack unless publication
    /// compacts them.
    #[test]
    fn accounted_publish_compacts_winners_capacity_after_duplicate_keys() {
        let limits = jqf_resource::ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
        let control = jqf_resource::ContinueControl;
        let account = jqf_resource::RequestAccount::try_new(limits).expect("account allocates");
        let work = jqf_resource::WorkMeter::try_new_v1(4096).expect("work starts");
        let resources = jqf_resource::ResourceContext::new(account, &control, work).expect("context starts");
        let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder starts");
        let root = builder
            .add_node(
                "test.object",
                AccountedSemanticNode::Object {
                    member_role: "test.member",
                },
                None,
                &resources,
            )
            .expect("root is added");
        let value = builder
            .add_node("test.bool", AccountedSemanticNode::Bool(true), None, &resources)
            .expect("value is added");
        // Eight occurrences under one duplicate key: winner grouping collapses them to the one last-value-wins entry
        // the repro names.
        for _ in 0..8 {
            builder
                .add_occurrence(
                    LocalOwnerRef::Node(root),
                    "test.member",
                    Some(AccountedOccurrenceKey::Text("same")),
                    value,
                    &resources,
                )
                .expect("duplicate-key member is added");
        }

        let document = builder.finish(root, &resources).expect("document publishes");
        assert_eq!(document.storage.winners.len(), 1);
        assert_eq!(document.storage.winners.capacity(), 1);
        // One winner is at or below the small-object threshold: the lookup segment is retained (the shared projection
        // range stays aligned) but never sorted.
        assert_eq!(document.storage.lookup.len(), 1);
        assert_eq!(document.storage.edge_refs.capacity(), 0);
    }

    /// A codec that learns the extent its document names only when its root value ends may admit source spans before it
    /// seals that extent, but the seal it eventually binds still has to cover every one of them — and a builder that
    /// binds none cannot publish spans nothing authenticated.
    #[test]
    fn a_source_span_admitted_before_the_seal_must_be_covered_by_it() {
        fn context() -> jqf_resource::ResourceContext<'static> {
            static CONTROL: jqf_resource::ContinueControl = jqf_resource::ContinueControl;
            let limits = jqf_resource::ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
            let account = jqf_resource::RequestAccount::try_new(limits).expect("account allocates");
            let work = jqf_resource::WorkMeter::try_new_v1(4096).expect("work starts");
            jqf_resource::ResourceContext::new(account, &CONTROL, work).expect("context starts")
        }

        // `["alpha"]` — the span below names the five bytes `alpha` inside it, so a seal is wide enough exactly from
        // length seven upwards.
        const BYTES: &[u8] = br#"["alpha"]"#;
        let span = jqf_source::Span::new(2, 7);
        let seal_over = |len: usize| {
            let source = jqf_source::ResolvedSource::new(
                jqf_source::SourceRef::new(jqf_source::SourceId::new(9), jqf_source::SourceKind::Input),
                "fixture",
                &BYTES[..len],
                0,
            );
            super::super::DocumentSourceBinding::from_resolved(source).expect("seal is taken")
        };
        let recipe = DocumentSchemaRecipe::try_new("test", None, &["test.string"], &[], &[], &[])
            .expect("fixture recipe is valid");
        // Each case below reaches a terminal guard, so each needs a builder that has admitted the span and bound
        // nothing yet.
        let admitted = |resources: &jqf_resource::ResourceContext<'_>| {
            let prototype = crate::DocumentSchemaPrototype::try_new(&recipe).expect("prototype is prepared");
            let (mut builder, schema) = prototype
                .try_new_builder_with_coverage(BuilderCoverage::complete())
                .expect("builder starts");
            let kind = schema.node_kind(0).expect("string kind exists");
            // SAFETY: the span names UTF-8 inside the immutable fixture every
            // seal here is taken over, which is what the binding authenticates.
            let root = unsafe { builder.add_prepared_bound_source_string_node(&schema, kind, span, resources) }
                .expect("a span may be admitted before the seal that authenticates it");
            (builder, root)
        };

        let resources = context();
        let (mut builder, root) = admitted(&resources);
        // A seal ending inside the span leaves those bytes unauthenticated.
        assert_eq!(
            builder
                .bind_source(seal_over(5))
                .expect_err("a seal shorter than an admitted span is refused"),
            DataError::InvalidDocument
        );
        // No seal at all is the same failure, caught where the document would otherwise become visible.
        assert_eq!(
            builder
                .begin_finish(root, &resources)
                .err()
                .expect("an unsealed source span cannot be published"),
            DataError::InvalidDocument
        );

        // The one-shot publication route carries its own copy of that guard, so it is asserted directly rather than
        // through the cooperative one.
        let resources = context();
        let (builder, root) = admitted(&resources);
        assert_eq!(
            builder
                .finish(root, &resources)
                .expect_err("an unsealed source span cannot be published"),
            DataError::InvalidDocument
        );

        // The exact fit: a seal that ends precisely where the admitted span does covers it. Containment is `end <=
        // length`, and an off-by-one here would refuse every document whose last value reaches its final byte — which
        // is every document a codec seals at its root's end.
        let resources = context();
        let (mut builder, root) = admitted(&resources);
        builder
            .bind_source(seal_over(7))
            .expect("a seal ending exactly at the admitted span's end covers it");
        builder
            .finish(root, &resources)
            .expect("an exactly-covered document publishes");

        let resources = context();
        let (mut builder, root) = admitted(&resources);
        builder
            .bind_source(seal_over(BYTES.len()))
            .expect("a seal covering every admitted span binds");
        builder
            .begin_finish(root, &resources)
            .expect("a sealed document finalizes");
    }

    /// The tag-layer law: a kindless node carrying a non-core intrinsic tag owns exactly one keyless payload
    /// occurrence, and materialization turns the chain into nested `Value::Tagged` wrappers (the representation for
    /// CBOR tag chains).
    #[test]
    fn tag_layer_chain_materializes_nested_tagged_values() {
        let mut resources = tag_layer_context();
        let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder starts");
        let outer = builder
            .add_node(
                "test.layer",
                AccountedSemanticNode::Unrepresentable,
                Some(AccountedIntrinsicTag::Tagged("outer.tag")),
                &resources,
            )
            .expect("outer layer node is added");
        let inner = builder
            .add_node(
                "test.layer",
                AccountedSemanticNode::Unrepresentable,
                Some(AccountedIntrinsicTag::Tagged("inner.tag")),
                &resources,
            )
            .expect("inner layer node is added");
        let payload = builder
            .add_node(
                "test.array",
                AccountedSemanticNode::Array { item_role: "test.item" },
                None,
                &resources,
            )
            .expect("payload array is added");
        let item = builder
            .add_node("test.scalar", AccountedSemanticNode::Integer("1"), None, &resources)
            .expect("item is added");
        builder
            .add_occurrence(LocalOwnerRef::Node(payload), "test.item", None, item, &resources)
            .expect("payload item is added");
        builder
            .add_occurrence(
                LocalOwnerRef::Node(inner),
                "test.tag-payload",
                None,
                payload,
                &resources,
            )
            .expect("inner payload is added");
        builder
            .add_occurrence(LocalOwnerRef::Node(outer), "test.tag-payload", None, inner, &resources)
            .expect("outer payload is added");

        let document = builder.finish(outer, &resources).expect("document publishes");
        let value = document
            .materialize_root(&mut resources)
            .expect("the chain materializes");
        assert_eq!(tag_chain(&value), ["outer.tag", "inner.tag"]);
        assert_eq!(value.untagged().kind(), crate::ValueKind::Array);
    }

    /// The builder contract admits shared edges, so a tag layer may point at itself. `payload_view` descends layers; a
    /// cycle must raise — never hang the reader.
    #[test]
    fn a_self_referential_tag_layer_raises_instead_of_hanging_payload_view() {
        let resources = tag_layer_context();
        let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder starts");
        let layer = builder
            .add_node(
                "test.layer",
                AccountedSemanticNode::Unrepresentable,
                Some(AccountedIntrinsicTag::Tagged("loop.tag")),
                &resources,
            )
            .expect("layer node is added");
        builder
            .add_occurrence(LocalOwnerRef::Node(layer), "test.tag-payload", None, layer, &resources)
            .expect("self payload edge is added");
        let document = builder.finish(layer, &resources).expect("document publishes");
        assert!(matches!(
            document.payload_view(document.root_handle()),
            Err(DataError::CyclicSemanticGraph)
        ));
    }

    /// A single tag wrapping a representable payload stays on the payload node (the pre-existing single-tag form),
    /// which is unchanged by the layer law.
    #[test]
    fn single_tag_wraps_a_representable_payload_node() {
        let mut resources = tag_layer_context();
        let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder starts");
        let node = builder
            .add_node(
                "test.scalar",
                AccountedSemanticNode::Integer("7"),
                Some(AccountedIntrinsicTag::Tagged("one.tag")),
                &resources,
            )
            .expect("tagged scalar is added");
        let document = builder.finish(node, &resources).expect("document publishes");
        let value = document
            .materialize_root(&mut resources)
            .expect("the tagged scalar materializes");
        assert_eq!(tag_chain(&value), ["one.tag"]);
        assert_eq!(value.untagged().kind(), crate::ValueKind::Number);
    }

    /// A layer with no payload cannot materialize: the projection is empty and the layer's exactly-one-payload law is
    /// violated.
    #[test]
    fn tag_layer_without_payload_fails_materialization() {
        let mut resources = tag_layer_context();
        let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder starts");
        let layer = builder
            .add_node(
                "test.layer",
                AccountedSemanticNode::Unrepresentable,
                Some(AccountedIntrinsicTag::Tagged("lone.tag")),
                &resources,
            )
            .expect("layer node is added");
        let document = builder.finish(layer, &resources).expect("document publishes");
        assert_eq!(
            document
                .materialize_root(&mut resources)
                .expect_err("a payloadless layer cannot materialize"),
            crate::DataError::InvalidDocument
        );
    }

    /// A layer owning TWO payload occurrences cannot materialize: the exactly-one-payload law is enforced at
    /// materialization time.
    #[test]
    fn tag_layer_with_multiple_payloads_fails_materialization() {
        let mut resources = tag_layer_context();
        let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder starts");
        let layer = builder
            .add_node(
                "test.layer",
                AccountedSemanticNode::Unrepresentable,
                Some(AccountedIntrinsicTag::Tagged("multi.tag")),
                &resources,
            )
            .expect("layer node is added");
        let first = builder
            .add_node("test.scalar", AccountedSemanticNode::Null, None, &resources)
            .expect("first payload is added");
        let second = builder
            .add_node("test.scalar", AccountedSemanticNode::Null, None, &resources)
            .expect("second payload is added");
        builder
            .add_occurrence(LocalOwnerRef::Node(layer), "test.tag-payload", None, first, &resources)
            .expect("first payload occurrence is added");
        builder
            .add_occurrence(LocalOwnerRef::Node(layer), "test.tag-payload", None, second, &resources)
            .expect("second payload occurrence is added");
        let document = builder.finish(layer, &resources).expect("document publishes");
        assert_eq!(
            document
                .materialize_root(&mut resources)
                .expect_err("a two-payload layer cannot materialize"),
            crate::DataError::InvalidDocument
        );
    }

    /// A keyed occurrence on a tag-layer node is rejected at publication: a layer owns exactly one keyless payload.
    #[test]
    fn tag_layer_rejects_keyed_payload_at_publication() {
        let resources = tag_layer_context();
        let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder starts");
        let layer = builder
            .add_node(
                "test.layer",
                AccountedSemanticNode::Unrepresentable,
                Some(AccountedIntrinsicTag::Tagged("keyed.tag")),
                &resources,
            )
            .expect("layer node is added");
        let payload = builder
            .add_node("test.scalar", AccountedSemanticNode::Null, None, &resources)
            .expect("payload is added");
        builder
            .add_occurrence(
                LocalOwnerRef::Node(layer),
                "test.tag-payload",
                Some(AccountedOccurrenceKey::Text("keyed")),
                payload,
                &resources,
            )
            .expect("keyed occurrence is added");
        assert_eq!(
            builder
                .finish(layer, &resources)
                .expect_err("keyed layer cannot publish"),
            crate::DataError::InvalidDocument
        );
    }

    fn tag_layer_context() -> jqf_resource::ResourceContext<'static> {
        static CONTROL: jqf_resource::ContinueControl = jqf_resource::ContinueControl;
        let limits = jqf_resource::ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
        let account = jqf_resource::RequestAccount::try_new(limits).expect("account allocates");
        let work = jqf_resource::WorkMeter::try_new_v1(4096).expect("work starts");
        jqf_resource::ResourceContext::new(account, &CONTROL, work).expect("context starts")
    }

    fn tag_chain(value: &crate::Value) -> Vec<&str> {
        let mut chain = Vec::new();
        let mut current = value;
        while let crate::Value::Tagged { tag, payload } = current {
            chain.push(tag.as_str());
            current = &**payload;
        }
        chain
    }

    /// A node whose kind fails schema binding must strand no bytes: the compact text arena is appended only AFTER
    /// resolution binds, so a refused admission leaves the arena exactly as it was.
    #[test]
    fn refused_node_binding_appends_no_text_arena_bytes() {
        let resources = tag_layer_context();
        let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder starts");
        let refused = builder.add_node(
            "bad kind",
            AccountedSemanticNode::String("payload that must never land"),
            None,
            &resources,
        );
        assert!(matches!(refused, Err(crate::DataError::InvalidDocument)));
        assert!(
            builder.stored_text.is_empty(),
            "refused binding left {} arena bytes behind",
            builder.stored_text.len()
        );
    }
}
