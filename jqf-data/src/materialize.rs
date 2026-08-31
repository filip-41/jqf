//! Turn a document node into an owned [`crate::Value`].
//!
//! Walks iteratively. A cycle on the active path fails. Core tags stay document facts; non-core tags become
//! [`crate::Value::Tagged`].

use alloc::vec::Vec;
use jqf_resource::ResourceContext;

use crate::document::{
    AccountedLocalDateTime, AccountedLocalTime, AccountedOffsetDateTime, ArrayItems, NodeSemantic, ObjectEntries,
    WidePayload,
};
use crate::{
    Array, DataError, Document, FractionalSecond, IntrinsicTagSemantics, LocalDateTime, LocalTime, NodeId,
    ObjectBuilder, ObjectKey, OffsetDateTime, TagId, TemporalError, Value,
};

/// Hash sets in one traversal's object-key reuse cache. A power of two, so the set index is a mask.
const KEY_CACHE_SETS: usize = 16;

/// Keys retained per set.
///
/// The cache is set-ASSOCIATIVE rather than direct-mapped because a record's field names are not independent: they all
/// appear in every record, so two names sharing one slot evict each other on every single record and the table never
/// hits at all. Four ways absorb the collisions a record shape actually produces; a fifth colliding name simply
/// allocates, which is what the caller did unconditionally before.
const KEY_CACHE_WAYS: usize = 4;

/// The longest key text the cache retains.
///
/// Interning pays off on the short, repeated names a record shape is built from; a long key amortizes its own copy
/// against its own length. Refusing long keys also bounds what an idle cache can pin to `KEY_CACHE_SETS` ×
/// `KEY_CACHE_WAYS` × `KEY_CACHE_MAX_KEY_BYTES`, which matters because the cache outlives the values it served and is
/// not itself charged to the request ledger.
const KEY_CACHE_MAX_KEY_BYTES: usize = 64;

enum Frame<'document> {
    Array {
        node: NodeId,
        tag: Option<TagId>,
        items: ArrayItems<'document>,
        cursor: usize,
        /// The frame's complete element run, reserved in ONE ledger growth when the frame opens (the array projection
        /// carries the final count). Transient working memory: the run becomes an owned value only through
        /// [`Array::try_from_vec`], which charges every element it carries before the array can be observed.
        values: Vec<Value>,
    },
    Object {
        node: NodeId,
        tag: Option<TagId>,
        entries: ObjectEntries<'document>,
        lookup: &'document [u32],
        cursor: usize,
        values: ObjectBuilder,
        lookup_positions: Vec<usize>,
    },
    /// One tag-layer wrap: the payload node materializes into `state.produced` and this frame wraps it with the layer's
    /// tag.
    Tag { node: NodeId, tag: TagId },
}

/// Cycle-detection scratch that does not borrow a document.
///
/// Holds a node-index bitmap and a small key cache. Reuse it across documents with
/// [`Document::materialize_root_with`](crate::Document::materialize_root_with) or
/// [`Document::materialize_node_with`](crate::Document::materialize_node_with). After each call (success or error) the
/// bitmap is clear again; a panic unwinds with stale bits, and the next call clears them at entry. `materialize_root`
/// and `materialize_node` allocate a fresh workspace each time.
pub struct MaterializeWorkspace {
    active: Vec<bool>,
    /// Raised while a walk is (or was) in flight; every normal exit — success or error — lowers it, because both
    /// paths clear every bit they set. Only a panic leaves it raised, so the next walk's entry sweep runs then and only
    /// then.
    dirty: bool,
    keys: ObjectKeyCache,
    /// Empty between calls. The allocation is reused by transmuting an empty `Vec<Frame<'static>>` to the document
    /// lifetime of the next walk — sound because no `Frame` values exist while the vec is empty.
    frames: Vec<Frame<'static>>,
}

impl Default for MaterializeWorkspace {
    fn default() -> Self {
        Self::new()
    }
}

impl MaterializeWorkspace {
    /// Empty workspace. Grows on first use.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            active: Vec::new(),
            dirty: false,
            keys: ObjectKeyCache::new(),
            frames: Vec::new(),
        }
    }

    fn take_frames<'document>(&mut self) -> Vec<Frame<'document>> {
        let frames = core::mem::take(&mut self.frames);
        debug_assert!(frames.is_empty());
        // SAFETY: the vec is empty, so no `Frame` values have their
        // document lifetime extended. Only the allocation moves.
        unsafe { core::mem::transmute::<Vec<Frame<'static>>, Vec<Frame<'document>>>(frames) }
    }

    fn restore_frames(&mut self, mut frames: Vec<Frame<'_>>) {
        frames.clear();
        // SAFETY: `clear` dropped every `Frame`; the empty allocation is lifetime-free.
        self.frames = unsafe { core::mem::transmute::<Vec<Frame<'_>>, Vec<Frame<'static>>>(frames) };
    }

    /// Grows the workspace so it can service at least `node_count` node indices. Added slots are clear; existing slots
    /// are untouched — between walks the bitmap is always clear (the walk clears each bit it sets on frame pop,
    /// `clear_active_frames` clears on the error path, and panic-stale bits are swept at walk entry via
    /// [`Self::dirty`]), so growth never disturbs the invariant.
    fn reserve_nodes(&mut self, node_count: usize) -> Result<(), DataError> {
        if self.active.len() >= node_count {
            return Ok(());
        }
        let additional = node_count - self.active.len();
        self.active.try_reserve(additional).map_err(|_| DataError::Allocation)?;
        self.active.resize(node_count, false);
        Ok(())
    }
}

struct SyncTraversalState {
    current: Option<NodeId>,
    produced: Option<Value>,
}

pub(crate) fn materialize_document_node(
    document: &Document<'_>,
    root: NodeId,
    resources: &mut ResourceContext<'_>,
) -> Result<Value, DataError> {
    let mut workspace = MaterializeWorkspace::new();
    materialize_node_with_workspace(document, &mut workspace, root, resources)
}

/// Same as a one-shot materialize, using `workspace` instead of a fresh cycle bitmap. The workspace is grown to this
/// document and left clear on return, including on error.
pub(crate) fn materialize_node_with_workspace(
    document: &Document<'_>,
    workspace: &mut MaterializeWorkspace,
    root: NodeId,
    resources: &mut ResourceContext<'_>,
) -> Result<Value, DataError> {
    // A panicked earlier walk left stale bits behind; one O(n) sweep restores the between-walks invariant before this
    // walk reads the bitmap.
    if workspace.dirty {
        workspace.active.fill(false);
        workspace.dirty = false;
    }
    workspace.reserve_nodes(document.node_count())?;
    let mut frames = workspace.take_frames();
    workspace.dirty = true;
    let result = materialize_with_scratch(
        document,
        root,
        &mut workspace.active,
        &mut frames,
        &mut workspace.keys,
        resources,
    );
    if result.is_err() {
        clear_active_frames(&mut frames, &mut workspace.active);
    }
    workspace.restore_frames(frames);
    workspace.dirty = false;
    // String-types mode converts temporal values as they leave the document.
    match result {
        Ok(value) if resources.types_as_strings() => cast_extended_to_strings(value, resources),
        other => other,
    }
}

/// Replace each temporal value with its canonical text as a string.
///
/// Used when the request asked for string types. The document still stores the rich kinds; only a value that is
/// materialized and written back loses them. Bytes stay bytes — they have no agreed text form.
///
/// Recursive. Each container charges one nesting level.
pub(crate) fn cast_extended_to_strings(value: Value, resources: &ResourceContext<'_>) -> Result<Value, DataError> {
    // One text projection for the four temporal kinds: canonical text, then an owned string value.
    fn text_value(
        written: impl FnOnce(&mut alloc::string::String) -> Result<(), TemporalError>,
    ) -> Result<Value, DataError> {
        let mut out = alloc::string::String::new();
        written(&mut out).map_err(|error| match error {
            TemporalError::Allocation => DataError::Allocation,
            TemporalError::InvalidFraction | TemporalError::Syntax | TemporalError::OutOfRange => {
                DataError::InvalidDocument
            }
        })?;
        Value::try_string(&out).map_err(|_| DataError::Allocation)
    }
    match value {
        Value::LocalDate(date) => text_value(|out| date.write_text(out)),
        Value::LocalTime(time) => text_value(|out| time.write_text(out)),
        Value::LocalDateTime(datetime) => text_value(|out| datetime.write_text(out)),
        Value::OffsetDateTime(datetime) => text_value(|out| datetime.write_text(out)),
        Value::Array(items) => {
            let _depth = resources.enter_nesting()?;
            let mut out = Vec::new();
            out.try_reserve_exact(items.len()).map_err(|_| DataError::Allocation)?;
            for item in &items {
                out.push(cast_extended_to_strings(item.clone(), resources)?);
            }
            Array::try_from_vec(out)
                .map(Value::Array)
                .map_err(|_| DataError::Allocation)
        }
        Value::Object(object) => {
            let _depth = resources.enter_nesting()?;
            let mut builder = ObjectBuilder::try_with_capacity(object.len()).map_err(|_| DataError::Allocation)?;
            for entry in &object {
                builder
                    .try_insert_last(
                        entry.clone_key(),
                        cast_extended_to_strings(entry.value().clone(), resources)?,
                    )
                    .map_err(|_| DataError::Allocation)?;
            }
            builder
                .try_finish()
                .map(Value::Object)
                .map_err(|_| DataError::Allocation)
        }
        // Bytes stay rich (no text encoding); the tagged wrapper recurses into its payload.
        Value::Tagged { tag, payload } => {
            let _depth = resources.enter_nesting()?;
            let inner = cast_extended_to_strings((*payload).clone(), resources)?;
            Value::try_tagged(tag, inner).map_err(|_| DataError::Allocation)
        }
        other => Ok(other),
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one iterative state machine keeps traversal and cleanup ordering explicit"
)]
fn materialize_with_scratch<'document>(
    document: &'document Document<'_>,
    root: NodeId,
    active: &mut [bool],
    frames: &mut Vec<Frame<'document>>,
    keys: &mut ObjectKeyCache,
    resources: &mut ResourceContext<'_>,
) -> Result<Value, DataError> {
    document.node_record(root)?;
    let mut state = SyncTraversalState {
        current: Some(root),
        produced: None,
    };

    loop {
        if let Some(value) = advance_sync(document, active, frames, &mut state, keys, resources)? {
            return Ok(value);
        }
    }
}

#[allow(
    clippy::inline_always,
    clippy::too_many_lines,
    reason = "the retained materialization benchmark shows this per-node hot transition must inline"
)]
#[inline(always)]
fn advance_sync<'document>(
    document: &'document Document<'_>,
    active: &mut [bool],
    frames: &mut Vec<Frame<'document>>,
    state: &mut SyncTraversalState,
    keys: &mut ObjectKeyCache,
    resources: &mut ResourceContext<'_>,
) -> Result<Option<Value>, DataError> {
    if let Some(node) = state.current.take() {
        let index = node.index();
        if *active.get(index).ok_or(DataError::InvalidNode)? {
            return Err(DataError::CyclicSemanticGraph);
        }
        let record = document.node_record(node)?;
        let tag = document
            .resolve_intrinsic_tag(record.intrinsic_tag)
            .filter(|tag| tag.semantics() == IntrinsicTagSemantics::Tagged)
            .map(|tag| tag.tag().clone());
        match &record.semantic {
            NodeSemantic::Array { .. } => {
                frames.try_reserve(1).map_err(|_| DataError::Allocation)?;
                let items = document.array_projection_checked_from(record)?;
                let mut values = Vec::new();
                values
                    .try_reserve_exact(items.len())
                    .map_err(|_| DataError::Allocation)?;
                active[index] = true;
                frames.push(Frame::Array {
                    node,
                    tag,
                    items,
                    cursor: 0,
                    values,
                });
                if let Some(target) = items.first() {
                    state.current = Some(target);
                } else {
                    state.produced = Some(finish_frame(
                        frames.pop().expect("the array frame was just pushed"),
                        active,
                    )?);
                }
            }
            NodeSemantic::Object { .. } => {
                frames.try_reserve(1).map_err(|_| DataError::Allocation)?;
                let (entries, lookup) = document.object_projection_lookup_from(record)?;
                let values = ObjectBuilder::try_with_capacity(entries.len()).map_err(|_| DataError::Allocation)?;
                let mut lookup_positions = Vec::new();
                lookup_positions
                    .try_reserve_exact(lookup.len())
                    .map_err(|_| DataError::Allocation)?;
                active[index] = true;
                frames.push(Frame::Object {
                    node,
                    tag,
                    entries,
                    lookup,
                    cursor: 0,
                    values,
                    lookup_positions,
                });
                if let Some(entry) = entries.first() {
                    state.current = Some(entry.target);
                } else {
                    state.produced = Some(finish_frame(
                        frames.pop().expect("the object frame was just pushed"),
                        active,
                    )?);
                }
            }
            NodeSemantic::Text(_) => {
                let value = document
                    .semantic_text(&record.semantic)
                    .ok_or(DataError::InvalidDocument)?;
                state.produced = Some(wrap_tag(
                    Value::try_string(value).map_err(|_| DataError::Allocation)?,
                    tag,
                )?);
            }
            NodeSemantic::StoredInteger(value) => {
                let value = document.text(*value).ok_or(DataError::InvalidDocument)?;
                // `from_canonical` preserves a retained `-0` sign (the document-decode path keeps it; `parse`
                // normalizes to `0` under the program-literal law).
                let integer = crate::Integer::from_canonical_ref(value).map_err(|_| DataError::InvalidDocument)?;
                state.produced = Some(wrap_tag(
                    Value::Number(crate::Number::try_integer_unaccounted(integer).map_err(|_| DataError::Allocation)?),
                    tag,
                )?);
            }
            NodeSemantic::AccountedFloat(value) => {
                state.produced = Some(wrap_tag(Value::Number(crate::Number::float(*value)), tag)?);
            }
            NodeSemantic::Wide { id, .. } => match document.wide_payload(*id)? {
                WidePayload::AccountedBytes(value) => {
                    let slice = value.as_slice();
                    let mut bytes = Vec::new();
                    bytes
                        .try_reserve_exact(slice.len())
                        .map_err(|_| DataError::Allocation)?;
                    bytes.extend_from_slice(slice);
                    state.produced = Some(wrap_tag(
                        Value::try_bytes(&bytes).map_err(|_| DataError::Allocation)?,
                        tag,
                    )?);
                }
                payload => {
                    state.produced = Some(materialize_wide_scalar(document, payload, tag)?);
                }
            },
            NodeSemantic::ContainerSpan { .. } => {
                state.produced = Some(wrap_tag(
                    materialize_container_span(document, &record.semantic, resources)?,
                    tag,
                )?);
            }
            NodeSemantic::Unrepresentable => {
                let Some(tag) = tag else {
                    // A bare kindless node is meaning-bearing topology with no owned-value projection: a terminal
                    // refusal, stated here rather than routed through a helper that refuses this kind unconditionally.
                    return Err(DataError::UnrepresentableSemantic);
                };
                // A tag-LAYER node: its owned payload projection carries exactly one item (the projection passes
                // enforce keyless single-ownership), which becomes the wrapped value.
                let items = document.array_projection_checked_from(record)?;
                let (1, Some(payload)) = (items.len(), items.first()) else {
                    return Err(DataError::InvalidDocument);
                };
                frames.try_reserve(1).map_err(|_| DataError::Allocation)?;
                active[index] = true;
                frames.push(Frame::Tag { node, tag });
                state.current = Some(payload);
            }
            semantic => {
                state.produced = Some(materialize_bounded_scalar(semantic, tag)?);
            }
        }
        return Ok(None);
    }

    let Some(value) = state.produced.take() else {
        return Err(DataError::InvalidDocument);
    };
    let Some(frame) = frames.last_mut() else {
        return Ok(Some(value));
    };
    match frame {
        Frame::Array {
            items, cursor, values, ..
        } => {
            // The run reserved every slot when the frame opened, so this cannot reallocate and cannot fail.
            values.push(value);
            *cursor += 1;
            if let Some(target) = items.get(*cursor) {
                state.current = Some(target);
            } else {
                let frame = frames.pop().expect("the active array frame exists");
                state.produced = Some(finish_frame(frame, active)?);
            }
        }
        Frame::Object {
            entries,
            lookup,
            cursor,
            values,
            lookup_positions,
            ..
        } => {
            let entry = entries.get(*cursor).ok_or(DataError::InvalidDocument)?;
            let key = document
                .object_projection_key(&entry)
                .ok_or(DataError::InvalidDocument)?;
            values
                .try_insert_last(keys.try_intern(key)?, value)
                .map_err(|_| DataError::Allocation)?;
            // Small objects keep an unsorted lookup segment; their sorted positions are not recorded and the frame
            // finishes through the unique-adopt path.
            if entries.len() > crate::document::SMALL_OBJECT_WINNER_LIMIT {
                lookup_positions.push(
                    usize::try_from(*lookup.get(*cursor).ok_or(DataError::InvalidDocument)?)
                        .map_err(|_| DataError::ArithmeticOverflow)?,
                );
            }
            *cursor += 1;
            if let Some(entry) = entries.get(*cursor) {
                state.current = Some(entry.target);
            } else {
                let frame = frames.pop().expect("the active object frame exists");
                state.produced = Some(finish_frame(frame, active)?);
            }
        }
        Frame::Tag { node, tag } => {
            let index = node.index();
            active[index] = false;
            let wrapped = wrap_tag(value, Some(tag.clone()))?;
            frames.pop().expect("the active tag frame exists");
            state.produced = Some(wrapped);
        }
    }
    Ok(None)
}

fn clear_active_frames(frames: &mut Vec<Frame<'_>>, active: &mut [bool]) {
    for frame in frames.drain(..) {
        let node = match frame {
            Frame::Array { node, .. } | Frame::Object { node, .. } | Frame::Tag { node, .. } => node,
        };
        active[node.index()] = false;
    }
}

fn materialize_bounded_scalar(semantic: &NodeSemantic, tag: Option<TagId>) -> Result<Value, DataError> {
    let value = match semantic {
        NodeSemantic::Null => Value::Null,
        NodeSemantic::Bool(value) => Value::Bool(*value),
        NodeSemantic::LocalDate(value) => Value::LocalDate(*value),
        NodeSemantic::StoredInteger(_)
        | NodeSemantic::AccountedFloat(_)
        | NodeSemantic::Text(_)
        | NodeSemantic::Array { .. }
        | NodeSemantic::Object { .. }
        | NodeSemantic::ContainerSpan { .. }
        | NodeSemantic::Wide { .. } => {
            return Err(DataError::InvalidDocument);
        }
        NodeSemantic::Unrepresentable => return Err(DataError::UnrepresentableSemantic),
    };
    wrap_tag(value, tag)
}

/// Reads one span-backed container subtree into an owned value, through the format-owned reader the decoder installed.
///
/// The charge-at-materialization seam: an untouched span costs its node record and nothing else; the subtree's bytes
/// are paid for here, by the toucher, against the toucher's ledger. The value is FRESH — nothing is written back onto
/// the node — so a materialized subtree is never shared.
fn materialize_container_span(
    document: &Document<'_>,
    semantic: &NodeSemantic,
    resources: &mut ResourceContext<'_>,
) -> Result<Value, DataError> {
    let NodeSemantic::ContainerSpan { text, .. } = semantic else {
        return Err(DataError::InvalidDocument);
    };
    let materializer = document.span_materializer().ok_or(DataError::InvalidDocument)?;
    let span_bytes = document.bytes(*text).ok_or(DataError::InvalidDocument)?;
    // One deferred subtree read back: the explain surface's `materialized=` fact. Counted on success AND failure —
    // the span was touched either way — and it is a diagnostics counter, not a ledger charge.
    resources.bump_lazy_materialized_spans();
    materializer.materialize_span_bytes(span_bytes, resources)
}

fn materialize_wide_scalar(
    document: &Document<'_>,
    payload: &WidePayload,
    tag: Option<TagId>,
) -> Result<Value, DataError> {
    let value = match payload {
        WidePayload::StoredDecimal { coefficient, scale } => {
            let coefficient = document.text(*coefficient).ok_or(DataError::InvalidDocument)?;
            let integer = crate::Integer::from_canonical_ref(coefficient).map_err(|_| DataError::InvalidDocument)?;
            // Stored decimals keep trailing zeroes (`1.000` in, `1.000` out). Comparison that wants `1.000 == 1` lives
            // with the caller.
            let decimal = crate::Decimal::from_literal_validated_parts(integer, *scale);
            Value::Number(crate::Number::try_decimal_unaccounted(decimal).map_err(|_| DataError::Allocation)?)
        }
        WidePayload::AccountedLocalTime(value) => Value::LocalTime(materialize_accounted_local_time(value)?),
        WidePayload::AccountedLocalDateTime(value) => {
            Value::LocalDateTime(materialize_accounted_local_date_time(value)?)
        }
        WidePayload::AccountedOffsetDateTime(value) => {
            Value::OffsetDateTime(materialize_accounted_offset_date_time(value)?)
        }
        WidePayload::AccountedBytes(_) => {
            return Err(DataError::InvalidDocument);
        }
    };
    wrap_tag(value, tag)
}

fn materialize_accounted_local_time(value: &AccountedLocalTime) -> Result<LocalTime, DataError> {
    let fraction = FractionalSecond::parse(value.fraction.as_str()).map_err(|error| match error {
        TemporalError::InvalidFraction | TemporalError::Syntax | TemporalError::OutOfRange => {
            DataError::InvalidDocument
        }
        TemporalError::Allocation => DataError::Allocation,
    })?;
    LocalTime::new(value.hour, value.minute, value.second, fraction).ok_or(DataError::InvalidDocument)
}

fn materialize_accounted_local_date_time(value: &AccountedLocalDateTime) -> Result<LocalDateTime, DataError> {
    Ok(LocalDateTime {
        date: value.date,
        time: materialize_accounted_local_time(&value.time)?,
    })
}

fn materialize_accounted_offset_date_time(value: &AccountedOffsetDateTime) -> Result<OffsetDateTime, DataError> {
    Ok(OffsetDateTime {
        local: materialize_accounted_local_date_time(&value.local)?,
        offset: value.offset,
    })
}

fn finish_frame(frame: Frame<'_>, active: &mut [bool]) -> Result<Value, DataError> {
    match frame {
        Frame::Array { node, tag, values, .. } => {
            active[node.index()] = false;
            let array = Array::try_from_vec(values).map_err(|_| DataError::Allocation)?;
            wrap_tag(Value::Array(array), tag)
        }
        Frame::Object {
            node,
            tag,
            entries,
            values,
            lookup_positions,
            ..
        } => {
            active[node.index()] = false;
            // The document already resolved this object's entries to unique winners. A small object's lookup segment is
            // never sorted: the entries are adopted as-is (below the linear threshold `try_finish_unique` builds no
            // index and performs no scan), so the unique finish is the same result with none of the re-derivation. An
            // object above the threshold hands the sorted positions over with the winners, and the cooperative path has
            // always taken that finish.
            let object = if entries.len() <= crate::document::SMALL_OBJECT_WINNER_LIMIT {
                values.try_finish_unique().map_err(|_| DataError::Allocation)?
            } else {
                values
                    .try_finish_unique_with_lookup(lookup_positions)
                    .map_err(|_| DataError::Allocation)?
            };
            wrap_tag(Value::Object(object), tag)
        }
        Frame::Tag { node, .. } => {
            // A tag frame is consumed by the traversal itself (its payload wraps through `state.produced`), never
            // through `finish_frame`.
            active[node.index()] = false;
            Err(DataError::InvalidDocument)
        }
    }
}

fn wrap_tag(value: Value, tag: Option<TagId>) -> Result<Value, DataError> {
    match tag {
        Some(tag) => Value::try_tagged(tag, value).map_err(|_| DataError::Allocation),
        None => Ok(value),
    }
}

/// Reused object keys for one materialization run.
///
/// A document repeats its record shape, so the same key strings arrive once per record. Materializing them
/// independently allocated a fresh copy every time, while every other payload in a `Value` was already refcount-shared.
/// This table makes the repeat a refcount bump.
///
/// Interned keys are content-addressed: a hit means the TEXT matched, and an [`ObjectKey`] carries no document, node or
/// span identity. A cache may therefore be retained across values and across documents — which is what makes it
/// useful at all, since a per-value cache would see each of a record's distinct keys exactly once and never hit.
struct ObjectKeyCache {
    /// `KEY_CACHE_SETS` sets of `KEY_CACHE_WAYS` ways, flattened, and EMPTY until the first object key is interned.
    ///
    /// The table is heap-backed rather than inline because the workspace holding it is a long-lived field on the run,
    /// reached once per materialized value. An inline table made the workspace a kilobyte wide and cost every
    /// materialization, scalar lanes included — documents with no object keys at all. Lazily allocated, a document of
    /// scalars or arrays never touches this at all.
    ways: Vec<Option<ObjectKey>>,
}

impl ObjectKeyCache {
    const fn new() -> Self {
        Self { ways: Vec::new() }
    }

    /// Returns a shared key holding `text`, allocating only on a miss.
    ///
    /// Each set is kept most-recent-first, so a miss evicts the way a record shape has gone longest without naming —
    /// the key least likely to be the next one asked for.
    fn try_intern(&mut self, text: &str) -> Result<ObjectKey, DataError> {
        if text.len() > KEY_CACHE_MAX_KEY_BYTES {
            return ObjectKey::try_from_str(text).map_err(|_| DataError::Allocation);
        }
        if self.ways.is_empty() {
            let capacity = KEY_CACHE_SETS * KEY_CACHE_WAYS;
            self.ways
                .try_reserve_exact(capacity)
                .map_err(|_| DataError::Allocation)?;
            self.ways.resize_with(capacity, || None);
        }
        let base = set_of(text) * KEY_CACHE_WAYS;
        let set = &mut self.ways[base..base + KEY_CACHE_WAYS];
        if let Some(way) = set
            .iter()
            .position(|way| way.as_ref().is_some_and(|key| key.as_str() == text))
        {
            set[..=way].rotate_right(1);
            let key = set[0].as_ref().expect("the promoted way holds the hit key");
            return Ok(key.clone_shared());
        }
        let key = ObjectKey::try_from_str(text).map_err(|_| DataError::Allocation)?;
        set.rotate_right(1);
        set[0] = Some(key.clone_shared());
        Ok(key)
    }
}

/// FNV-1a over the key text, masked to a set.
///
/// Field names are short, so the whole hash is a handful of cycles — far below the allocation it replaces — and
/// FNV-1a spreads the same-length, shared-prefix names a record shape tends to use across distinct sets, where a
/// cheaper length-or-first-byte index would collide them wholesale.
///
/// Truncating the hash to a set index is the point of a hash, not a lossy cast bug: only the low bits are consulted and
/// any value is a valid set.
#[allow(
    clippy::cast_possible_truncation,
    reason = "a hash is masked to a set index; every bit pattern is a valid set"
)]
fn set_of(text: &str) -> usize {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (hash as usize) & (KEY_CACHE_SETS - 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{format, string::String};
    use jqf_resource::{ContinueControl, RequestAccount, ResourceLimits, WorkMeter};

    static CONTROL: ContinueControl = ContinueControl;

    fn resources() -> ResourceContext<'static> {
        let account = RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
            .expect("account");
        let work = WorkMeter::try_new_v1(1).expect("work meter");
        ResourceContext::new(account, &CONTROL, work).expect("ledger")
    }
    /// Distinct key texts that all land in one cache set, so the ways are exercised without depending on which sets
    /// real field names hash to.
    fn colliding_keys(count: usize) -> Vec<String> {
        let target = set_of("a");
        let mut out = Vec::new();
        let mut i = 0usize;
        while out.len() < count {
            let candidate = format!("k{i}");
            if set_of(&candidate) == target {
                out.push(candidate);
            }
            i += 1;
        }
        out
    }

    fn set_texts<'a>(cache: &'a ObjectKeyCache, text: &str) -> Vec<&'a str> {
        let base = set_of(text) * KEY_CACHE_WAYS;
        cache.ways[base..base + KEY_CACHE_WAYS]
            .iter()
            .map(|way| way.as_ref().map(ObjectKey::as_str).unwrap_or_default())
            .collect()
    }

    /// A panicked walk leaves `active` bits set; the next walk must sweep them at entry instead of reporting a cycle
    /// that the document does not have.
    #[test]
    fn a_panicked_walks_stale_bits_are_swept_before_the_next_walk() {
        let mut resources = resources();
        let mut builder = crate::AccountedDocumentBuilder::try_new("test", None).expect("builder");
        let root_node = builder
            .add_node("test.bool", crate::AccountedSemanticNode::Bool(true), None, &resources)
            .expect("root");
        let document = builder.finish(root_node, &resources).expect("document");

        let mut workspace = MaterializeWorkspace::new();
        // Poison by hand, exactly as an unwound walk would leave the bitmap.
        workspace.reserve_nodes(document.node_count()).expect("capacity");
        workspace.active[0] = true;
        workspace.dirty = true;

        match materialize_node_with_workspace(&document, &mut workspace, root_node, &mut resources) {
            Ok(crate::Value::Bool(true)) => {}
            other => panic!("the stale bit must not surface as a cycle; got {other:?}"),
        }
        assert!(
            !workspace.active.iter().any(|bit| *bit),
            "the walk leaves no bits behind"
        );
    }

    #[test]
    fn repeat_text_hits_one_cached_key() {
        let mut cache = ObjectKeyCache::new();
        let first = cache.try_intern("field").expect("intern");
        let second = cache.try_intern("field").expect("re-intern");
        assert_eq!(first.as_str(), "field");
        assert_eq!(second.as_str(), "field");
        assert_eq!(
            cache.ways.iter().flatten().count(),
            1,
            "a hit must not add a second entry"
        );
    }

    /// Each set is most-recent-first: a hit rotates its way to the front, pushing the others back in their previous
    /// recency order.
    #[test]
    fn a_hit_promotes_its_key_to_the_set_front() {
        let keys = colliding_keys(4);
        let mut cache = ObjectKeyCache::new();
        for key in &keys {
            cache.try_intern(key).expect("intern");
        }
        let [a, b, c, d] = keys.as_slice() else {
            panic!("four colliding keys were generated");
        };

        cache.try_intern(b).expect("hit");
        assert_eq!(set_texts(&cache, b), [b.as_str(), d.as_str(), c.as_str(), a.as_str()]);
    }

    /// A fifth same-set key evicts the way gone longest without a hit — the oldest insert, since nothing was re-asked
    /// for.
    #[test]
    fn a_fifth_colliding_key_evicts_the_least_recently_used_way() {
        let keys = colliding_keys(5);
        let mut cache = ObjectKeyCache::new();
        for key in &keys {
            cache.try_intern(key).expect("intern");
        }
        assert!(!set_texts(&cache, &keys[4]).contains(&""), "the set stays full");
        assert!(
            !set_texts(&cache, &keys[0]).contains(&keys[0].as_str()),
            "the oldest way left"
        );
    }

    /// Long keys bypass the table entirely: they still intern correctly but leave the retained set untouched, bounding
    /// what the cache can pin.
    #[test]
    fn long_keys_pass_through_uncached() {
        let long = "x".repeat(KEY_CACHE_MAX_KEY_BYTES + 1);
        let mut cache = ObjectKeyCache::new();
        let before = cache.ways.len();
        let key = cache.try_intern(&long).expect("pass-through");
        assert_eq!(key.as_str(), long);
        assert_eq!(cache.ways.len(), before, "nothing was cached");
    }
}
