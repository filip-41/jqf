//! The published document and its arenas.
//!
//! [`Document`] holds node records, occurrence edges, intrinsic tags, and optional source text. `edges` / `sidecars` /
//! `keys` are addressed by occurrence id. `edge_refs` / `winners` / `lookup` are addressed by each node's
//! `projection_range`.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::{fmt, marker::PhantomData, ops::Deref};

use crate::{DialectId, FormatId, LocalDate, TagId, Value, ValueKind};
use jqf_resource::{ControlError, CooperativeError, ResourceContext, ResourceError};
#[cfg(feature = "benchmark-internals")]
use jqf_source::SourceRef;
use jqf_source::{ResolvedSource, Span};

#[cfg(feature = "benchmark-internals")]
use super::SourceSnapshotSeal;
use super::{
    DialectBindingId, DocumentCapability, DocumentCoverage, DocumentFact, DocumentId, DocumentSchema,
    DocumentTextStorage, DocumentTextStorageStats, FactId, FactKindBindingId, FactPayloadView, FactRoleBindingId,
    FormatBindingId, LocalOwnerRef, NodeHandle, NodeId, NodeKindBindingId, OccurrenceId, OccurrenceRoleBindingId,
    StoredDocumentFact, TextRef, ValidatedSourceBacking,
};

#[cfg(feature = "benchmark-internals")]
use super::SchemaExecution;

/// Benchmark-only observation of current private document table capacities.
///
/// This surface exists only with the `benchmark-internals` feature and carries aggregate counts and byte totals rather
/// than private record values.
#[cfg(feature = "benchmark-internals")]
#[doc(hidden)]
#[allow(
    missing_docs,
    clippy::struct_excessive_bools,
    reason = "the flat benchmark diagnostic reports independent observed route facts"
)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DocumentStorageLayoutStats {
    pub prepared_schema_only: bool,
    pub prepared_schema_recipe_fingerprint: Option<u64>,
    pub prepared_builder_accounted: bool,
    pub prepared_append_count: u64,
    pub dynamic_append_count: u64,
    pub dynamic_existing_schema_fast_append_count: u64,
    pub dynamic_schema_transaction_append_count: u64,
    pub canonical_identity_count: usize,
    pub canonical_identity_utf8_bytes: usize,
    pub node_kind_binding_count: usize,
    pub occurrence_role_binding_count: usize,
    pub fact_kind_binding_count: usize,
    pub fact_role_binding_count: usize,
    pub identity_table_shallow_bytes: usize,
    pub node_record_bytes: usize,
    pub occurrence_record_bytes: usize,
    pub stored_fact_record_bytes: usize,
    pub source_reference_count: usize,
    pub source_span_sum_bytes: usize,
    pub source_span_union_bytes: usize,
    pub source_identity_present: bool,
    pub physical_source_backing: bool,
    pub text_ref_size: usize,
    pub stored_occurrence_key_size: usize,
    pub node_len: usize,
    pub node_capacity: usize,
    pub occurrence_len: usize,
    pub occurrence_capacity: usize,
    pub array_projection_len: usize,
    pub array_projection_capacity: usize,
    pub object_projection_len: usize,
    pub object_projection_capacity: usize,
    pub object_projection_index_len: usize,
    pub object_projection_index_capacity: usize,
    pub fact_len: usize,
    pub fact_capacity: usize,
    pub decoded_text_arena_capacity_bytes: usize,
    pub node_table_capacity_bytes: usize,
    pub occurrence_table_capacity_bytes: usize,
    pub array_projection_capacity_bytes: usize,
    pub object_projection_capacity_bytes: usize,
    pub object_projection_index_capacity_bytes: usize,
    pub fact_table_capacity_bytes: usize,
    pub shallow_table_capacity_bytes: usize,
}

/// Which container an unmaterialized subtree span holds.
///
/// A container span names validated source bytes instead of a built subtree, so the node's payload-transparent category
/// has to be carried explicitly: the occurrences that would otherwise answer "array or object" were never built.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContainerSpanKind {
    /// The span holds one complete array text in the document's own format.
    Array,
    /// The span holds one complete object text in the document's own format.
    Object,
}

impl ContainerSpanKind {
    /// The payload-transparent semantic category of the deferred container.
    #[must_use]
    pub const fn value_kind(self) -> ValueKind {
        match self {
            Self::Array => ValueKind::Array,
            Self::Object => ValueKind::Object,
        }
    }
}

/// Builder-facing semantic payload, later mapped by `place_semantic` into the inline header niche or the wide side
/// arena.
pub(crate) enum StoredSemanticNode {
    Null,
    Bool(bool),
    StoredInteger(TextRef),
    StoredDecimal {
        coefficient: TextRef,
        scale: i64,
    },
    AccountedFloat(crate::Float),
    Text(TextRef),
    AccountedBytes(Vec<u8>),
    LocalDate(LocalDate),
    AccountedLocalTime(AccountedLocalTime),
    AccountedLocalDateTime(AccountedLocalDateTime),
    AccountedOffsetDateTime(AccountedOffsetDateTime),
    Array {
        item_role: OccurrenceRoleBindingId,
    },
    Object {
        member_role: OccurrenceRoleBindingId,
    },
    /// A container subtree left UNBUILT, named by a span of the sealed source.
    ///
    /// Carries no occurrences. Projecting it fails with [`DataError::UnmaterializedContainerSpan`].
    ContainerSpan {
        text: TextRef,
        container: ContainerSpanKind,
    },
    Unrepresentable,
}

/// Request-accounted local wall-clock time payload; the fraction digits are the only heap-owning field.
pub(crate) struct AccountedLocalTime {
    pub(crate) hour: u8,
    pub(crate) minute: u8,
    pub(crate) second: u8,
    pub(crate) fraction: String,
}

/// Request-accounted local date-time payload.
pub(crate) struct AccountedLocalDateTime {
    pub(crate) date: LocalDate,
    pub(crate) time: AccountedLocalTime,
}

/// Request-accounted offset date-time payload.
pub(crate) struct AccountedOffsetDateTime {
    pub(crate) local: AccountedLocalDateTime,
    pub(crate) offset: crate::UtcOffset,
}

/// Compact index into the document `wide` side arena.
#[derive(Clone, Copy)]
pub(crate) struct WidePayloadId(pub(crate) u32);

/// Compact stored semantic payload held inline in each fixed [`NodeRecord`] header.
///
/// Small, common scalar payloads (bool, the canonical number handle, a text reference, a stored integer, a float, a
/// local date) plus the container role ids stay inline as niches. Large or uncommon scalar payloads move to the typed
/// `wide` side arena and are addressed here by a compact [`WidePayloadId`], keeping the header dense for topology
/// traversal.
pub(crate) enum NodeSemantic {
    Null,
    Bool(bool),
    StoredInteger(TextRef),
    AccountedFloat(crate::Float),
    Text(TextRef),
    LocalDate(LocalDate),
    Array {
        item_role: OccurrenceRoleBindingId,
    },
    Object {
        member_role: OccurrenceRoleBindingId,
    },
    /// A container subtree the decode deliberately did NOT build, held as a span of the sealed, already-validated
    /// source it occupies.
    ///
    /// The node exists, carries its payload-transparent category, and costs one node record — the span bookkeeping
    /// requires it to be charged — while the subtree's own nodes, occurrences and relationship arenas are never
    /// built. A toucher materializes it through [`LazySpanMaterializer`](super::LazySpanMaterializer), which hands the
    /// toucher an independent owned value; nothing is written back, so no second handle to a materialized subtree is
    /// ever created.
    ContainerSpan {
        text: TextRef,
        container: ContainerSpanKind,
    },
    Unrepresentable,
    /// A large/uncommon scalar payload held in the `wide` side arena. The payload's semantic category is cached here so
    /// category queries stay header-local.
    Wide {
        id: WidePayloadId,
        kind: ValueKind,
    },
}

impl NodeSemantic {
    pub(crate) fn kind(&self) -> Option<ValueKind> {
        Some(match self {
            Self::Null => ValueKind::Null,
            Self::Bool(_) => ValueKind::Bool,
            Self::StoredInteger(_) | Self::AccountedFloat(_) => ValueKind::Number,
            Self::Text(_) => ValueKind::String,
            Self::LocalDate(_) => ValueKind::LocalDate,
            Self::Array { .. } => ValueKind::Array,
            Self::Object { .. } => ValueKind::Object,
            Self::ContainerSpan { container, .. } => container.value_kind(),
            Self::Wide { kind, .. } => *kind,
            Self::Unrepresentable => return None,
        })
    }
}

/// Large or uncommon scalar payload stored in the typed `wide` side arena so the fixed [`NodeRecord`] header stays
/// compact. Each is addressed by the owning node's [`NodeSemantic::Wide`] index.
pub(crate) enum WidePayload {
    StoredDecimal { coefficient: TextRef, scale: i64 },
    AccountedBytes(Vec<u8>),
    AccountedLocalTime(AccountedLocalTime),
    AccountedLocalDateTime(AccountedLocalDateTime),
    AccountedOffsetDateTime(AccountedOffsetDateTime),
}

impl WidePayload {
    /// The semantic category of this wide payload, cached in the owning node's [`NodeSemantic::Wide`] header so
    /// category queries never touch the arena.
    pub(crate) fn kind(&self) -> ValueKind {
        match self {
            Self::StoredDecimal { .. } => ValueKind::Number,
            Self::AccountedBytes(_) => ValueKind::Bytes,
            Self::AccountedLocalTime(_) => ValueKind::LocalTime,
            Self::AccountedLocalDateTime(_) => ValueKind::LocalDateTime,
            Self::AccountedOffsetDateTime(_) => ValueKind::OffsetDateTime,
        }
    }
}

/// Placement of a freshly built [`StoredSemanticNode`] into either the inline header niche or the typed `wide` side
/// arena.
pub(crate) enum PlacedSemantic {
    Inline(NodeSemantic),
    Wide(WidePayload),
}

/// Classifies a transient built payload into an inline header niche or a wide side-arena payload. The builders assign
/// the arena index at push time.
pub(crate) fn place_semantic(semantic: StoredSemanticNode) -> PlacedSemantic {
    match semantic {
        StoredSemanticNode::Null => PlacedSemantic::Inline(NodeSemantic::Null),
        StoredSemanticNode::Bool(value) => PlacedSemantic::Inline(NodeSemantic::Bool(value)),
        StoredSemanticNode::StoredInteger(value) => PlacedSemantic::Inline(NodeSemantic::StoredInteger(value)),
        StoredSemanticNode::AccountedFloat(value) => PlacedSemantic::Inline(NodeSemantic::AccountedFloat(value)),
        StoredSemanticNode::Text(value) => PlacedSemantic::Inline(NodeSemantic::Text(value)),
        StoredSemanticNode::LocalDate(value) => PlacedSemantic::Inline(NodeSemantic::LocalDate(value)),
        StoredSemanticNode::Array { item_role } => PlacedSemantic::Inline(NodeSemantic::Array { item_role }),
        StoredSemanticNode::Object { member_role } => PlacedSemantic::Inline(NodeSemantic::Object { member_role }),
        StoredSemanticNode::ContainerSpan { text, container } => {
            PlacedSemantic::Inline(NodeSemantic::ContainerSpan { text, container })
        }
        StoredSemanticNode::Unrepresentable => PlacedSemantic::Inline(NodeSemantic::Unrepresentable),
        StoredSemanticNode::StoredDecimal { coefficient, scale } => {
            PlacedSemantic::Wide(WidePayload::StoredDecimal { coefficient, scale })
        }
        StoredSemanticNode::AccountedBytes(value) => PlacedSemantic::Wide(WidePayload::AccountedBytes(value)),
        StoredSemanticNode::AccountedLocalTime(value) => PlacedSemantic::Wide(WidePayload::AccountedLocalTime(value)),
        StoredSemanticNode::AccountedLocalDateTime(value) => {
            PlacedSemantic::Wide(WidePayload::AccountedLocalDateTime(value))
        }
        StoredSemanticNode::AccountedOffsetDateTime(value) => {
            PlacedSemantic::Wide(WidePayload::AccountedOffsetDateTime(value))
        }
    }
}

/// Core tag (matches the node's kind) or a non-core wrapper.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntrinsicTagSemantics {
    /// Core tag; matches the node's kind.
    Core,
    /// Non-core tag; materializes as `Value::Tagged`.
    Tagged,
}

/// The tag on one document node.
#[derive(Debug, Eq, PartialEq)]
pub struct IntrinsicTag {
    tag: TagId,
    semantics: IntrinsicTagSemantics,
}

impl IntrinsicTag {
    /// Core tag. The node's kind is the tag's category.
    #[must_use]
    pub fn core(tag: TagId) -> Self {
        Self {
            tag,
            semantics: IntrinsicTagSemantics::Core,
        }
    }

    /// Non-core tag. Materializes as `Value::Tagged`.
    #[must_use]
    pub fn tagged(tag: TagId) -> Self {
        Self {
            tag,
            semantics: IntrinsicTagSemantics::Tagged,
        }
    }

    /// The tag text.
    #[must_use]
    pub const fn tag(&self) -> &TagId {
        &self.tag
    }

    /// Core or non-core.
    #[must_use]
    pub const fn semantics(&self) -> IntrinsicTagSemantics {
        self.semantics
    }
}

/// Packed occurrence key: a source or stored text span; packed by `key_kind_and_payload` and rebuilt by
/// `reconstruct_key`.
pub(crate) type StoredOccurrenceKey = TextRef;

/// Compact optional reference to a node's intrinsic tag in the `tags` side arena, with the `Tagged`-semantics bit
/// packed inline.
///
/// Encoding: `0` means no tag; otherwise the low bit carries the `Tagged`-semantics flag and the remaining bits carry
/// `index + 1`. The flag is read on the object-key derivation hot path without touching the arena.
#[derive(Clone, Copy, Default, Eq, PartialEq)]
pub(crate) struct IntrinsicTagRef(u32);

impl IntrinsicTagRef {
    pub(crate) const NONE: Self = Self(0);

    pub(crate) fn present(index: u32, tagged: bool) -> Result<Self, DataError> {
        let slot = index.checked_add(1).ok_or(DataError::ArithmeticOverflow)?;
        // `checked_mul`, not `checked_shl`: the shift only bounds the shift AMOUNT, so a slot with its top bit set
        // would shift that bit out and alias a different node's tag.
        let shifted = slot.checked_mul(2).ok_or(DataError::ArithmeticOverflow)?;
        Ok(Self(shifted | u32::from(tagged)))
    }

    pub(crate) fn is_present(self) -> bool {
        self.0 != 0
    }

    /// Whether the node carries a `Tagged`-semantics intrinsic tag.
    pub(crate) fn is_tagged(self) -> bool {
        self.0 & 1 != 0
    }

    fn index(self) -> Option<usize> {
        (self.0 != 0).then(|| ((self.0 >> 1) - 1) as usize)
    }
}

/// Fixed compact node header (40 bytes).
///
/// Holds the node's inline semantic niche (or a [`NodeSemantic::Wide`] index into the typed `wide` side arena for
/// large/uncommon scalar payloads), its schema-bound kind, a compact intrinsic-tag reference into the `tags` side
/// arena, and the owned occurrence and projection ranges. Codecs store integers as `StoredInteger` and floats as
/// `AccountedFloat`, which keeps the header at 40 bytes. `semantic` is ordered first so its 8-byte alignment leaves the
/// header padding-free.
pub(crate) struct NodeRecord {
    pub(crate) semantic: NodeSemantic,
    pub(crate) kind: NodeKindBindingId,
    pub(crate) intrinsic_tag: IntrinsicTagRef,
    pub(crate) occurrence_range: StorageRange,
    pub(crate) projection_range: StorageRange,
}

/// Builder-side occurrence later projected into the `edges`/`sidecars`/`keys` arenas and the winner entries.
pub(crate) struct OccurrenceRecord {
    pub(crate) owner: LocalOwnerRef,
    pub(crate) role: OccurrenceRoleBindingId,
    pub(crate) position: u32,
    pub(crate) key: Option<StoredOccurrenceKey>,
    pub(crate) target: NodeId,
}

/// Self-contained object winner entry (16 bytes).
///
/// Carries an object member's value target and packed key inline so the object read hot path resolves both without
/// scattering through the `edges`, `sidecars`, or `keys` arenas. `key`/`key_kind` mirror the packed occurrence key;
/// `key_kind == KEY_KIND_NONE` marks a keyless winner, which well-formed objects never produce.
#[derive(Clone, Copy)]
pub(crate) struct ObjectWinnerEntry {
    pub(crate) target: NodeId,
    pub(crate) key: KeyPayload,
    pub(crate) key_kind: u32,
}

fn object_winner_entry(record: &OccurrenceRecord) -> ObjectWinnerEntry {
    let (key_kind, key) = match &record.key {
        None => (KEY_KIND_NONE, KeyPayload::default()),
        Some(key) => key_kind_and_payload(key),
    };
    ObjectWinnerEntry {
        target: record.target,
        key,
        key_kind,
    }
}

/// A `u32`-packed arena span (`start` plus `len`); `slice` saturates to an empty slice and `checked_slice` refuses
/// overflow.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct StorageRange {
    pub(crate) start: u32,
    pub(crate) len: u32,
}

impl StorageRange {
    pub(crate) fn try_new(start: usize, len: usize) -> Result<Self, DataError> {
        Ok(Self {
            start: u32::try_from(start).map_err(|_| DataError::ArithmeticOverflow)?,
            len: u32::try_from(len).map_err(|_| DataError::ArithmeticOverflow)?,
        })
    }

    pub(crate) fn slice<T>(self, items: &[T]) -> &[T] {
        let start = self.start as usize;
        let end = start.saturating_add(self.len as usize);
        items.get(start..end).unwrap_or(&[])
    }

    fn checked_slice<T>(self, items: &[T]) -> Option<&[T]> {
        let start = self.start as usize;
        let end = start.checked_add(self.len as usize)?;
        items.get(start..end)
    }
}

// --------------------------------------------------------------------------- compact4 relationship storage carriers.
//
// These are the compact4 relationship-storage carriers behind the live relationship-arena derivation and the read paths
// that serve from the `edges`/`sidecars`/`keys` arenas. The unwired candidate layouts (node/array/object records and
// the route flags) are not carried here — only the live arena carriers are.
// ---------------------------------------------------------------------------

/// Compact4 occurrence key kind: no key.
pub(crate) const KEY_KIND_NONE: u32 = 0;
/// Compact4 occurrence key kind: source-span text.
pub(crate) const KEY_KIND_SOURCE_TEXT: u32 = 1;
/// Compact4 occurrence key kind: decoded-arena text.
pub(crate) const KEY_KIND_STORED_TEXT: u32 = 2;

/// Compact4 relationship owner kind: the document root edge.
pub(crate) const OWNER_KIND_ROOT: u32 = 0;
/// Compact4 relationship owner kind: one logical node.
pub(crate) const OWNER_KIND_NODE: u32 = 1;

/// One resolved semantic relationship target in the compact4 edge arena.
///
/// The edge index is the public occurrence id: occurrence `n` lives at edge index `n`.
#[derive(Clone, Copy)]
pub(crate) struct SemanticEdge {
    pub(crate) target: NodeId,
}

/// Packed relationship owner reference and occurrence key kind.
///
/// The low bit selects the owner kind (root or node); the next three bits carry the occurrence key kind. `id` is the
/// owner node id (zero for the root).
#[derive(Clone, Copy, Default, Eq, PartialEq)]
pub(crate) struct CompactOwnerAndKeyKind {
    pub(crate) kind_flags: u32,
    pub(crate) id: u32,
}

impl CompactOwnerAndKeyKind {
    const OWNER_KIND_MASK: u32 = 0x1;
    const KEY_KIND_MASK: u32 = 0xe;
    const KEY_KIND_SHIFT: u32 = 1;

    pub(crate) const fn root(key_kind: u32) -> Self {
        Self {
            kind_flags: key_kind << Self::KEY_KIND_SHIFT,
            id: 0,
        }
    }

    pub(crate) const fn node(node: NodeId, key_kind: u32) -> Self {
        Self {
            kind_flags: OWNER_KIND_NODE | (key_kind << Self::KEY_KIND_SHIFT),
            id: node.as_u32(),
        }
    }

    pub(crate) const fn key_kind(self) -> u32 {
        (self.kind_flags & Self::KEY_KIND_MASK) >> Self::KEY_KIND_SHIFT
    }

    pub(crate) const fn owner_kind(self) -> u32 {
        self.kind_flags & Self::OWNER_KIND_MASK
    }
}

/// Edge-aligned rich occurrence sidecar: owner adjacency, position, and key.
///
/// Physically separate from the edge arena so topology-free coverage can later omit it. Always built in this stage.
#[derive(Clone, Copy)]
pub(crate) struct RichOccurrenceSidecar {
    pub(crate) owner: CompactOwnerAndKeyKind,
    pub(crate) position: u32,
    pub(crate) role: OccurrenceRoleBindingId,
    pub(crate) key_slot: u32,
}

/// Packed occurrence key payload (text span endpoints or key-node id).
///
/// The key kind lives in the sidecar owner word; this holds only the two u32 payload words (span start/len, or key-node
/// id in `payload0`).
#[derive(Clone, Copy, Default)]
pub(crate) struct KeyPayload {
    pub(crate) payload0: u32,
    pub(crate) payload1: u32,
}

/// Resolves an object member's key text, mirroring [`Document::occurrence_key_text`]. The returned text borrows the
/// builder, not the node table, so the arena pass may mutate nodes after resolution.
fn resolve_member_key<'resolver>(
    key: &StoredOccurrenceKey,
    resolver: &'resolver super::builder::AccountedDocumentBuilder<'_>,
) -> Option<&'resolver str> {
    resolver.resolve_text_ref(*key)
}

/// The projected relationship shape of one node, with its topology role copied out so the arena pass can drop its
/// borrow of the node table before mutating it.
enum ProjectionKind {
    Array(OccurrenceRoleBindingId),
    Object(OccurrenceRoleBindingId),
    Scalar,
}

impl ProjectionKind {
    fn of(semantic: &NodeSemantic) -> Self {
        match semantic {
            NodeSemantic::Array { item_role } => Self::Array(*item_role),
            NodeSemantic::Object { member_role } => Self::Object(*member_role),
            _ => Self::Scalar,
        }
    }
}

fn key_kind_and_payload(key: &StoredOccurrenceKey) -> (u32, KeyPayload) {
    match key {
        TextRef::Source(span) => (
            KEY_KIND_SOURCE_TEXT,
            KeyPayload {
                payload0: span.start(),
                payload1: span.end(),
            },
        ),
        TextRef::Stored(span) => (
            KEY_KIND_STORED_TEXT,
            KeyPayload {
                payload0: span.start(),
                payload1: span.end(),
            },
        ),
    }
}

fn reconstruct_key(kind: u32, payload: KeyPayload) -> Result<Option<StoredOccurrenceKey>, DataError> {
    Ok(match kind {
        KEY_KIND_NONE => None,
        KEY_KIND_SOURCE_TEXT => Some(TextRef::Source(
            Span::try_new(payload.payload0, payload.payload1).ok_or(DataError::InvalidDocument)?,
        )),
        KEY_KIND_STORED_TEXT => Some(TextRef::Stored(
            Span::try_new(payload.payload0, payload.payload1).ok_or(DataError::InvalidDocument)?,
        )),
        _ => return Err(DataError::InvalidDocument),
    })
}

fn owner_word(owner: LocalOwnerRef, key_kind: u32) -> Result<CompactOwnerAndKeyKind, DataError> {
    match owner {
        LocalOwnerRef::DocumentRoot => Ok(CompactOwnerAndKeyKind::root(key_kind)),
        LocalOwnerRef::Node(node) => Ok(CompactOwnerAndKeyKind::node(node, key_kind)),
        // Occurrence-owned occurrences are rejected by both builders; only fact owners ever name occurrences.
        LocalOwnerRef::Occurrence(_) => Err(DataError::InvalidDocument),
    }
}

fn reconstruct_owner(word: CompactOwnerAndKeyKind) -> Result<LocalOwnerRef, DataError> {
    match word.owner_kind() {
        OWNER_KIND_ROOT => Ok(LocalOwnerRef::DocumentRoot),
        OWNER_KIND_NODE => Ok(LocalOwnerRef::Node(
            NodeId::try_from_index(word.id as usize).ok_or(DataError::InvalidDocument)?,
        )),
        _ => Err(DataError::InvalidDocument),
    }
}

/// Appends the array item refs owned by a node, in authored order.
///
/// When `store_targets` is set, each ref is the item's target node index (minimal-semantic documents keep no `edges`
/// arena). Otherwise each ref is the occurrence index into `edges`.
fn emit_array_edge_refs(
    occurrences: &[OccurrenceRecord],
    owned: &[OccurrenceId],
    item_role: OccurrenceRoleBindingId,
    edge_refs: &mut Vec<u32>,
    store_targets: bool,
) -> Result<(), DataError> {
    for id in owned {
        let index = id.index();
        let record = occurrences.get(index).ok_or(DataError::InvalidOccurrence)?;
        if record.role == item_role {
            if record.key.is_some() {
                return Err(DataError::InvalidDocument);
            }
            let stored = if store_targets { record.target.index() } else { index };
            edge_refs.push(u32::try_from(stored).map_err(|_| DataError::ArithmeticOverflow)?);
        }
    }
    Ok(())
}

/// Owned-occurrence count above which winner derivation switches from a linear probe over accumulated winners to a
/// reused open-addressing index, and above which an object's projection gets an eytzinger lookup arena (below it, the
/// read side walks the winners linearly and no lookup is built or retained). Small objects stay allocation-free; the
/// threshold matches the object read path's linear/eytzinger boundary.
pub(crate) const SMALL_OBJECT_WINNER_LIMIT: usize = 16;
const _: () = assert!(SMALL_OBJECT_WINNER_LIMIT == crate::value::object::LINEAR_DEDUP_THRESHOLD);

/// One open-addressing index cell mapping a key hash to a winner slot. The cell is occupied only when `stamp` equals
/// the scratch's current generation, so the table is invalidated across objects by bumping the generation, never
/// cleared.
#[derive(Clone, Copy)]
struct WinnerSlot {
    stamp: u32,
    winner: u32,
}

/// Reused scratch computing object winner edges incrementally, without a per-object sort or a per-object hash
/// allocation.
///
/// Members are visited in authored order. The first occurrence of a key fixes its winner position; every later
/// occurrence of the same key overwrites the winner value, so the final occurrence supplies it. Winners therefore
/// emerge directly in first-occurrence order carrying last-occurrence values, matching the sort-by-`(key,
/// order)`/keep-final derivation exactly. Small objects probe `keys` linearly; larger objects consult a reused
/// generation-stamped open-addressing index so wide duplicate-heavy objects stay linear in the member count.
struct ObjectWinnerScratch<'text> {
    keys: Vec<&'text str>,
    winners: Vec<ObjectWinnerEntry>,
    index: Vec<WinnerSlot>,
    mask: usize,
    generation: u32,
    lookup_order: Vec<usize>,
}

/// Hashes key bytes with FNV-1a and a SplitMix64-style finalizer so that keys sharing a common prefix (the wide
/// duplicate fixtures) scatter across the linear-probe index. The winner order and lookup are independent of this hash,
/// so its only effect is probe distribution.
fn hash_key(bytes: &[u8]) -> u64 {
    let mut hash = bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    });
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xff51_afd7_ed55_8ccd);
    hash ^= hash >> 33;
    hash
}

impl<'text> ObjectWinnerScratch<'text> {
    /// An empty scratch; buffers grow on first use.
    fn new() -> Self {
        Self {
            keys: Vec::new(),
            winners: Vec::new(),
            index: Vec::new(),
            lookup_order: Vec::new(),
            mask: 0,
            generation: 1,
        }
    }

    fn compute(
        &mut self,
        occurrences: &'text [OccurrenceRecord],
        owned: &[OccurrenceId],
        member_role: OccurrenceRoleBindingId,
        resolver: &'text super::builder::AccountedDocumentBuilder<'_>,
    ) -> Result<(), DataError> {
        self.keys.clear();
        self.winners.clear();
        let use_index = owned.len() > SMALL_OBJECT_WINNER_LIMIT;
        // Pre-reserve the scratch to the member upper bound ONCE per object: every member can name a distinct winner
        // and the index needs twice the slots, so later pushes hit the reserved capacity and stay cheap.
        self.keys
            .try_reserve_exact(owned.len())
            .map_err(|_| DataError::Allocation)?;
        self.winners
            .try_reserve_exact(owned.len())
            .map_err(|_| DataError::Allocation)?;
        if use_index {
            self.prepare_index(owned.len())?;
        }
        for id in owned {
            let index = id.index();
            let record = occurrences.get(index).ok_or(DataError::InvalidOccurrence)?;
            if record.role != member_role {
                continue;
            }
            let key = record.key.as_ref().ok_or(DataError::UnrepresentableSemantic)?;
            let text = resolve_member_key(key, resolver).ok_or(DataError::UnrepresentableSemantic)?;
            let entry = object_winner_entry(record);
            if use_index {
                self.insert_indexed(text, entry)?;
            } else {
                self.insert_linear(text, entry);
            }
        }
        Ok(())
    }

    /// Sizes the open-addressing index to at least twice the member upper bound (load factor below one half) and
    /// invalidates prior entries. Growth resets every cell to the empty stamp; reuse only advances the generation.
    fn prepare_index(&mut self, member_estimate: usize) -> Result<(), DataError> {
        let needed = member_estimate
            .checked_mul(2)
            .and_then(usize::checked_next_power_of_two)
            .ok_or(DataError::ArithmeticOverflow)?
            .max(16);
        if self.index.len() < needed {
            self.index.clear();
            self.index
                .try_reserve_exact(needed)
                .map_err(|_| DataError::Allocation)?;
            let empty = WinnerSlot { stamp: 0, winner: 0 };
            self.index.resize(needed, empty);
            self.mask = needed - 1;
            self.generation = 1;
        } else if let Some(generation) = self.generation.checked_add(1) {
            self.generation = generation;
        } else {
            self.index.as_mut_slice().iter_mut().for_each(|slot| slot.stamp = 0);
            self.generation = 1;
        }
        Ok(())
    }

    fn push_winner(&mut self, key: &'text str, entry: ObjectWinnerEntry) -> usize {
        let winner = self.winners.len();
        self.keys.push(key);
        self.winners.push(entry);
        winner
    }

    fn insert_linear(&mut self, key: &'text str, entry: ObjectWinnerEntry) {
        if let Some(position) = self.keys.as_slice().iter().position(|existing| *existing == key) {
            self.winners.as_mut_slice()[position] = entry;
        } else {
            self.push_winner(key, entry);
        }
    }

    fn insert_indexed(&mut self, key: &'text str, entry: ObjectWinnerEntry) -> Result<(), DataError> {
        let mut cell =
            usize::try_from(hash_key(key.as_bytes()) & self.mask as u64).map_err(|_| DataError::ArithmeticOverflow)?;
        loop {
            let slot = self.index.as_slice()[cell];
            if slot.stamp != self.generation {
                let winner = self.push_winner(key, entry);
                self.index.as_mut_slice()[cell] = WinnerSlot {
                    stamp: self.generation,
                    winner: u32::try_from(winner).map_err(|_| DataError::ArithmeticOverflow)?,
                };
                return Ok(());
            }
            let winner = slot.winner as usize;
            if self.keys.as_slice()[winner] == key {
                self.winners.as_mut_slice()[winner] = entry;
                return Ok(());
            }
            cell = (cell + 1) & self.mask;
        }
    }

    fn len(&self) -> usize {
        self.winners.len()
    }

    fn winners(&self) -> impl Iterator<Item = ObjectWinnerEntry> + '_ {
        self.winners.as_slice().iter().copied()
    }

    /// Fills `output` with the eytzinger comparison-index ranks over the winners, mirroring the object projection
    /// index: the winners are sorted by key, and each eytzinger slot stores the winner's first-occurrence-order local
    /// position. `output.len()` must equal [`Self::len`].
    fn fill_lookup(&mut self, output: &mut [u32]) -> Result<(), DataError> {
        self.lookup_order.clear();
        self.lookup_order
            .try_reserve_exact(self.winners.len())
            .map_err(|_| DataError::Allocation)?;
        for index in 0..self.winners.len() {
            self.lookup_order.push(index);
        }
        let order = self.lookup_order.as_mut_slice();
        order.sort_unstable_by(|&left, &right| self.keys.as_slice()[left].cmp(self.keys.as_slice()[right]));
        crate::index::try_fill_eytzinger_by(output, |index| order[index]).map_err(|()| DataError::ArithmeticOverflow)
    }
}

/// The six compact4 relationship arenas emitted directly into accounted storage.
pub(crate) struct AccountedRelationshipArenas {
    pub(crate) edges: Vec<SemanticEdge>,
    pub(crate) sidecars: Vec<RichOccurrenceSidecar>,
    pub(crate) keys: Vec<KeyPayload>,
    pub(crate) edge_refs: Vec<u32>,
    pub(crate) winners: Vec<ObjectWinnerEntry>,
    pub(crate) lookup: Vec<u32>,
    /// One per authored occurrence, in both coverage modes: the count the `semantic_relationship_count` law names. In
    /// topology mode it is `edges.len()`; in minimal-semantic mode the `edges` arena is empty (targets live in
    /// `edge_refs`), so the authored total is carried here.
    pub(crate) relationship_total: usize,
}

/// Emits the compact4 arenas directly into owned storage, assigning each node's `projection_range` as it goes. Winner
/// grouping and the parallel eytzinger lookup ranks are produced by the shared [`ObjectWinnerScratch`].
#[allow(
    clippy::too_many_lines,
    reason = "one pass builds all six arenas so the parallel ranges stay obviously consistent"
)]
pub(crate) fn emit_relationship_arenas_accounted(
    nodes: &mut [NodeRecord],
    occurrences: &[OccurrenceRecord],
    owner_occurrences: &[OccurrenceId],
    build_topology: bool,
    resolver: &super::builder::AccountedDocumentBuilder<'_>,
) -> Result<AccountedRelationshipArenas, DataError> {
    let mut edges = Vec::new();
    // Minimal-semantic documents store array targets in `edge_refs` and leave the `edges` arena empty. Topology
    // documents keep occurrence identity here so sidecars can name the same edge.
    if build_topology {
        edges
            .try_reserve_exact(occurrences.len())
            .map_err(jqf_resource::ResourceError::from)?;
    }
    // The sidecar and key arenas exist only for rich occurrence topology; when topology is not demanded they are
    // neither reserved nor populated.
    let sidecar_capacity = if build_topology { occurrences.len() } else { 0 };
    let key_capacity = if build_topology {
        occurrences.iter().filter(|record| record.key.is_some()).count()
    } else {
        0
    };
    let mut sidecars = Vec::new();
    sidecars
        .try_reserve_exact(sidecar_capacity)
        .map_err(jqf_resource::ResourceError::from)?;
    let mut keys = Vec::new();
    keys.try_reserve_exact(key_capacity)
        .map_err(jqf_resource::ResourceError::from)?;
    for record in occurrences {
        if !build_topology {
            continue;
        }
        edges.push(SemanticEdge { target: record.target });
        let (key_kind, key_slot) = match &record.key {
            None => (KEY_KIND_NONE, 0),
            Some(key) => {
                let (kind, payload) = key_kind_and_payload(key);
                let slot = u32::try_from(keys.len()).map_err(|_| DataError::ArithmeticOverflow)?;
                keys.push(payload);
                (kind, slot)
            }
        };
        sidecars.push(RichOccurrenceSidecar {
            owner: owner_word(record.owner, key_kind)?,
            position: record.position,
            role: record.role,
            key_slot,
        });
    }

    // Projection arenas grow as each node writes its range; a census walk of the same node table would only pre-count
    // those ranges.
    let mut edge_refs = Vec::new();
    let mut winners = Vec::new();
    let mut lookup = Vec::new();
    let store_targets = !build_topology;
    let mut winner_scratch = ObjectWinnerScratch::new();
    for node in nodes {
        // Tag-layer nodes own exactly one keyless payload occurrence; see [`Document::tag_payload`] for the tag-layer
        // law.
        if node.intrinsic_tag.is_tagged() && matches!(node.semantic, NodeSemantic::Unrepresentable) {
            let owned = node.occurrence_range.slice(owner_occurrences);
            let start = edge_refs.len();
            edge_refs
                .try_reserve(owned.len())
                .map_err(jqf_resource::ResourceError::from)?;
            for id in owned {
                let record = occurrences.get(id.index()).ok_or(DataError::InvalidOccurrence)?;
                if record.key.is_some() {
                    return Err(DataError::InvalidDocument);
                }
                let stored = if store_targets {
                    record.target.index()
                } else {
                    id.index()
                };
                let edge = u32::try_from(stored).map_err(|_| DataError::ArithmeticOverflow)?;
                edge_refs.push(edge);
            }
            node.projection_range = StorageRange::try_new(start, edge_refs.len() - start)?;
            continue;
        }
        let kind = ProjectionKind::of(&node.semantic);
        let owned = node.occurrence_range.slice(owner_occurrences);
        let projection = match kind {
            ProjectionKind::Array(item_role) => {
                let start = edge_refs.len();
                edge_refs
                    .try_reserve(owned.len())
                    .map_err(jqf_resource::ResourceError::from)?;
                emit_array_edge_refs(occurrences, owned, item_role, &mut edge_refs, store_targets)?;
                StorageRange::try_new(start, edge_refs.len() - start)?
            }
            ProjectionKind::Object(member_role) => {
                let start = winners.len();
                winners
                    .try_reserve(owned.len())
                    .map_err(jqf_resource::ResourceError::from)?;
                lookup
                    .try_reserve(owned.len())
                    .map_err(jqf_resource::ResourceError::from)?;
                winner_scratch.compute(occurrences, owned, member_role, resolver)?;
                for winner in winner_scratch.winners() {
                    winners.push(winner);
                }
                let length = winner_scratch.len();
                lookup.resize(lookup.len() + length, 0);
                // Small objects (at or below the winner threshold) keep a zero-filled, never-read lookup segment so the
                // shared projection range stays aligned, but the eytzinger sort is skipped: the read side walks their
                // winners linearly at this size (the owned-value side makes the same call at `LINEAR_DEDUP_THRESHOLD`),
                // so the per-object sort at finalize is the cost cut here.
                if length > SMALL_OBJECT_WINNER_LIMIT {
                    winner_scratch.fill_lookup(&mut lookup.as_mut_slice()[start..])?;
                }
                StorageRange::try_new(start, length)?
            }
            ProjectionKind::Scalar => StorageRange::default(),
        };
        node.projection_range = projection;
    }

    // Per-node `try_reserve` may overshoot (duplicate object members, role filtering); compact so published storage
    // does not retain that slack.
    edge_refs.shrink_to_fit();
    winners.shrink_to_fit();
    lookup.shrink_to_fit();

    Ok(AccountedRelationshipArenas {
        edges,
        sidecars,
        keys,
        edge_refs,
        winners,
        lookup,
        relationship_total: occurrences.len(),
    })
}

/// Borrowed compact4 array item cursor resolving item edges to target nodes.
#[derive(Clone, Copy)]
pub(crate) struct ArrayItems<'document> {
    refs: &'document [u32],
    edges: &'document [SemanticEdge],
}

impl ArrayItems<'_> {
    pub(crate) fn len(self) -> usize {
        self.refs.len()
    }

    pub(crate) fn is_empty(self) -> bool {
        self.refs.is_empty()
    }

    pub(crate) fn get(self, index: usize) -> Option<NodeId> {
        let stored = *self.refs.get(index)?;
        if self.edges.is_empty() {
            // Minimal-semantic: refs are target node indices. Emptiness is an exact MODE discriminator, not a count
            // test: topology builds push one edge per occurrence document-wide before any ref can name one, so a single
            // edge-indexed ref implies a non-empty arena.
            NodeId::try_from_index(stored as usize)
        } else {
            self.edges.get(stored as usize).map(|edge| edge.target)
        }
    }

    pub(crate) fn first(self) -> Option<NodeId> {
        self.get(0)
    }
}

/// Borrowed compact4 object entry cursor over self-contained winner entries.
///
/// Each entry carries its value target and packed key inline, so iteration and key comparison never touch the `edges`,
/// `sidecars`, or `keys` arenas.
#[derive(Clone, Copy)]
pub(crate) struct ObjectEntries<'document> {
    winners: &'document [ObjectWinnerEntry],
}

impl ObjectEntries<'_> {
    pub(crate) fn len(self) -> usize {
        self.winners.len()
    }

    pub(crate) fn is_empty(self) -> bool {
        self.winners.is_empty()
    }

    pub(crate) fn get(self, index: usize) -> Option<ObjectWinnerEntry> {
        self.winners.get(index).copied()
    }

    pub(crate) fn first(self) -> Option<ObjectWinnerEntry> {
        self.get(0)
    }
}

/// Node-owned facts grouped by their node, built once at finalize so the `.@`/`.&` accessors read only the owning
/// node's facts instead of scanning every fact in the document. Empty (and free) when the document carries no
/// node-owned facts.
#[derive(Default)]
pub(crate) struct FactOwnerIndex {
    /// Fact-owning node indices in ascending order (the binary-search key).
    pub(crate) nodes: Vec<u32>,
    /// Each entry's contiguous range into `fact_ids`.
    pub(crate) ranges: Vec<StorageRange>,
    /// Fact ids grouped by owner, document order preserved within an owner.
    pub(crate) fact_ids: Vec<FactId>,
}

impl FactOwnerIndex {
    /// Returns the owning node's fact ids, or an empty slice when the node owns no facts.
    fn ids_for(&self, node: NodeId) -> &[FactId] {
        // Compare widened to usize: narrowing the node index with a sentinel fallback could binary-search a value a
        // real node legitimately holds, and fail-closed beats false-match.
        let Ok(position) = self
            .nodes
            .binary_search_by(|stored| (*stored as usize).cmp(&node.index()))
        else {
            return &[];
        };
        self.ranges[position].slice(&self.fact_ids)
    }

    /// Groups node-owned facts by owner. Empty when `facts` is empty — no allocations. The cooperative finalizer
    /// builds the same shape in work-credited slices; this is the one-shot form `finish` uses.
    pub(crate) fn build(facts: &[StoredDocumentFact], node_count: usize) -> Result<Self, DataError> {
        if facts.is_empty() {
            return Ok(Self::default());
        }
        // ponytail: size-gated two-path build; drop the sparse path if document shapes ever stop splitting cleanly at
        // this ratio.
        if facts.len() * 8 < node_count {
            Self::build_grouped(facts, node_count)
        } else {
            Self::build_counted(facts, node_count)
        }
    }

    /// Sparse-fact form: one owner index per node-owned fact, sorted into runs. Every scratch table is sized by the
    /// fact count, so a large document with few node-owned facts commits no per-node table.
    fn build_grouped(facts: &[StoredDocumentFact], node_count: usize) -> Result<Self, DataError> {
        // One owner index per node-owned fact, then sorted into runs: every scratch table here is sized by the fact
        // count, so a large document with few node-owned facts commits no per-node table.
        let mut owners = Vec::new();
        owners
            .try_reserve_exact(facts.len())
            .map_err(jqf_resource::ResourceError::from)?;
        for fact in facts {
            if let LocalOwnerRef::Node(node) = fact.owner {
                let index = node.index();
                if index >= node_count {
                    return Err(DataError::InvalidDocument);
                }
                owners.push(u32::try_from(index).map_err(|_| DataError::ArithmeticOverflow)?);
            }
        }
        owners.sort_unstable();
        // Distinct owners cannot exceed the fact count (an owner owns at least one fact) nor the node count, so the
        // smaller bound is the exact reservation.
        let owner_bound = facts.len().min(node_count);
        let mut nodes = Vec::new();
        let mut ranges = Vec::new();
        let mut cursors = Vec::new();
        nodes
            .try_reserve_exact(owner_bound)
            .map_err(jqf_resource::ResourceError::from)?;
        ranges
            .try_reserve_exact(owner_bound)
            .map_err(jqf_resource::ResourceError::from)?;
        cursors
            .try_reserve_exact(owner_bound)
            .map_err(jqf_resource::ResourceError::from)?;
        let mut start = 0usize;
        let mut position = 0usize;
        while position < owners.len() {
            let owner = owners[position];
            let mut run = 1usize;
            while position + run < owners.len() && owners[position + run] == owner {
                run += 1;
            }
            nodes.push(owner);
            ranges.push(StorageRange::try_new(start, run)?);
            cursors.push(start);
            start = start.checked_add(run).ok_or(DataError::ArithmeticOverflow)?;
            position += run;
        }
        let placeholder = FactId::try_from_index(0).ok_or(DataError::ArithmeticOverflow)?;
        let mut fact_ids = Vec::new();
        fact_ids
            .try_reserve_exact(start)
            .map_err(jqf_resource::ResourceError::from)?;
        fact_ids.resize(start, placeholder);
        for (cursor, fact) in facts.iter().enumerate() {
            if let LocalOwnerRef::Node(node) = fact.owner {
                let index = u32::try_from(node.index()).map_err(|_| DataError::ArithmeticOverflow)?;
                // Every node-owned fact's owner entered `owners`, so its distinct-owner slot exists; an absence stays
                // fail-closed.
                let slot = nodes.binary_search(&index).map_err(|_| DataError::InvalidDocument)?;
                let target = cursors[slot];
                fact_ids[target] = FactId::try_from_index(cursor).ok_or(DataError::ArithmeticOverflow)?;
                cursors[slot] = target.checked_add(1).ok_or(DataError::ArithmeticOverflow)?;
            }
        }
        Ok(Self {
            nodes,
            ranges,
            fact_ids,
        })
    }

    /// Dense-fact form: counting sort over a per-node scratch table sized by `node_count`, linear in both passes.
    fn build_counted(facts: &[StoredDocumentFact], node_count: usize) -> Result<Self, DataError> {
        let mut counts = Vec::new();
        counts
            .try_reserve_exact(node_count)
            .map_err(jqf_resource::ResourceError::from)?;
        counts.resize(node_count, 0usize);
        for fact in facts {
            if let LocalOwnerRef::Node(node) = fact.owner {
                let index = node.index();
                let slot = counts.get_mut(index).ok_or(DataError::InvalidDocument)?;
                *slot = slot.checked_add(1).ok_or(DataError::ArithmeticOverflow)?;
            }
        }
        let mut nodes = Vec::new();
        let mut ranges = Vec::new();
        // Distinct owners cannot exceed the fact count (an owner owns at least one fact) nor the node count, so the
        // smaller bound is the exact reservation: a large document with few node-owned facts does not transiently
        // commit a second full per-node table.
        let owner_bound = facts.len().min(node_count);
        nodes
            .try_reserve_exact(owner_bound)
            .map_err(jqf_resource::ResourceError::from)?;
        ranges
            .try_reserve_exact(owner_bound)
            .map_err(jqf_resource::ResourceError::from)?;
        let mut start = 0usize;
        for (index, count) in counts.iter_mut().enumerate() {
            if *count == 0 {
                continue;
            }
            nodes.push(u32::try_from(index).map_err(|_| DataError::ArithmeticOverflow)?);
            ranges.push(StorageRange::try_new(start, *count)?);
            let next = start.checked_add(*count).ok_or(DataError::ArithmeticOverflow)?;
            *count = start;
            start = next;
        }
        let placeholder = FactId::try_from_index(0).ok_or(DataError::ArithmeticOverflow)?;
        let mut fact_ids = Vec::new();
        fact_ids
            .try_reserve_exact(start)
            .map_err(jqf_resource::ResourceError::from)?;
        fact_ids.resize(start, placeholder);
        for (cursor, fact) in facts.iter().enumerate() {
            if let LocalOwnerRef::Node(node) = fact.owner {
                let index = node.index();
                let position = counts[index];
                fact_ids[position] = FactId::try_from_index(cursor).ok_or(DataError::ArithmeticOverflow)?;
                counts[index] = position.checked_add(1).ok_or(DataError::ArithmeticOverflow)?;
            }
        }
        Ok(Self {
            nodes,
            ranges,
            fact_ids,
        })
    }
}

/// The storage facade behind [`Document`]: schema-bound tables, the compact4 arenas, and the source-text state.
pub(crate) struct DocumentStorage<'source> {
    #[allow(
        dead_code,
        reason = "immutable execution proof is exposed only by benchmark-internals"
    )]
    #[cfg(feature = "benchmark-internals")]
    pub(crate) schema_execution: SchemaExecution,
    pub(crate) format: FormatBindingId,
    pub(crate) dialect: Option<DialectBindingId>,
    pub(crate) key: DocumentId,
    pub(crate) root: NodeId,
    pub(crate) coverage: DocumentCoverage,
    pub(crate) text: DocumentTextStorage,
    pub(crate) _source: PhantomData<&'source ()>,
    pub(crate) nodes: Vec<NodeRecord>,
    // Large/uncommon scalar payloads moved out of the fixed node header, addressed by each node's `NodeSemantic::Wide`
    // index.
    pub(crate) wide: Vec<WidePayload>,
    // Intrinsic tags moved out of the fixed node header, addressed by each node's compact `IntrinsicTagRef`. Uncommon,
    // so usually empty.
    pub(crate) tags: Vec<IntrinsicTag>,
    pub(crate) facts: Vec<StoredDocumentFact>,
    /// Node-owned facts grouped by their node, built once at finalize so the `.@`/`.&` accessors read only the owning
    /// node's facts instead of scanning every fact in the document. Empty (and free) when the document carries no
    /// node-owned facts.
    pub(crate) fact_owner_index: FactOwnerIndex,
    /// Whether the finalize-time fact-owner index pass RAN. When true, the index names every node-owned fact, so an
    /// empty slice for a node PROVES it owns none and the accessor must not fall back to a whole arena scan. Both
    /// `finish` and the cooperative finalizer set this.
    pub(crate) fact_owner_indexed: bool,
    // compact4 relationship arenas. Every occurrence and projection read is served from these;
    // `edges`/`sidecars`/`keys` are addressed by occurrence id, and `edge_refs`/`winners`/`lookup` are addressed by
    // each node's `projection_range`.
    pub(crate) edges: Vec<SemanticEdge>,
    pub(crate) sidecars: Vec<RichOccurrenceSidecar>,
    pub(crate) keys: Vec<KeyPayload>,
    pub(crate) edge_refs: Vec<u32>,
    pub(crate) winners: Vec<ObjectWinnerEntry>,
    pub(crate) lookup: Vec<u32>,
    /// One per authored occurrence, in both coverage modes: the count the `semantic_relationship_count` law names. In
    /// topology mode it is `edges.len()`; in minimal-semantic mode the `edges` arena is empty (targets live in
    /// `edge_refs`), so the authored total is carried here.
    pub(crate) relationship_total: usize,
    /// The format-owned reader that turns a [`NodeSemantic::ContainerSpan`]'s validated source text into an owned
    /// value.
    ///
    /// `Document` stays format-neutral: it holds an opaque materializer the codec that produced the spans installed,
    /// and knows only that a span of its own retained source can be turned into a value. A document with no container
    /// spans carries `None` and pays nothing.
    pub(crate) span_materializer: Option<&'static dyn super::LazySpanMaterializer>,
    /// How many [`NodeSemantic::ContainerSpan`] nodes this document holds.
    pub(crate) container_spans: u32,
    /// Last-wins Exact cache recorded while the decoder proved a container span.
    ///
    /// One row per winning span: child / filter / probe / has, object keys, `FanOut` values, and minmax. Last-wins
    /// updates that row during the proving pass; it does not rematerialize the hit. Empty when the span was published
    /// without a cache — count then lexes the span. Whole is not packed into this row.
    pub(crate) span_cache: Vec<SpanCache>,
    /// The authored source spans of scalars whose retained semantic carries no span of its own — the codecs' floats,
    /// decimals, and booleans, which re-resolve their semantic from stored storage but whose authored token the edit
    /// lane must be able to address for verbatim echo and patching.
    ///
    /// Kept OUT of the fixed node header so a node without one pays nothing; the records are authored in node order and
    /// looked up by binary search. Sorted at finalize by [`super::AccountedDocumentBuilder::record_authored_span`]'s
    /// ownership: the recording codec builds nodes strictly in order, so the table is sorted by construction.
    pub(crate) authored_spans: Vec<AuthoredSpanRecord>,
}

/// One authored source span recorded for a scalar whose retained semantic carries no span of its own (a codec float,
/// decimal, or boolean).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AuthoredSpanRecord {
    /// The node whose authored token the span names.
    pub(crate) node: NodeId,
    /// The authored token's span, segment-relative like every source span.
    pub(crate) span: Span,
}

/// Last-wins Exact cache for one container span, recorded during the scan that proved it.
///
/// Optional fields stay `None` when that job was not packed. See [`DocumentStorage::span_cache`] for the last-wins
/// law.
#[derive(Clone, Debug)]
pub(crate) struct SpanCache {
    /// The span node this row describes.
    pub(crate) node: NodeId,
    /// Array element or last-wins object member cardinality. `None` when the decoder did not count children.
    pub(crate) child_count: Option<u64>,
    /// Collect-filter hits over the same span. `None` when the walk declined a member or the demand was not a
    /// collect-filter.
    pub(crate) filter_count: Option<u64>,
    /// Collect-probe tally (`[.users[].name] | length`). `None` when the probe was not packed, or an item declined.
    pub(crate) probe_count: Option<u64>,
    /// Last-wins `has(LITERAL)` presence. `None` when Has was not packed.
    pub(crate) has_present: Option<bool>,
    /// Last-wins object key names for `PATH | keys`. `None` when Keys was not packed.
    pub(crate) keys: Option<alloc::vec::Vec<alloc::string::String>>,
    /// `FanOut` probe/construct values. `None` when that job was not packed.
    pub(crate) values: Option<alloc::vec::Vec<crate::Value>>,
    /// Last-wins `min`/`max` winner. `None` when minmax was not packed.
    pub(crate) minmax: Option<crate::Value>,
}

impl SpanCache {
    pub(crate) fn empty(node: NodeId) -> Self {
        Self {
            node,
            child_count: None,
            filter_count: None,
            probe_count: None,
            has_present: None,
            keys: None,
            values: None,
            minmax: None,
        }
    }

    fn find(records: &[Self], node: NodeId) -> Option<&Self> {
        records.iter().find(|record| record.node == node)
    }
}

impl AuthoredSpanRecord {
    /// Looks up one node's authored span by binary search over the node-ordered table.
    pub(crate) fn find(records: &[AuthoredSpanRecord], node: NodeId) -> Option<Span> {
        records
            .binary_search_by_key(&node.index(), |record| record.node.index())
            .ok()
            .and_then(|index| records.get(index))
            .map(|record| record.span)
    }
}

/// Storage that owns its schema inline.
pub(crate) struct InlineDocumentStorage<'source> {
    schema: DocumentSchema,
    storage: DocumentStorage<'source>,
}

/// Storage that shares a schema allocation across documents.
pub(crate) struct SharedDocumentStorage<'source> {
    schema: Arc<DocumentSchema>,
    storage: DocumentStorage<'source>,
}

/// The owned storage container: inline-schema or shared-schema.
pub(crate) enum DocumentStorageOwner<'source> {
    AccountedInline(Arc<InlineDocumentStorage<'source>>),
    AccountedShared(Arc<SharedDocumentStorage<'source>>),
}

impl<'source> DocumentStorageOwner<'source> {
    /// Wraps inline-schema storage in a shared owner.
    pub(super) fn new_accounted_inline(schema: DocumentSchema, storage: DocumentStorage<'source>) -> Self {
        Self::AccountedInline(Arc::new(InlineDocumentStorage { schema, storage }))
    }

    /// Wraps shared-schema storage in a shared owner.
    pub(super) fn new_accounted_shared(schema: Arc<DocumentSchema>, storage: DocumentStorage<'source>) -> Self {
        Self::AccountedShared(Arc::new(SharedDocumentStorage { schema, storage }))
    }

    pub(crate) fn schema(&self) -> &DocumentSchema {
        match self {
            Self::AccountedInline(storage) => &storage.schema,
            Self::AccountedShared(storage) => &storage.schema,
        }
    }

    #[cfg(any(test, feature = "benchmark-internals"))]
    pub(crate) fn shares_schema_allocation_with(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::AccountedShared(left), Self::AccountedShared(right)) => Arc::ptr_eq(&left.schema, &right.schema),
            _ => false,
        }
    }
}

impl<'source> Deref for DocumentStorageOwner<'source> {
    type Target = DocumentStorage<'source>;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::AccountedInline(storage) => &storage.storage,
            Self::AccountedShared(storage) => &storage.storage,
        }
    }
}

/// One immutable document.
///
/// Storage is immutable once finalized and Arc-shared: [`Clone`] and [`Self::try_clone`] retain the same tables without
/// copying them, so every clone observes identical retained data for the lifetime of the document. `Value` is `Clone`
/// too (refcount bump); this type is not a deep copy of the document graph.
pub struct Document<'source> {
    pub(crate) storage: DocumentStorageOwner<'source>,
    pub(crate) borrowed_source: Option<ValidatedSourceBacking<'source>>,
    pub(crate) trusted_session_source_attachment: bool,
    /// Whether the source bytes already *are* the compact render: no extra whitespace, no exponent numbers, no
    /// non-minimal escapes, no duplicate keys. Set at finalize. `false` means do not echo the source as the encoded
    /// form.
    pub(crate) source_canonical: bool,
}

impl Clone for Document<'_> {
    fn clone(&self) -> Self {
        Self {
            storage: match &self.storage {
                DocumentStorageOwner::AccountedInline(storage) => {
                    DocumentStorageOwner::AccountedInline(Arc::clone(storage))
                }
                DocumentStorageOwner::AccountedShared(storage) => {
                    DocumentStorageOwner::AccountedShared(Arc::clone(storage))
                }
            },
            borrowed_source: self.borrowed_source,
            trusted_session_source_attachment: self.trusted_session_source_attachment,
            source_canonical: self.source_canonical,
        }
    }
}

impl fmt::Debug for Document<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Document")
            .field("format", &self.format())
            .field("dialect", &self.dialect())
            .field("key", &self.key())
            .field("root", &self.root())
            .field("nodes", &self.storage.nodes.len())
            .field("occurrences", &self.storage.edges.len())
            .field("facts", &self.storage.facts.len())
            .finish()
    }
}

impl<'source> Document<'source> {
    /// Returns the immutable retained-data authority of this document.
    #[must_use]
    pub fn coverage(&self) -> DocumentCoverage {
        self.storage.coverage
    }

    pub(crate) fn require_capability(&self, capability: DocumentCapability) -> Result<(), DataError> {
        if self.coverage().contains(capability) {
            Ok(())
        } else {
            Err(DataError::CapabilityUnavailable { capability })
        }
    }

    /// Retains another owner of the same immutable storage.
    ///
    /// Infallible: sharing is an Arc bump. [`Clone`] is the same operation without the `Result`.
    pub fn try_clone(&self) -> Result<Self, DataError> {
        Ok(self.clone())
    }

    /// Returns whether two documents retain the same immutable prepared-schema allocation.
    #[cfg(feature = "benchmark-internals")]
    #[doc(hidden)]
    #[must_use]
    pub fn benchmark_shares_schema_storage_with(&self, other: &Self) -> bool {
        self.storage.shares_schema_allocation_with(&other.storage)
    }

    /// Installs source backing already proved by one codec-core-owned immutable access session without repeating the
    /// digest and per-reference UTF-8 pass.
    ///
    /// # Safety
    ///
    /// `source` must be the exact immutable source authority used to create the document's canonical seal and every
    /// retained source text reference. The caller must own that authority continuously from sealing through parse,
    /// finalization, and this call — the metadata-equality-is-not-continuity law stated on
    /// `DocumentSourceBinding::text_from_bound_authority`.
    #[doc(hidden)]
    pub unsafe fn with_borrowed_source_from_bound_authority<'attached>(
        self,
        source: ResolvedSource<'attached>,
        resources: &jqf_resource::ResourceContext<'_>,
    ) -> Result<Document<'attached>, DataError>
    where
        'source: 'attached,
    {
        if self.borrowed_source.is_some() {
            return Err(DataError::InvalidDocument);
        }
        let seal = self.storage.text.seal.ok_or(DataError::InvalidDocument)?;
        if !seal.metadata_matches(source) {
            return Err(DataError::InvalidDocument);
        }
        resources.check_control()?;
        let storage: DocumentStorageOwner<'attached> = self.storage;
        Ok(Document {
            storage,
            borrowed_source: Some(ValidatedSourceBacking::new(source.bytes())),
            trusted_session_source_attachment: true,
            // The decode-side canonicality verdict survives the source install — the canonical-identity law in the
            // `source_canonical` field doc.
            source_canonical: self.source_canonical,
        })
    }

    /// Returns the canonical format identity.
    ///
    /// # Panics
    ///
    /// Panics only if private published storage violates schema validation.
    #[must_use]
    pub fn format(&self) -> &FormatId {
        self.storage
            .schema()
            .format(self.storage.format)
            .expect("published format binding is valid")
    }

    /// Returns the optional canonical dialect identity.
    ///
    /// # Panics
    ///
    /// Panics only if private published storage violates schema validation.
    #[must_use]
    pub fn dialect(&self) -> Option<&DialectId> {
        self.storage.dialect.map(|id| {
            self.storage
                .schema()
                .dialect(id)
                .expect("published dialect binding is valid")
        })
    }

    /// Returns this document's identity.
    #[must_use]
    pub fn key(&self) -> DocumentId {
        self.storage.key
    }

    /// Returns the root node identity.
    #[must_use]
    pub fn root(&self) -> NodeId {
        self.storage.root
    }

    /// Returns a revision-scoped root handle.
    #[must_use]
    pub fn root_handle(&self) -> NodeHandle {
        NodeHandle::new(self.storage.key, self.storage.root)
    }

    /// Returns a revision-scoped node handle after validating the local id.
    pub fn node_handle(&self, node: NodeId) -> Result<NodeHandle, DataError> {
        self.node_record(node)?;
        Ok(NodeHandle::new(self.key(), node))
    }

    /// Returns the single PAYLOAD node of a tag-LAYER node (a kindless `Unrepresentable` semantic carrying a non-core
    /// intrinsic tag), or `None` for any other node.
    ///
    /// This is the payload-transparent descent a navigator needs to see through a tag chain: a tag-layer owns exactly
    /// one keyless payload occurrence (routed through the array `edge_refs` arena), which is this method's answer.
    /// `Some` is returned only for a true layer; a representable tagged node (its payload is the node's own value)
    /// returns `None`.
    pub fn tag_payload(&self, node: NodeId) -> Result<Option<NodeId>, DataError> {
        let record = self.node_record(node)?;
        if !matches!(record.semantic, NodeSemantic::Unrepresentable) || !record.intrinsic_tag.is_tagged() {
            return Ok(None);
        }
        let items = self.array_projection_checked(node)?;
        match (items.len(), items.first()) {
            (1, Some(payload)) => Ok(Some(payload)),
            _ => Err(DataError::InvalidDocument),
        }
    }

    /// Validates and returns a node id from a scoped handle.
    pub fn resolve_node_handle(&self, handle: NodeHandle) -> Result<NodeId, DataError> {
        if handle.document() != self.key() {
            return Err(DataError::StaleOrForeignHandle);
        }
        self.node_record(handle.local())?;
        Ok(handle.local())
    }

    /// Returns the number of logical nodes.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.storage.nodes.len()
    }

    /// Returns the number of ordered topology occurrences.
    pub fn occurrence_count(&self) -> Result<usize, DataError> {
        self.require_capability(DocumentCapability::Topology)?;
        Ok(self.semantic_relationship_count())
    }

    /// Returns the number of retained semantic relationship edges.
    ///
    /// Edges are mandatory semantics: one is retained per authored occurrence regardless of demanded coverage, so this
    /// count is available even when rich topology was not retained and equals [`Self::occurrence_count`] when it was.
    /// In minimal-semantic coverage the `edges` arena is empty (array targets live in `edge_refs`), so the authored
    /// total the emit pass counted is carried beside the arenas.
    #[must_use]
    pub fn semantic_relationship_count(&self) -> usize {
        self.storage.relationship_total
    }

    /// Returns the number of attached portable facts.
    pub fn fact_count(&self) -> Result<usize, DataError> {
        self.require_capability(DocumentCapability::AttachedFacts)?;
        Ok(self.storage.facts.len())
    }

    pub(crate) fn text(&self, text: TextRef) -> Option<&str> {
        self.storage.text.resolve(text, self.borrowed_source)
    }

    /// Resolves a text reference to its RAW bytes (no UTF-8 validation). The binary-span materialization path (CBOR)
    /// reads deferred container spans whose bytes are arbitrary, and a `TextRef::Source` span is already validated by
    /// the owning codec; the stored arm resolves arena bytes.
    pub(crate) fn bytes(&self, text: TextRef) -> Option<&[u8]> {
        match text {
            TextRef::Stored(span) => self
                .storage
                .text
                .decoded
                .bytes
                .as_bytes()
                .get(span.start() as usize..span.end() as usize),
            TextRef::Source(span) => {
                let seal = self.storage.text.seal?;
                let bytes = self.borrowed_source.map(ValidatedSourceBacking::bytes)?;
                bytes.get(seal.local_range(span)?)
            }
        }
    }

    pub(crate) fn span_materializer(&self) -> Option<&'static dyn super::LazySpanMaterializer> {
        self.storage.span_materializer
    }

    /// How many container subtrees this document left unbuilt as source spans.
    ///
    /// Zero for every eagerly decoded document, which is every document the default route publishes.
    #[must_use]
    pub fn container_span_count(&self) -> u32 {
        self.storage.container_spans
    }

    /// Array element cardinality recorded while the decoder proved `node`'s container span.
    ///
    /// Exact JSON locate counts children of the last-wins hit during the validating walk. `None` when the span was
    /// published without that count — count then lexes the span. A non-span node answers `None`.
    pub fn container_span_child_count(&self, node: NodeHandle) -> Result<Option<u64>, DataError> {
        let id = self.resolve_node_handle(node)?;
        Ok(self.cached_span_child_count(id))
    }

    /// Collect-filter hits recorded while the decoder proved `node`'s container span, for the packed filter demand.
    ///
    /// `None` when the walk could not answer the filter on that pass, or the decode was not a collect-filter.
    pub fn container_span_filter_count(&self, node: NodeHandle) -> Result<Option<u64>, DataError> {
        let id = self.resolve_node_handle(node)?;
        Ok(self.cached_span_filter_count(id))
    }

    fn span_cache(&self, node: NodeId) -> Option<&SpanCache> {
        SpanCache::find(&self.storage.span_cache, node)
    }

    pub(crate) fn cached_span_child_count(&self, node: NodeId) -> Option<u64> {
        self.span_cache(node).and_then(|record| record.child_count)
    }

    pub(crate) fn cached_span_filter_count(&self, node: NodeId) -> Option<u64> {
        self.span_cache(node).and_then(|record| record.filter_count)
    }

    pub(crate) fn cached_span_probe_count(&self, node: NodeId) -> Option<u64> {
        self.span_cache(node).and_then(|record| record.probe_count)
    }

    /// Collect-probe tally recorded while the decoder proved `node`'s container span.
    pub fn container_span_probe_count(&self, node: NodeHandle) -> Result<Option<u64>, DataError> {
        let id = self.resolve_node_handle(node)?;
        Ok(self.cached_span_probe_count(id))
    }

    pub(crate) fn cached_span_has_present(&self, node: NodeId) -> Option<bool> {
        self.span_cache(node).and_then(|record| record.has_present)
    }

    /// Last-wins `has` presence recorded while the decoder proved `node`'s container span.
    pub fn container_span_has_present(&self, node: NodeHandle) -> Result<Option<bool>, DataError> {
        let id = self.resolve_node_handle(node)?;
        Ok(self.cached_span_has_present(id))
    }

    pub(crate) fn cached_span_keys(&self, node: NodeId) -> Option<&[alloc::string::String]> {
        self.span_cache(node).and_then(|record| record.keys.as_deref())
    }

    /// Last-wins object key names recorded while the decoder proved `node`'s container span.
    pub fn container_span_keys(&self, node: NodeHandle) -> Result<Option<&[alloc::string::String]>, DataError> {
        let id = self.resolve_node_handle(node)?;
        Ok(self.cached_span_keys(id))
    }

    pub(crate) fn cached_span_values(&self, node: NodeId) -> Option<&[crate::Value]> {
        self.span_cache(node).and_then(|record| record.values.as_deref())
    }

    /// `FanOut` probe/construct values recorded while the decoder proved `node`'s container span.
    pub fn container_span_values(&self, node: NodeHandle) -> Result<Option<&[crate::Value]>, DataError> {
        let id = self.resolve_node_handle(node)?;
        Ok(self.cached_span_values(id))
    }

    pub(crate) fn cached_span_minmax(&self, node: NodeId) -> Option<&crate::Value> {
        self.span_cache(node).and_then(|record| record.minmax.as_ref())
    }

    /// Last-wins `min`/`max` winner recorded while the decoder proved `node`'s container span.
    pub fn container_span_minmax(&self, node: NodeHandle) -> Result<Option<&crate::Value>, DataError> {
        let id = self.resolve_node_handle(node)?;
        Ok(self.cached_span_minmax(id))
    }

    /// Whether the decoded SOURCE is canonical — its compact render is the source itself (see the field's
    /// documentation for the disqualifiers).
    #[must_use]
    pub fn source_canonical(&self) -> bool {
        self.source_canonical
    }

    /// Records the decoding session's canonicality verdict at finalize. The setter exists for the decoding session
    /// only; any other caller setting it is a soundness bug in the making.
    pub fn set_source_canonical(&mut self, canonical: bool) {
        self.source_canonical = canonical;
    }

    pub(crate) fn semantic_text<'document>(
        &'document self,
        semantic: &'document NodeSemantic,
    ) -> Option<&'document str> {
        match semantic {
            NodeSemantic::Text(text) => self.text(*text),
            _ => None,
        }
    }

    /// Resolves a node's large/uncommon scalar payload from the typed `wide` side arena.
    pub(crate) fn wide_payload(&self, id: WidePayloadId) -> Result<&WidePayload, DataError> {
        self.storage.wide.get(id.0 as usize).ok_or(DataError::InvalidDocument)
    }

    /// Resolves a node's intrinsic tag from the `tags` side arena, if present.
    pub(crate) fn resolve_intrinsic_tag(&self, tag: IntrinsicTagRef) -> Option<&IntrinsicTag> {
        self.storage.tags.get(tag.index()?)
    }

    pub(crate) fn node_record(&self, node: NodeId) -> Result<&NodeRecord, DataError> {
        let index = node.index();
        self.storage.nodes.get(index).ok_or(DataError::InvalidNode)
    }

    pub(crate) fn occurrence_record(&self, occurrence: OccurrenceId) -> Result<OccurrenceRecord, DataError> {
        let index = occurrence.index();
        let edge = self.storage.edges.get(index).ok_or(DataError::InvalidOccurrence)?;
        let sidecar = self.storage.sidecars.get(index).ok_or(DataError::InvalidOccurrence)?;
        let owner = reconstruct_owner(sidecar.owner)?;
        let key_kind = sidecar.owner.key_kind();
        let key = if key_kind == KEY_KIND_NONE {
            None
        } else {
            let payload = self
                .storage
                .keys
                .get(sidecar.key_slot as usize)
                .copied()
                .ok_or(DataError::InvalidDocument)?;
            reconstruct_key(key_kind, payload)?
        };
        Ok(OccurrenceRecord {
            owner,
            role: sidecar.role,
            position: sidecar.position,
            key,
            target: edge.target,
        })
    }

    /// Resolves one attached fact by its document-local identity.
    pub fn fact(&self, fact: FactId) -> Result<DocumentFact<'_>, DataError> {
        let index = fact.index();
        self.storage
            .facts
            .get(index)
            .map(|stored| {
                let role = self.storage.schema().validated_fact_role(stored.role_binding());
                let kind = self.storage.schema().validated_fact_kind(stored.kind_binding());
                DocumentFact { stored, role, kind }
            })
            .ok_or(DataError::InvalidFact)
    }

    /// The owning node's attached-fact ids via the finalize-time owner index, or an empty slice when the node owns none
    /// (or the document has no node-owned facts at all). The `.@`/`.&` accessors read these instead of scanning every
    /// fact.
    #[must_use]
    pub fn owner_fact_ids(&self, node: NodeId) -> &[FactId] {
        self.storage.fact_owner_index.ids_for(node)
    }

    /// Whether the finalize-time fact-owner index pass ran for this document. When true, `owner_fact_ids` names EVERY
    /// node-owned fact, so an empty slice proves the node owns none. Both `finish` and the cooperative finalizer set
    /// this.
    #[must_use]
    pub fn fact_owner_indexed(&self) -> bool {
        self.storage.fact_owner_indexed
    }

    /// Compact interned fact-role id, if this document interned `role`.
    #[must_use]
    pub fn fact_role_binding(&self, role: &str) -> Option<FactRoleBindingId> {
        self.storage.schema().fact_role_binding(role)
    }

    /// Compact interned fact-kind id, if this document interned `kind`.
    #[must_use]
    pub fn fact_kind_binding(&self, kind: &str) -> Option<FactKindBindingId> {
        self.storage.schema().fact_kind_binding(kind)
    }

    /// Every interned fact role as `(id, namespaced string)`.
    pub fn interned_fact_roles(&self) -> impl Iterator<Item = (FactRoleBindingId, &str)> + '_ {
        self.storage.schema().interned_fact_roles()
    }

    /// Borrowed payload of the first owner fact whose compact role is in `roles` and whose kind matches `kind` when
    /// given.
    pub fn owner_fact_payload_in(
        &self,
        node: NodeId,
        roles: &[FactRoleBindingId],
        kind: Option<FactKindBindingId>,
    ) -> Result<Option<FactPayloadView<'_>>, DataError> {
        for fact_id in self.owner_fact_ids(node) {
            // An id out of range is document corruption mid-scan, not "not found": fail closed like [`Self::fact`].
            let stored = self.storage.facts.get(fact_id.index()).ok_or(DataError::InvalidFact)?;
            if !roles.iter().any(|role| stored.role_binding() == *role) {
                continue;
            }
            if let Some(kind) = kind
                && stored.kind_binding() != kind
            {
                continue;
            }
            return Ok(Some(stored.payload_view()));
        }
        Ok(None)
    }

    pub(crate) fn object_projection(&self, node: NodeId) -> ObjectEntries<'_> {
        let winners = self
            .storage
            .nodes
            .get(node.index())
            .map_or(&[][..], |record| record.projection_range.slice(&self.storage.winners));
        ObjectEntries { winners }
    }

    pub(crate) fn object_projection_lookup(&self, node: NodeId) -> Result<(ObjectEntries<'_>, &[u32]), DataError> {
        self.object_projection_lookup_from(self.node_record(node)?)
    }

    /// Record-taking form of [`Self::object_projection_lookup`]: callers that already hold the node's record
    /// (materialize's `advance_sync`, the encode cursor) skip the second lookup.
    pub(crate) fn object_projection_lookup_from(
        &self,
        record: &NodeRecord,
    ) -> Result<(ObjectEntries<'_>, &[u32]), DataError> {
        let range = record.projection_range;
        let winners = range
            .checked_slice(&self.storage.winners)
            .ok_or(DataError::InvalidDocument)?;
        let index = range
            .checked_slice(&self.storage.lookup)
            .ok_or(DataError::InvalidDocument)?;
        // Every object carries a lookup segment of exactly its winner count — a small one's is zero-filled and never
        // read, not absent — so the two slices must agree in length whatever the object's size. Emptiness is NOT the
        // small-object test: an all-zero eytzinger index read as a real one resolves every probe to the first winner.
        if winners.len() != index.len() {
            return Err(DataError::InvalidDocument);
        }
        Ok((ObjectEntries { winners }, index))
    }

    pub(crate) fn array_projection(&self, node: NodeId) -> ArrayItems<'_> {
        let refs = self
            .storage
            .nodes
            .get(node.index())
            .map_or(&[][..], |record| record.projection_range.slice(&self.storage.edge_refs));
        ArrayItems {
            refs,
            edges: &self.storage.edges,
        }
    }

    pub(crate) fn array_projection_checked(&self, node: NodeId) -> Result<ArrayItems<'_>, DataError> {
        self.array_projection_checked_from(self.node_record(node)?)
    }

    /// Record-taking form of [`Self::array_projection_checked`]: callers that already hold the node's record
    /// (materialize's `advance_sync`, the encode cursor) skip the second lookup.
    pub(crate) fn array_projection_checked_from(&self, record: &NodeRecord) -> Result<ArrayItems<'_>, DataError> {
        let range = record.projection_range;
        let refs = range
            .checked_slice(&self.storage.edge_refs)
            .ok_or(DataError::InvalidDocument)?;
        Ok(ArrayItems {
            refs,
            edges: &self.storage.edges,
        })
    }

    /// Resolves an object winner's key text from the self-contained winner entry, so the object read hot path never
    /// touches the sidecar or key arenas.
    pub(crate) fn object_projection_key(&self, entry: &ObjectWinnerEntry) -> Option<&str> {
        let key = reconstruct_key(entry.key_kind, entry.key).ok()??;
        self.occurrence_key_text(&key)
    }

    pub(crate) fn occurrence_key_text(&self, key: &StoredOccurrenceKey) -> Option<&str> {
        self.text(*key)
    }

    /// Returns physical text-storage counts for diagnostics and route receipts.
    ///
    /// Requires only retained semantic nodes: occurrence key counts come from the sidecar arena when rich topology is
    /// retained, and otherwise from the self-contained object winner entries, which carry the retained keys of a
    /// minimal-coverage document.
    pub fn text_storage_stats(&self) -> Result<DocumentTextStorageStats, DataError> {
        self.require_capability(DocumentCapability::SemanticNodes)?;
        let mut stats = DocumentTextStorageStats {
            trusted_session_source_attachment: self.trusted_session_source_attachment,
            decoded_arena_len: self.storage.text.decoded.bytes.len(),
            decoded_arena_capacity: self.storage.text.decoded.bytes.capacity(),
            ..DocumentTextStorageStats::default()
        };
        for record in &self.storage.nodes {
            match &record.semantic {
                NodeSemantic::Text(TextRef::Source(_)) => stats.source_string_values += 1,
                NodeSemantic::Text(TextRef::Stored(_)) => stats.stored_string_values += 1,
                NodeSemantic::StoredInteger(text) => {
                    stats.stored_integer_refs += 1;
                    if matches!(text, TextRef::Source(_)) {
                        stats.source_integer_values += 1;
                    }
                }
                _ => {}
            }
        }
        for payload in &self.storage.wide {
            if let WidePayload::StoredDecimal { .. } = payload {
                stats.stored_decimal_coefficient_refs += 1;
            }
        }
        if self.storage.coverage.contains(DocumentCapability::Topology) {
            for sidecar in &self.storage.sidecars {
                match sidecar.owner.key_kind() {
                    KEY_KIND_SOURCE_TEXT => stats.source_keys += 1,
                    KEY_KIND_STORED_TEXT => stats.stored_keys += 1,
                    _ => {}
                }
            }
        } else {
            for entry in &self.storage.winners {
                match entry.key_kind {
                    KEY_KIND_SOURCE_TEXT => stats.source_keys += 1,
                    KEY_KIND_STORED_TEXT => stats.stored_keys += 1,
                    _ => {}
                }
            }
        }
        Ok(stats)
    }

    /// Returns benchmark-only current private table capacities and source-span extent observations without exposing
    /// private records.
    #[cfg(feature = "benchmark-internals")]
    #[doc(hidden)]
    #[allow(
        clippy::too_many_lines,
        reason = "the flat diagnostic receipt deliberately mirrors every private storage table"
    )]
    pub fn benchmark_storage_layout_stats(&self) -> DocumentStorageLayoutStats {
        let mut source_spans = Vec::new();
        for record in &self.storage.nodes {
            // The extent receipt must see every node semantic that names source bytes, not only the string one: a
            // verbatim integer retains its span exactly as an unescaped string does, and a span this receipt cannot see
            // is a byte the document holds without accounting for.
            if let NodeSemantic::Text(TextRef::Source(span)) | NodeSemantic::StoredInteger(TextRef::Source(span)) =
                record.semantic
                && let Some(source) = self.storage.text.seal.map(SourceSnapshotSeal::source)
            {
                source_spans.push((source, span));
            }
        }
        for sidecar in &self.storage.sidecars {
            if sidecar.owner.key_kind() == KEY_KIND_SOURCE_TEXT
                && let Some(payload) = self.storage.keys.get(sidecar.key_slot as usize)
                && let Some(source) = self.storage.text.seal.map(SourceSnapshotSeal::source)
            {
                source_spans.push((source, Span::new(payload.payload0, payload.payload1)));
            }
        }
        let source_span_sum_bytes = source_spans.iter().fold(0usize, |total, (_, span)| {
            total.saturating_add((span.end() - span.start()) as usize)
        });
        source_spans.sort_unstable_by_key(|(source, span)| (*source, span.start(), span.end()));
        let mut source_span_union_bytes = 0usize;
        let mut current: Option<(SourceRef, u32, u32)> = None;
        for (source, span) in &source_spans {
            match current {
                Some((current_source, start, end)) if current_source == *source && span.start() <= end => {
                    current = Some((current_source, start, end.max(span.end())));
                }
                Some((_, start, end)) => {
                    source_span_union_bytes = source_span_union_bytes.saturating_add((end - start) as usize);
                    current = Some((*source, span.start(), span.end()));
                }
                None => current = Some((*source, span.start(), span.end())),
            }
        }
        if let Some((_, start, end)) = current {
            source_span_union_bytes = source_span_union_bytes.saturating_add((end - start) as usize);
        }

        let decoded_text_arena_capacity_bytes = self.storage.text.decoded.bytes.capacity();
        let node_capacity = self.storage.nodes.capacity();
        let occurrence_capacity = self.storage.edges.capacity();
        let array_projection_capacity = self.storage.edge_refs.capacity();
        let object_projection_capacity = self.storage.winners.capacity();
        let object_projection_index_capacity = self.storage.lookup.capacity();
        let fact_capacity = self.storage.facts.capacity();
        let node_table_capacity_bytes = node_capacity.saturating_mul(core::mem::size_of::<NodeRecord>());
        let occurrence_table_capacity_bytes =
            occurrence_capacity.saturating_mul(core::mem::size_of::<OccurrenceRecord>());
        let array_projection_capacity_bytes = array_projection_capacity.saturating_mul(core::mem::size_of::<NodeId>());
        let object_projection_capacity_bytes =
            object_projection_capacity.saturating_mul(core::mem::size_of::<ObjectWinnerEntry>());
        let object_projection_index_capacity_bytes =
            object_projection_index_capacity.saturating_mul(core::mem::size_of::<u32>());
        let fact_table_capacity_bytes = fact_capacity.saturating_mul(core::mem::size_of::<StoredDocumentFact>());
        // The wide side arena is deliberately not folded into this aggregate: the aggregate mirrors the established
        // table set, and the authoritative retained-byte total is the request ledger, which already charges the wide
        // side arena.
        let shallow_table_capacity_bytes = [
            decoded_text_arena_capacity_bytes,
            node_table_capacity_bytes,
            occurrence_table_capacity_bytes,
            array_projection_capacity_bytes,
            object_projection_capacity_bytes,
            object_projection_index_capacity_bytes,
            fact_table_capacity_bytes,
        ]
        .into_iter()
        .fold(0usize, usize::saturating_add);

        let (
            canonical_identity_count,
            node_kind_binding_count,
            occurrence_role_binding_count,
            fact_kind_binding_count,
            fact_role_binding_count,
        ) = self.storage.schema().counts();
        DocumentStorageLayoutStats {
            prepared_schema_only: self.storage.schema_execution.prepared_only(),
            prepared_schema_recipe_fingerprint: self.storage.schema_execution.recipe_fingerprint(),
            prepared_builder_accounted: self.storage.schema_execution.accounted_frontend(),
            prepared_append_count: self.storage.schema_execution.prepared_appends(),
            dynamic_append_count: self.storage.schema_execution.dynamic_appends(),
            dynamic_existing_schema_fast_append_count: self
                .storage
                .schema_execution
                .dynamic_existing_schema_fast_appends(),
            dynamic_schema_transaction_append_count: self.storage.schema_execution.dynamic_schema_transaction_appends(),
            canonical_identity_count,
            canonical_identity_utf8_bytes: self.storage.schema().identity_utf8_bytes(),
            node_kind_binding_count,
            occurrence_role_binding_count,
            fact_kind_binding_count,
            fact_role_binding_count,
            identity_table_shallow_bytes: self.storage.schema().shallow_table_bytes(),
            node_record_bytes: core::mem::size_of::<NodeRecord>(),
            occurrence_record_bytes: core::mem::size_of::<OccurrenceRecord>(),
            stored_fact_record_bytes: core::mem::size_of::<StoredDocumentFact>(),
            source_reference_count: source_spans.len(),
            source_span_sum_bytes,
            source_span_union_bytes,
            source_identity_present: self.storage.text.seal.is_some(),
            physical_source_backing: self.source_segment().is_some(),
            text_ref_size: core::mem::size_of::<TextRef>(),
            stored_occurrence_key_size: core::mem::size_of::<StoredOccurrenceKey>(),
            node_len: self.storage.nodes.len(),
            node_capacity,
            occurrence_len: self.storage.edges.len(),
            occurrence_capacity,
            array_projection_len: self.storage.edge_refs.len(),
            array_projection_capacity,
            object_projection_len: self.storage.winners.len(),
            object_projection_capacity,
            object_projection_index_len: self.storage.lookup.len(),
            object_projection_index_capacity,
            fact_len: self.storage.facts.len(),
            fact_capacity,
            decoded_text_arena_capacity_bytes,
            node_table_capacity_bytes,
            occurrence_table_capacity_bytes,
            array_projection_capacity_bytes,
            object_projection_capacity_bytes,
            object_projection_index_capacity_bytes,
            fact_table_capacity_bytes,
            shallow_table_capacity_bytes,
        }
    }

    pub(crate) fn facts(&self) -> &[StoredDocumentFact] {
        &self.storage.facts
    }

    /// Materializes the canonical root semantic value iteratively.
    ///
    /// Allocates a fresh [`crate::MaterializeWorkspace`]. To reuse cycle-detection scratch across documents, call
    /// [`Self::materialize_root_with`].
    pub fn materialize_root(&self, resources: &mut ResourceContext<'_>) -> Result<Value, DataError> {
        self.require_capability(DocumentCapability::SemanticNodes)?;
        self.require_capability(DocumentCapability::IntrinsicTags)?;
        crate::materialize::materialize_document_node(self, self.root(), resources)
    }

    /// Materializes the canonical root into an owned value, reusing `workspace` as document-independent cycle-detection
    /// scratch.
    ///
    /// Equivalent to [`Self::materialize_root`] but amortizes the O(node-count) cycle-detection bitmap across calls,
    /// the same reuse [`Self::materialize_node_with`] provides for a named node.
    pub fn materialize_root_with(
        &self,
        workspace: &mut crate::MaterializeWorkspace,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Value, DataError> {
        self.require_capability(DocumentCapability::SemanticNodes)?;
        self.require_capability(DocumentCapability::IntrinsicTags)?;
        crate::materialize::materialize_node_with_workspace(self, workspace, self.root(), resources)
    }

    /// Materializes one revision-scoped node semantic value iteratively.
    pub fn materialize_node(
        &self,
        handle: NodeHandle,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Value, DataError> {
        self.require_capability(DocumentCapability::SemanticNodes)?;
        self.require_capability(DocumentCapability::IntrinsicTags)?;
        let node = self.resolve_node_handle(handle)?;
        crate::materialize::materialize_document_node(self, node, resources)
    }

    /// Materializes one revision-scoped node into an owned value, reusing `workspace` as document-independent
    /// cycle-detection scratch.
    ///
    /// Equivalent to [`Document::materialize_node`] but amortizes the O(node-count) cycle-detection bitmap across
    /// calls: the workspace is grown to cover this document's nodes once and left all-clear on return, so a caller
    /// materializing many nodes pays the document-sized setup a single time instead of per value. The workspace holds
    /// no document borrow, so an owner may keep it across captures that also borrow this document.
    pub fn materialize_node_with(
        &self,
        workspace: &mut crate::MaterializeWorkspace,
        handle: NodeHandle,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Value, DataError> {
        self.require_capability(DocumentCapability::SemanticNodes)?;
        self.require_capability(DocumentCapability::IntrinsicTags)?;
        let node = self.resolve_node_handle(handle)?;
        crate::materialize::materialize_node_with_workspace(self, workspace, node, resources)
    }

    /// Returns a borrowed semantic view for one revision-scoped node.
    pub fn value_view(&self, handle: NodeHandle) -> Result<crate::ValueView<'_, 'source>, DataError> {
        self.require_capability(DocumentCapability::SemanticNodes)?;
        let node = self.resolve_node_handle(handle)?;
        Ok(crate::ValueView::new(self, node))
    }

    /// Returns the borrowed semantic view of one node's PAYLOAD, descending every tag-LAYER node to the single value it
    /// wraps.
    ///
    /// A format that resolves one tag per value (YAML) records the tag as an intrinsic fact ON the node, so the node
    /// already IS its own payload and this is the node's own view. A format whose tags nest and can tag a container
    /// (CBOR's uninterpreted tags) cannot say that in one node and builds the kindless tag LAYER instead — an
    /// `Unrepresentable` node whose only occurrence is the payload — which has to be descended before anything asks
    /// the node what it is. Payload-transparent reads (kind, scalar projection, container projection) take this view; a
    /// read that must see the TAG itself keeps [`value_view`](Self::value_view) and asks
    /// [`tag_semantics`](crate::ValueView::tag_semantics) on the outer node.
    pub fn payload_view(&self, handle: NodeHandle) -> Result<crate::ValueView<'_, 'source>, DataError> {
        self.require_capability(DocumentCapability::SemanticNodes)?;
        let mut node = self.resolve_node_handle(handle)?;
        // The builder contract admits shared edges, so a tag-layer chain may in principle cycle; a revisit raises
        // instead of hanging. The set stays empty (no allocation) for every acyclic prefix, which is every document the
        // codecs build.
        let mut seen: Option<Vec<NodeId>> = None;
        while let Some(payload) = self.tag_payload(node)? {
            let seen = seen.get_or_insert_with(Vec::new);
            if seen.contains(&payload) {
                return Err(DataError::CyclicSemanticGraph);
            }
            seen.push(payload);
            node = payload;
        }
        self.value_view(self.node_handle(node)?)
    }

    /// Returns the retained source span of one node's authored value text.
    ///
    /// The strict-JSON decoder keeps source text references for scalar values (numbers and strings) and for deferred
    /// container spans, so their exact byte ranges in the sealed source remain addressable after decode. Built
    /// containers, booleans, nulls, and values the codec materialized into owned storage answer `None`: they have no
    /// single source range of their own. This is the source-preserving publication seam the round-trip editing lanes
    /// read: a leaf whose span is known can be re-published verbatim when unchanged, or replaced by an exact patch when
    /// edited.
    pub fn node_source_span(&self, node: NodeId) -> Result<Option<Span>, DataError> {
        let record = self.node_record(node)?;
        let semantic = match &record.semantic {
            NodeSemantic::Text(TextRef::Source(span))
            | NodeSemantic::StoredInteger(TextRef::Source(span))
            | NodeSemantic::ContainerSpan {
                text: TextRef::Source(span),
                ..
            } => Some(*span),
            NodeSemantic::Wide { id, .. } => match self.wide_payload(*id)? {
                WidePayload::StoredDecimal {
                    coefficient: TextRef::Source(span),
                    ..
                } => Some(*span),
                _ => None,
            },
            _ => None,
        };
        if let Some(span) = semantic {
            return Ok(Some(span));
        }
        // A scalar whose retained semantic carries no span may still have an AUTHORED span the codec recorded
        // out-of-band (a float, decimal, or boolean token). The semantic is stored and re-resolves exactly as without
        // the span; the span only names the authored bytes the edit lane echoes verbatim or patches.
        Ok(AuthoredSpanRecord::find(&self.storage.authored_spans, node))
    }

    /// Returns the complete retained source segment this document's spans address, when the document is source-backed.
    ///
    /// An adjacent-value document is attached to the segment holding exactly its own text, so its spans are
    /// segment-relative: patching the segment's bytes with the segment-relative spans is the same operation as patching
    /// the whole buffer with absolute spans.
    #[must_use]
    pub fn source_segment(&self) -> Option<&[u8]> {
        self.borrowed_source.map(ValidatedSourceBacking::bytes)
    }
}

/// Why a document build, read, handle, or materialize failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DataError {
    /// The immutable product did not retain the exact optional reader capability.
    CapabilityUnavailable {
        /// The capability the caller attempted to open.
        capability: DocumentCapability,
    },
    /// Published authoritative-empty coverage contradicts retained evidence.
    ContradictoryCoverage {
        /// The family simultaneously declared absent and retained.
        family: super::DocumentCapabilityFamily,
    },
    /// A node id does not exist in this document.
    InvalidNode,
    /// An occurrence id does not exist in this document.
    InvalidOccurrence,
    /// A fact id does not exist in this document.
    InvalidFact,
    /// A scoped handle belongs to another document.
    StaleOrForeignHandle,
    /// The demanded semantic shape cannot become a jqf value.
    UnrepresentableSemantic,
    /// This reader hit a container that is still a source span.
    ///
    /// A span-backed container has no occurrences. Projecting it would look empty. Materialize it instead.
    UnmaterializedContainerSpan,
    /// Semantic graph materialization encountered a cycle.
    CyclicSemanticGraph,
    /// The document or materialization allocation failed.
    Allocation,
    /// Checked document size or position arithmetic overflowed.
    ArithmeticOverflow,
    /// The proposed document structure violated a canonical invariant.
    InvalidDocument,
    /// A reader previously failed and cannot resume or establish completion.
    ReaderFailed,
    /// Request resource accounting rejected work.
    Resource(ResourceError),
    /// Host cancellation or deadline control stopped work.
    Control(ControlError),
}

/// Consumer-facing class of a [`DataError`].
///
/// Codecs and builtins match this instead of dumping every remaining variant as one internal-contract class.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DataErrorClass {
    /// Host pressure: [`DataError::Resource`] or [`DataError::Control`].
    Host,
    /// Memory or checked arithmetic: [`DataError::Allocation`] or [`DataError::ArithmeticOverflow`].
    Budget,
    /// A demanded reader capability is not retained: [`DataError::CapabilityUnavailable`].
    Absent,
    /// The demanded semantic shape cannot become a value: [`DataError::UnrepresentableSemantic`] or
    /// [`DataError::CyclicSemanticGraph`].
    Unrepresentable,
    /// A document, handle, coverage, or reader contract defect.
    Broken,
}

impl DataError {
    /// Classifies this error for a codec or builtin boundary.
    #[must_use]
    pub const fn class(self) -> DataErrorClass {
        match self {
            Self::Resource(_) | Self::Control(_) => DataErrorClass::Host,
            Self::Allocation | Self::ArithmeticOverflow => DataErrorClass::Budget,
            Self::CapabilityUnavailable { .. } => DataErrorClass::Absent,
            Self::UnrepresentableSemantic | Self::CyclicSemanticGraph => DataErrorClass::Unrepresentable,
            Self::ContradictoryCoverage { .. }
            | Self::InvalidNode
            | Self::InvalidOccurrence
            | Self::InvalidFact
            | Self::StaleOrForeignHandle
            | Self::UnmaterializedContainerSpan
            | Self::InvalidDocument
            | Self::ReaderFailed => DataErrorClass::Broken,
        }
    }
}

impl From<ResourceError> for DataError {
    fn from(value: ResourceError) -> Self {
        Self::Resource(value)
    }
}

impl From<ControlError> for DataError {
    fn from(value: ControlError) -> Self {
        Self::Control(value)
    }
}

impl From<CooperativeError> for DataError {
    fn from(value: CooperativeError) -> Self {
        match value {
            CooperativeError::Control(error) => Self::Control(error),
            CooperativeError::Memory(error) => Self::Resource(error),
        }
    }
}

impl fmt::Display for DataError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapabilityUnavailable { capability } => {
                write!(formatter, "document capability unavailable: {capability}")
            }
            Self::ContradictoryCoverage { family } => {
                write!(formatter, "document coverage contradicts retained {family}")
            }
            Self::InvalidNode => formatter.write_str("invalid document node"),
            Self::InvalidOccurrence => formatter.write_str("invalid document occurrence"),
            Self::InvalidFact => formatter.write_str("invalid document fact"),
            Self::StaleOrForeignHandle => formatter.write_str("stale or foreign document handle"),
            Self::UnrepresentableSemantic => formatter.write_str("document shape is not representable as a value"),
            Self::UnmaterializedContainerSpan => formatter.write_str("document container span is not materialized"),
            Self::CyclicSemanticGraph => formatter.write_str("semantic graph contains a cycle"),
            Self::Allocation => formatter.write_str("document allocation failed"),
            Self::ArithmeticOverflow => formatter.write_str("document arithmetic overflow"),
            Self::InvalidDocument => formatter.write_str("invalid canonical document structure"),
            Self::ReaderFailed => formatter.write_str("document reader is terminal after an earlier failure"),
            Self::Resource(error) => write!(formatter, "document resource failure: {error}"),
            Self::Control(error) => write!(formatter, "document control failure: {error}"),
        }
    }
}

impl core::error::Error for DataError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Resource(error) => Some(error),
            Self::Control(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod data_error_class_tests {
    use super::{DataError, DataErrorClass};
    use crate::document::DocumentCapability;

    #[test]
    fn class_splits_budget_absent_and_unrepresentable() {
        assert_eq!(DataError::Allocation.class(), DataErrorClass::Budget);
        assert_eq!(DataError::ArithmeticOverflow.class(), DataErrorClass::Budget);
        assert_eq!(
            DataError::CapabilityUnavailable {
                capability: DocumentCapability::AttachedFacts,
            }
            .class(),
            DataErrorClass::Absent
        );
        assert_eq!(
            DataError::UnrepresentableSemantic.class(),
            DataErrorClass::Unrepresentable
        );
        assert_eq!(DataError::CyclicSemanticGraph.class(), DataErrorClass::Unrepresentable);
        assert_eq!(DataError::InvalidDocument.class(), DataErrorClass::Broken);
    }
}

#[cfg(all(test, target_pointer_width = "64"))]
mod layout_baseline_tests {
    use core::mem::{align_of, size_of};

    use super::{
        DocumentStorageOwner, InlineDocumentStorage, IntrinsicTagRef, KeyPayload, NodeRecord, NodeSemantic,
        ObjectWinnerEntry, OccurrenceRecord, RichOccurrenceSidecar, SemanticEdge, SharedDocumentStorage, StorageRange,
        StoredDocumentFact, StoredOccurrenceKey, StoredSemanticNode, WidePayload,
    };
    use crate::document::{DocumentNodeKindId, NodeKindBindingId, OccurrenceRoleBindingId, TextRef};
    use crate::{NodeId, OccurrenceId, OccurrenceRoleId};

    #[test]
    fn compact4_production_relationship_layout_is_pinned() {
        assert_eq!((size_of::<SemanticEdge>(), align_of::<SemanticEdge>()), (4, 4));
        assert_eq!(
            (size_of::<RichOccurrenceSidecar>(), align_of::<RichOccurrenceSidecar>()),
            (20, 4)
        );
        assert_eq!((size_of::<KeyPayload>(), align_of::<KeyPayload>()), (8, 4));
    }

    #[test]
    fn current_document_storage_component_layout_is_pinned() {
        // The fixed node header: small/common payloads inline, large/uncommon scalar payloads relocated to the typed
        // `wide` side arena, and the intrinsic tag compacted to a 4-byte reference into the `tags` side arena. The
        // header fits the accounted payload set at 40: codecs store integers as `StoredInteger`, floats as
        // `AccountedFloat`, decimals as `StoredDecimal`.
        assert_eq!(size_of::<NodeRecord>(), 40);
        assert_eq!(align_of::<NodeRecord>(), 8);
        assert_eq!(size_of::<NodeSemantic>(), 16);
        assert_eq!(align_of::<NodeSemantic>(), 8);
        assert_eq!(size_of::<IntrinsicTagRef>(), 4);
        // The transient builder-facing payload and the wide side-arena element pin the layout law: bare
        // `String`/`Vec<u8>` payloads (24 B each), no tracking wrappers alongside them.
        assert_eq!(size_of::<StoredSemanticNode>(), 48);
        assert_eq!(align_of::<StoredSemanticNode>(), 8);
        assert_eq!(size_of::<WidePayload>(), 48);
        assert_eq!(align_of::<WidePayload>(), 8);
        assert_eq!(size_of::<OccurrenceRecord>(), 32);
        assert_eq!(align_of::<OccurrenceRecord>(), 4);
        assert_eq!(size_of::<TextRef>(), 12);
        assert_eq!(size_of::<StoredOccurrenceKey>(), 12);
        // Both schema ids carry an identity text, which is one shared pointer: it holds its own allocation and needs no
        // discriminant beside it.
        assert_eq!(size_of::<DocumentNodeKindId>(), 8);
        assert_eq!(size_of::<OccurrenceRoleId>(), 8);
        assert_eq!(size_of::<NodeKindBindingId>(), 4);
        assert_eq!(size_of::<OccurrenceRoleBindingId>(), 4);
        // `FactPayload` holds an owned `Integer`, whose inline machine arm (`i64` value + the 20-byte canonical
        // spelling + its length) is 40 bytes against the bare `String`'s 24. It is the widest payload in the fact.
        // Nothing on the DOCUMENT hot path moves — `NodeRecord`, `NodeSemantic`, `StoredSemanticNode` and
        // `WidePayload` all still hold document text through `TextRef`, never through an owned `Integer`. The
        // predecessor's `Option<Provenance>` field (128 bytes) went with the provenance records, and the `order` field
        // (a duplicate of the fact table position) was cut after it — the fact is now its
        // identity/owner/role/kind/schema/payload record plus an optional authored source span (markup attribute
        // quoted-value ranges).
        assert_eq!(size_of::<StoredDocumentFact>(), 48);
        assert_eq!(size_of::<ObjectWinnerEntry>(), 16);
        assert_eq!(align_of::<ObjectWinnerEntry>(), 4);
        assert_eq!(size_of::<StorageRange>(), 8);
        assert_eq!(size_of::<NodeId>(), 4);
        assert_eq!(size_of::<OccurrenceId>(), 4);
    }

    #[test]
    fn shared_schema_owner_carries_no_inline_schema_padding() {
        assert!(
            size_of::<DocumentStorageOwner<'static>>() < size_of::<InlineDocumentStorage<'static>>(),
            "the owner discriminant must select a concrete allocation instead of reserving the inline schema arm"
        );
        assert!(
            size_of::<SharedDocumentStorage<'static>>() + 128 < size_of::<InlineDocumentStorage<'static>>(),
            "a shared document allocation must carry a schema pointer, not inline-schema padding"
        );
    }
}
