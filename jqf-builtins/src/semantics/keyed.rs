//! The keyed-collect primitive: five modes over one (key, position) table.
//!
//! One job: own what `sort_by`, `group_by`, `unique_by`, `min_by` and `max_by` do once their per-element keys exist —
//! the stable sort under the total order, the equal-key run walk, and the ONE law per mode that says which member of a
//! run survives. The executor's `KeyedCollect` frame owns the COLLECTION of the keys (it drives the key filter once per
//! element); this module owns everything after that, so the five modes differ by a table row rather than by five
//! hand-written loops.
//!
//! [`KEY_MODES`] is a closed table in the discipline of `analysis/count.rs`:
//! five rows, no default arm, each stating the three laws that separate its mode from the others — which member of an
//! equal-key run it publishes, what it publishes for an EMPTY input, and which of the two doubled rejection templates a
//! non-array input takes. A sixth mode is a sixth row, not a widening of an existing one, and a row out of position
//! fails the const assertion below.
//!
//! The primitive underneath — sort a vector of (key, payload) pairs by [`compare_key`] and walk equal-key runs by
//! adjacency — is the one the correlated-join vertical's index build needs. The two sort sites share their law (a
//! `TooDeep` declines rather than inventing an order); the join index builds its own entries because its key is the raw
//! evaluated `Value` and its payload a `u32` position, neither of which is the boxed key law [`KeyValue`] owns.
//!
//! Negative space: it evaluates no filter, allocates no key, and renders no message — the frame owns the drive and
//! the rendering of both operands. What this module contributes to a rejection is the one per-mode fact
//! ([`KeyModeRow::rejection`]) that says WHICH template, because that is a property of the mode and not of the
//! execution that discovered it.
//!
//! [`compare_key`]: super::keyed::compare_key

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use core::cmp::Ordering;

use jqf_data::{Array, Value};
use jqf_resource::ResourceContext;

use super::depth::{TooDeep, comparison_error};
use super::order::NanLaw;
use crate::error::EngineRunError;

/// One element's key beside the position it came from.
///
/// The position is what makes the sort STABLE: equal keys keep ascending original position, so `group_by` hands back
/// each group in input order and `unique_by` keeps the first element of each run.
pub struct KeyedEntry {
    /// The element's key — for a `_by` form, the ARRAY of every output the key filter produced for that element.
    pub key: KeyValue,
    /// The element's original position in the input array.
    pub position: usize,
}

/// One keyed element's key, stored WITHOUT the per-element box.
///
/// The `_by` forms reach their comparator through `map([f])`, so the semantic key is the ARRAY of the filter's outputs
/// — but a one-element array is order-equivalent to its element, and boxing it costs one owned `Array` per row (two
/// allocations: the shared header and the element spine). The collect therefore stores the single-output case UNBOXED
/// and lets [`compare_key`] carry the box level virtually: `One(a)` against `One(b)` is `[a]` against `[b]`, one
/// container level deeper than `a` against `b`.
///
/// `Bare` is the ARITY-ZERO forms' whole-value key (`sort`/`unique`/`min`/ `max` compare elements directly). It carries
/// NO box level: the stack-depth gate pins `[$x,$x] | sort` succeeding at a 10000-deep `$x`, which the virtual box of
/// the `_by` forms would turn into a raise. `Bare` and the `_by` variants never share a run — an entry vector comes
/// from one source or the other — so the mixed arms of [`compare_key`] are unreachable.
pub enum KeyValue {
    /// The key filter produced nothing for this element: the empty array `[]`.
    Empty,
    /// Exactly one output — the common case — stored unboxed; the semantic key is the one-element array `[value]`.
    One(Value),
    /// Two or more outputs: the boxed array.
    Many(Array),
    /// The arity-zero forms' whole-value key: the element itself, no box.
    Bare(Value),
}

/// Compares two `_by` keys, carrying the virtual box level `One` implies.
///
/// `depth` counts containers already entered, exactly as [`super::order::compare`]'s recursion does; the top-level
/// callers pass 0.
/// The `[a]`-versus-`b` mixed arms reproduce [`super::order::compare_arrays`] for a one-element left operand: element
/// compare at `depth + 1`, and a tie resolves to `Less` because `[a]` is the shorter array.
///
/// `law` is the NaN rule the comparison answers under, and the choice is per-CALLER rather than per-module:
/// [`sort_entries`] must run on the INTERNAL total order or Rust's sort sees an inconsistent comparator, while
/// [`run_end`] and [`extreme`] read the OBSERVABLE one, which is what makes `unique` and `group_by` keep two NaNs apart
/// and what inverts the min/max tie law over a NaN key. See [`super::order`]'s module note.
///
/// # Errors
///
/// Returns [`TooDeep`] when a comparison passes the comparison row's ceiling; `Bare` mixed with a `_by` variant cannot
/// occur and answers `Equal` (the local precedent for impossible pairs, see `within_rank`).
pub fn compare_key(left: &KeyValue, right: &KeyValue, depth: usize, law: NanLaw) -> Result<Ordering, TooDeep> {
    match (left, right) {
        (KeyValue::Bare(a), KeyValue::Bare(b)) => super::order::compare(a, b, depth, law),
        (KeyValue::Empty, KeyValue::Empty) => Ok(Ordering::Equal),
        (KeyValue::Empty, _) => Ok(Ordering::Less),
        (_, KeyValue::Empty) => Ok(Ordering::Greater),
        (KeyValue::One(a), KeyValue::One(b)) => super::order::compare(a, b, depth + 1, law),
        (KeyValue::One(a), KeyValue::Many(b)) => compare_one_against_many(a, b, depth, law),
        (KeyValue::Many(a), KeyValue::One(b)) => compare_one_against_many(b, a, depth, law).map(Ordering::reverse),
        (KeyValue::Many(a), KeyValue::Many(b)) => super::order::compare_arrays(a, b, depth, law),
        (KeyValue::Bare(_), _) | (_, KeyValue::Bare(_)) => Ok(Ordering::Equal),
    }
}

/// One streamed group's accumulator: the group's key and the member references filed under it, in arrival (= input)
/// order.
pub struct KeyedGroup {
    /// The group's key.
    pub key: KeyValue,
    /// The members so far, input order (the stability law).
    pub members: Vec<Value>,
}

/// The streaming `group_by` accumulator: per-key groups, no materialized array.
///
/// Files each element under its key AS IT ARRIVES — one sorted insertion per DISTINCT key, O(log G) comparisons per
/// element — so neither the subject array nor the N-entry table the collect-then-publish path needs ever exists. The
/// two ordering laws are [`sort_entries`]/[`run_end`]'s own, one each: the vector is kept ordered by the INTERNAL total
/// order (the sort's placement law) and membership within a total-equal range reads the OBSERVABLE law (the
/// run-boundary law), which makes publishing a straight walk of the vector — every entry IS one published group, in
/// exactly [`Publication::RunsAsGroups`] order. Two NaN keys stay distinct groups in first-seen order because the
/// observable law splits them where the sort's internal law kept them adjacent; `-0` and `0` merge because both laws
/// call them equal.
pub struct KeyedGroups {
    groups: Vec<KeyedGroup>,
}

impl KeyedGroups {
    /// The empty accumulator.
    pub fn new() -> Self {
        Self { groups: Vec::new() }
    }

    /// Whether nothing was filed yet (the empty-input law's witness).
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// Files one element under its key.
    ///
    /// The binary search partitions on the INTERNAL order with an infallible closure; a comparison that runs too deep
    /// inside it only makes the partition undershoot, and the fallible scan below re-raises the same error before
    /// anything is inserted or merged. Insertion lands at the END of the total-equal range, which keeps NaN twins in
    /// first-seen order — the stable sort's own answer for them.
    ///
    /// # Errors
    ///
    /// Returns the comparison-depth error mapped through `resources`, or the allocation class when growing an
    /// accumulator fails.
    pub fn file(
        &mut self,
        key: KeyValue,
        member: Value,
        resources: &ResourceContext<'_>,
    ) -> Result<(), EngineRunError> {
        let mut index = self
            .groups
            .partition_point(|group| matches!(compare_key(&group.key, &key, 0, NanLaw::Total), Ok(Ordering::Less)));
        while let Some(group) = self.groups.get_mut(index) {
            if compare_key(&group.key, &key, 0, NanLaw::Total).map_err(|_| comparison_error(resources))?
                == Ordering::Greater
            {
                break;
            }
            if compare_key(&group.key, &key, 0, NanLaw::Observable).map_err(|_| comparison_error(resources))?
                == Ordering::Equal
            {
                // Same key: append. The vector stays ordered; the member keeps its arrival position (the stability
                // law).
                group
                    .members
                    .try_reserve(1)
                    .map_err(|_| EngineRunError::allocation_failure())?;
                group.members.push(member);
                return Ok(());
            }
            // Total-Equal but observable-different: a NaN twin. Keep scanning — later twins must sit AFTER this one
            // (first-seen order).
            index += 1;
        }
        self.groups
            .try_reserve(1)
            .map_err(|_| EngineRunError::allocation_failure())?;
        let mut members = Vec::new();
        members
            .try_reserve(1)
            .map_err(|_| EngineRunError::allocation_failure())?;
        members.push(member);
        self.groups.insert(index, KeyedGroup { key, members });
        Ok(())
    }

    /// Publishes the groups as the `group_by` answer: one nested array per accumulator, in the accumulator order
    /// (already the published law).
    ///
    /// Takes `self` by value: the sole production caller owns the accumulator and drops it right after this walk, so
    /// each member MOVES into its array instead of paying a clone.
    ///
    /// # Errors
    ///
    /// Returns the allocation class when building the arrays fails.
    pub fn publish(self) -> Result<Value, EngineRunError> {
        let mut out = Array::try_with_capacity(self.groups.len()).map_err(|_| EngineRunError::allocation_failure())?;
        for group in self.groups {
            let mut members =
                Array::try_with_capacity(group.members.len()).map_err(|_| EngineRunError::allocation_failure())?;
            for member in group.members {
                members
                    .try_push(member)
                    .map_err(|_| EngineRunError::allocation_failure())?;
            }
            out.try_push(Value::Array(members))
                .map_err(|_| EngineRunError::allocation_failure())?;
        }
        Ok(Value::Array(out))
    }
}

/// Whether a streamed selection candidate replaces the incumbent — [`extreme`]'s own tie law, one comparison per
/// arrival. `min_by` keeps the incumbent unless the candidate is strictly smaller
/// ([`Publication::FirstOfSmallestRun`]); `max_by` replaces unless the candidate is strictly smaller
/// ([`Publication::LastOfLargestRun`]).
/// OBSERVABLE law, exactly like [`extreme`]: two NaN keys compare Less both ways, so a later NaN replaces an earlier
/// one for `min_by` and never does for `max_by`.
pub fn selection_replaces(larger_wins: bool, candidate: &KeyValue, incumbent: &KeyValue) -> Result<bool, TooDeep> {
    let ordering = compare_key(candidate, incumbent, 0, NanLaw::Observable)?;
    Ok(match ordering {
        Ordering::Less => !larger_wins,
        // Equal replaces only when the larger side wins (max_by's own tie law).
        Ordering::Equal | Ordering::Greater => larger_wins,
    })
}

/// `[value]` against the boxed array `many`, lexicographic then by length.
///
/// `Many` is only ever built on a SECOND output, so `many` holds at least two elements; the `None` arm is a defensive
/// stand-in for that impossible state.
fn compare_one_against_many(value: &Value, many: &Array, depth: usize, law: NanLaw) -> Result<Ordering, TooDeep> {
    let first = many.get(0).ok_or(TooDeep)?;
    match super::order::compare(value, first, depth + 1, law)? {
        Ordering::Equal => Ok(Ordering::Less),
        other => Ok(other),
    }
}

/// Which keyed builtin a collection is being published for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyMode {
    /// `sort` / `sort_by`.
    Sort,
    /// `group_by`.
    Group,
    /// `unique` / `unique_by`.
    Unique,
    /// `min` / `min_by`.
    Min,
    /// `max` / `max_by`.
    Max,
}

/// Which element (or elements) of the sorted table a mode publishes.
///
/// This is the TIE law: every variant says what happens to a run of elements whose keys compare equal, and no two modes
/// share one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Publication {
    /// Every element in key order; a run keeps its input order.
    AllInKeyOrder,
    /// One nested array per run, each run in input order.
    RunsAsGroups,
    /// The FIRST element of each run.
    FirstOfEachRun,
    /// The FIRST element of the SMALLEST-key run.
    FirstOfSmallestRun,
    /// The LAST element of the LARGEST-key run.
    LastOfLargestRun,
}

/// What a mode publishes when the input array is empty.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EmptyLaw {
    /// The empty array.
    EmptyArray,
    /// `null`.
    Null,
}

/// Which of the two DOUBLED rejection templates a mode's `_by` form takes when its input iterates but is not an array
/// (an object, in practice).
///
/// Both templates name the input AND the boxed key array the `map([f])` spelling materializes; they differ only in
/// their tail, and the split is by mode rather than by input — which is why it is a row and not a branch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Rejection {
    /// "… and … cannot be sorted, as they are not both arrays".
    NotBothArrays,
    /// "… and … cannot be iterated over".
    NotIterable,
}

/// One mode's three separating laws.
pub struct KeyModeRow {
    /// The mode this row is the law for.
    pub mode: KeyMode,
    /// Which member of an equal-key run survives.
    pub tie: Publication,
    /// What an empty input publishes.
    pub empty: EmptyLaw,
    /// Which doubled template a non-array input is rejected with.
    pub rejection: Rejection,
}

/// The five keyed modes, closed.
///
/// `Min` and `Max` are NOT symmetric, and that asymmetry is the table's reason to exist: over
/// `[{"a":1,"i":0},{"a":1,"i":1},{"a":2,"i":2},{"a":2,"i":3}]`
/// `min_by(.a)` is `{"a":1,"i":0}` and `max_by(.a)` is `{"a":2,"i":3}`
/// — the FIRST of the smallest run and the LAST of the largest. One comparison predicate shared between them gets
/// exactly one of the two wrong.
pub const KEY_MODES: [KeyModeRow; 5] = [
    KeyModeRow {
        mode: KeyMode::Sort,
        tie: Publication::AllInKeyOrder,
        empty: EmptyLaw::EmptyArray,
        rejection: Rejection::NotBothArrays,
    },
    KeyModeRow {
        mode: KeyMode::Group,
        tie: Publication::RunsAsGroups,
        empty: EmptyLaw::EmptyArray,
        rejection: Rejection::NotBothArrays,
    },
    KeyModeRow {
        mode: KeyMode::Unique,
        tie: Publication::FirstOfEachRun,
        empty: EmptyLaw::EmptyArray,
        rejection: Rejection::NotBothArrays,
    },
    KeyModeRow {
        mode: KeyMode::Min,
        tie: Publication::FirstOfSmallestRun,
        empty: EmptyLaw::Null,
        rejection: Rejection::NotIterable,
    },
    KeyModeRow {
        mode: KeyMode::Max,
        tie: Publication::LastOfLargestRun,
        empty: EmptyLaw::Null,
        rejection: Rejection::NotIterable,
    },
];

/// The row for one mode. Five arms, no default: a new mode is a compile error here until it has been given a row.
pub const fn row(mode: KeyMode) -> &'static KeyModeRow {
    // The fieldless enum's implicit discriminant.
    &KEY_MODES[mode as usize]
}

/// Compile-time proof that the table and the lookup agree: every row sits at the index its mode looks up, so a
/// reordered table cannot silently hand `min_by`
/// `max_by`'s tie law.
const _: () = {
    assert!(matches!(KEY_MODES[0].mode, KeyMode::Sort));
    assert!(matches!(KEY_MODES[1].mode, KeyMode::Group));
    assert!(matches!(KEY_MODES[2].mode, KeyMode::Unique));
    assert!(matches!(KEY_MODES[3].mode, KeyMode::Min));
    assert!(matches!(KEY_MODES[4].mode, KeyMode::Max));
};

/// One entry per element, keyed by the ELEMENT ITSELF.
///
/// This is the arity-0 forms' whole key law: `sort` orders by whole values and `f_min` passes its input as both the
/// values and the keys. It is order-equivalent to the `_by` forms' one-element boxes — `[a]` against `[b]` is `a`
/// against `b` under [`compare_key`] — and it allocates no box. The entries are `Bare`: they carry no virtual box
/// level, which is the law the stack-depth gate pins (`[$x,$x] | sort` succeeds at a 10000-deep `$x`).
///
/// # Errors
///
/// Returns [`EngineRunError::Codec`] (allocation failure) when reserving the table or retaining an element fails.
pub fn try_entries(elements: &Array) -> Result<Vec<KeyedEntry>, EngineRunError> {
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(elements.len())
        .map_err(|_| EngineRunError::allocation_failure())?;
    for (position, element) in elements.iter().enumerate() {
        entries.push(KeyedEntry {
            key: KeyValue::Bare(element.clone()),
            position,
        });
    }
    Ok(entries)
}

/// Sorts `entries` by key under jqf's INTERNAL total order, STABLY.
///
/// Stability is load-bearing, not a preference: it is what makes `sort_by` keep equal-key elements in input order,
/// `group_by` hand back each group in input order, and `unique_by` keep the first element of each run.
///
/// [`NanLaw::Total`] is load-bearing too, and for a different reason: Rust's sort detects inconsistent comparators, and
/// the observable rule answers `Less` for `nan` against `nan` in BOTH argument orders. The placement the sort needs —
/// NaN below every number — is the same under both laws, so nothing observable is lost by sorting under the
/// consistent one.
///
/// The depth cap needs a CAPTURED FLAG rather than a `?`: `sort_by`'s comparator is infallible, so a comparison that
/// runs too deep reports the only ordering it may — `Equal`, which leaves the run where it was — and sets the flag
/// the caller checks before it publishes anything.
pub fn sort_entries(entries: &mut [KeyedEntry]) -> Result<(), TooDeep> {
    // One sorted key order per OBJECT-typed entry, built once: the comparator otherwise re-sorts every object's key
    // list on each of its O(N log N) invocations — K log K key moves and two Vec allocations per comparison. The
    // cache is an insertion-INDEX list, not key copies, so the entry table stays the single key storage.
    // Position-indexed:
    // the sort callers build entries with dense 0..n positions, and a position that misses (defensively) falls back to
    // the plain comparator.
    let cache = sorted_key_order_cache(entries);
    let mut too_deep = false;
    entries.sort_by(|left, right| {
        compare_key_cached(
            &left.key,
            &right.key,
            &cache,
            left.position,
            right.position,
            0,
            NanLaw::Total,
        )
        .unwrap_or_else(|TooDeep| {
            too_deep = true;
            Ordering::Equal
        })
    });
    if too_deep { Err(TooDeep) } else { Ok(()) }
}

/// One sorted insertion-index order per object-typed entry key, or `None` for every other key kind. Sized by the
/// entries' dense positions; an out-of-range position (defensive) leaves its slot `None` and the comparator falls back.
fn sorted_key_order_cache(entries: &[KeyedEntry]) -> Vec<Option<Box<[u32]>>> {
    if !entries.iter().any(|entry| entry_key_object(&entry.key).is_some()) {
        return Vec::new();
    }
    let mut cache = vec![None; entries.len()];
    for entry in entries {
        let Some(object) = entry_key_object(&entry.key) else {
            continue;
        };
        // An object too large for u32-indexed order cannot exist under the resource governor (construction fails long
        // before); the defensive skip keeps the no-panic law and falls back to the plain path.
        let Ok(len) = u32::try_from(object.len()) else {
            continue;
        };
        let mut order: Vec<u32> = (0..len).collect();
        order.sort_unstable_by(|&a, &b| key_at(object, a as usize).cmp(key_at(object, b as usize)));
        if let Some(slot) = cache.get_mut(entry.position) {
            *slot = Some(order.into_boxed_slice());
        }
    }
    cache
}

/// The entry's key when it is a single object, across the two single-value key arms (`Bare` for the arity-zero forms,
/// `One` for a one-output `_by` form).
fn entry_key_object(key: &KeyValue) -> Option<&jqf_data::Object> {
    match key {
        KeyValue::Bare(value) | KeyValue::One(value) => match value.untagged() {
            Value::Object(object) => Some(object),
            _ => None,
        },
        KeyValue::Empty | KeyValue::Many(_) => None,
    }
}

/// The key at an insertion index, or `""` when the index is out of range (unreachable by construction; the empty string
/// only orders a corrupt cache, which the comparator's other arm already treats as a fallback).
fn key_at(object: &jqf_data::Object, index: usize) -> &str {
    object.get_index(index).map_or("", |entry| entry.key())
}

/// [`compare_key`] with the object-key cache: the two single-object arms route through the cached order when both sides
/// have one, everything else keeps the plain law.
fn compare_key_cached(
    left: &KeyValue,
    right: &KeyValue,
    cache: &[Option<Box<[u32]>>],
    left_position: usize,
    right_position: usize,
    depth: usize,
    law: NanLaw,
) -> Result<Ordering, TooDeep> {
    let pair = match (left, right) {
        (KeyValue::Bare(a), KeyValue::Bare(b)) => Some((a, b, depth)),
        (KeyValue::One(a), KeyValue::One(b)) => Some((a, b, depth + 1)),
        _ => None,
    };
    if let Some((a, b, depth)) = pair
        && let (Some(left_order), Some(right_order)) = (
            cache.get(left_position).and_then(|order| order.as_deref()),
            cache.get(right_position).and_then(|order| order.as_deref()),
        )
    {
        return compare_objects_ordered(a, b, left_order, right_order, depth, law);
    }
    compare_key(left, right, depth, law)
}

/// [`order::compare_objects`] with both key orders already known: the key lists compare by the cached insertion-index
/// order (pairwise then length, the slice `cmp` the plain path runs), and the value walk is two direct index lookups
/// per key — no re-sort, no allocation, no key hashing.
fn compare_objects_ordered(
    left: &Value,
    right: &Value,
    left_order: &[u32],
    right_order: &[u32],
    depth: usize,
    law: NanLaw,
) -> Result<Ordering, TooDeep> {
    let (Value::Object(left), Value::Object(right)) = (left.untagged(), right.untagged()) else {
        return super::order::compare(left, right, depth, law);
    };
    for (&li, &ri) in left_order.iter().zip(right_order.iter()) {
        match key_at(left, li as usize).cmp(key_at(right, ri as usize)) {
            Ordering::Equal => {}
            other => return Ok(other),
        }
    }
    match left_order.len().cmp(&right_order.len()) {
        Ordering::Equal => {}
        other => return Ok(other),
    }
    for (&li, &ri) in left_order.iter().zip(right_order.iter()) {
        let (Some(left_value), Some(right_value)) = (
            left.get_index(li as usize).map(jqf_data::ObjectEntry::value),
            right.get_index(ri as usize).map(jqf_data::ObjectEntry::value),
        ) else {
            // Equal key lists guarantee both lookups resolve; a miss keeps the plain law's own arm.
            return Ok(Ordering::Equal);
        };
        match super::order::compare(left_value, right_value, depth + 1, law)? {
            Ordering::Equal => {}
            other => return Ok(other),
        }
    }
    Ok(Ordering::Equal)
}

/// One past the last entry of the equal-key run starting at `start`.
///
/// The caller has already sorted, so an equal-key run is an adjacent range and finding it costs one linear scan rather
/// than a second comparison structure.
pub fn run_end(entries: &[KeyedEntry], start: usize) -> Result<usize, TooDeep> {
    let Some(first) = entries.get(start) else {
        return Ok(start);
    };
    let mut end = start + 1;
    while let Some(entry) = entries.get(end) {
        if compare_key(&first.key, &entry.key, 0, NanLaw::Observable)? != Ordering::Equal {
            break;
        }
        end += 1;
    }
    Ok(end)
}

/// Publishes one mode's answer over `elements`, given their collected keys.
///
/// `entries` arrives in input order and is sorted here when the mode needs it, so no caller can publish an unsorted
/// table by forgetting to sort first. Each entry's `position` indexes `elements`.
///
/// # Errors
///
/// Returns [`EngineRunError::Codec`] (allocation failure) when building the published value fails, or an
/// internal-contract error when an entry names a position that is not in `elements`.
#[allow(
    clippy::too_many_lines,
    reason = "one publication arm per tie law: the five arms are the mode table's \
              observable halves, and splitting them would hide which law each \
              builtin answers with"
)]
pub fn publish(
    mode: KeyMode,
    entries: &mut [KeyedEntry],
    elements: &Array,
    spill_runs: &[jqf_resource::RunId],
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let row = row(mode);
    // The spilled runs are entries too: when the keyed frame flushed every child, `entries` is empty but the merge
    // still has the full collection.
    if entries.is_empty() && spill_runs.is_empty() {
        return match row.empty {
            EmptyLaw::EmptyArray => Ok(Value::Array(
                Array::try_new().map_err(|_| EngineRunError::allocation_failure())?,
            )),
            EmptyLaw::Null => Ok(Value::Null),
        };
    }
    match row.tie {
        // The two SELECTION modes read one extreme and never sort: `min_by`/ `max_by` scan, and answering an O(n)
        // question with an O(n log n) sort would diverge from the observable cost.
        Publication::FirstOfSmallestRun => {
            // Spills are SORT-only (the precondition lives in the frame's callers); runs here would hide entries from
            // the extreme scan.
            if !spill_runs.is_empty() {
                return Err(EngineRunError::internal_contract(
                    "a keyed selection published over spilled runs",
                ));
            }
            let best = extreme(entries, Extreme::First).map_err(|_| comparison_error(resources))?;
            element_at(elements, best)
        }
        Publication::LastOfLargestRun => {
            if !spill_runs.is_empty() {
                return Err(EngineRunError::internal_contract(
                    "a keyed selection published over spilled runs",
                ));
            }
            let best = extreme(entries, Extreme::Last).map_err(|_| comparison_error(resources))?;
            element_at(elements, best)
        }
        Publication::AllInKeyOrder => {
            // The external-sort spill: when the keyed collection flushed runs (the frame's budget check), the merged
            // POSITION sequence replaces the in-memory sort — the same stable law, the same positions, the same
            // bytes.
            // The merge's total is the ELEMENT count (an array subject's entry count is its length), which the merge
            // only needs for its output reservation.
            // The tail-flush recovery keeps the halves disjoint: a declined final flush folds the runs back into
            // `entries` and clears the runs, so at most one of the two is non-empty here — the merge below would
            // silently DROP in-memory entries alongside runs.
            debug_assert!(
                entries.is_empty() || spill_runs.is_empty(),
                "keyed publish saw spilled runs AND in-memory entries"
            );
            if !spill_runs.is_empty() {
                let total = elements.len().max(entries.len());
                let positions = super::spill::merge_run_positions(
                    resources
                        .spill_store()
                        .ok_or_else(|| EngineRunError::internal_contract("spill store vanished"))?,
                    spill_runs,
                    total,
                    resources,
                )?;
                let mut out =
                    Array::try_with_capacity(positions.len()).map_err(|_| EngineRunError::allocation_failure())?;
                for position in positions {
                    let element = element_at(elements, position)?;
                    out.try_push(element)
                        .map_err(|_| EngineRunError::allocation_failure())?;
                }
                return Ok(Value::Array(out));
            }
            sort_entries(entries).map_err(|_| comparison_error(resources))?;
            let mut out = Array::try_with_capacity(entries.len()).map_err(|_| EngineRunError::allocation_failure())?;
            for entry in entries {
                let element = element_at(elements, entry.position)?;
                out.try_push(element)
                    .map_err(|_| EngineRunError::allocation_failure())?;
            }
            Ok(Value::Array(out))
        }
        Publication::FirstOfEachRun => {
            // Spills are SORT-only (the precondition lives in the frame's callers); this publication consumes the
            // sorted ENTRIES, so runs here would silently publish an empty answer.
            if !spill_runs.is_empty() {
                return Err(EngineRunError::internal_contract(
                    "a keyed run-detection published over spilled runs",
                ));
            }
            sort_entries(entries).map_err(|_| comparison_error(resources))?;
            let mut out = Array::try_new().map_err(|_| EngineRunError::allocation_failure())?;
            let mut start = 0;
            while start < entries.len() {
                let element = element_at(elements, entries[start].position)?;
                out.try_push(element)
                    .map_err(|_| EngineRunError::allocation_failure())?;
                start = run_end(entries, start).map_err(|_| comparison_error(resources))?;
            }
            Ok(Value::Array(out))
        }
        Publication::RunsAsGroups => {
            if !spill_runs.is_empty() {
                return Err(EngineRunError::internal_contract(
                    "a keyed run-detection published over spilled runs",
                ));
            }
            sort_entries(entries).map_err(|_| comparison_error(resources))?;
            let mut out = Array::try_new().map_err(|_| EngineRunError::allocation_failure())?;
            let mut start = 0;
            while start < entries.len() {
                let end = run_end(entries, start).map_err(|_| comparison_error(resources))?;
                let mut group =
                    Array::try_with_capacity(end - start).map_err(|_| EngineRunError::allocation_failure())?;
                for entry in &entries[start..end] {
                    let element = element_at(elements, entry.position)?;
                    group
                        .try_push(element)
                        .map_err(|_| EngineRunError::allocation_failure())?;
                }
                out.try_push(Value::Array(group))
                    .map_err(|_| EngineRunError::allocation_failure())?;
                start = end;
            }
            Ok(Value::Array(out))
        }
    }
}

/// Which end of an equal-key extreme run a selection mode keeps.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Extreme {
    /// Keep the incumbent on a tie — the first element of the smallest run.
    First,
    /// Replace the incumbent on a tie — the last element of the largest run.
    Last,
}

/// The position of the extreme-key entry, one linear scan, no sort.
///
/// The whole `min`/`max` asymmetry is the one comparison below: `First` keeps the incumbent when the keys are equal,
/// `Last` replaces it.
///
/// A linear scan needs no consistent comparator, so this reads the OBSERVABLE law: `min`/`max` are equality consumers
/// whose answers turn on precisely the NaN-against-NaN leaf, which the internal order would wrongly call a tie.
fn extreme(entries: &[KeyedEntry], end: Extreme) -> Result<usize, TooDeep> {
    let mut best = 0;
    for index in 1..entries.len() {
        let ordering = compare_key(&entries[index].key, &entries[best].key, 0, NanLaw::Observable)?;
        let replace = match end {
            Extreme::First => ordering == Ordering::Less,
            Extreme::Last => ordering != Ordering::Less,
        };
        if replace {
            best = index;
        }
    }
    Ok(entries[best].position)
}

/// Duplicates the element at `position`, or reports the broken contract.
///
/// The duplication is [`Value::clone`], which RETAINS rather than copies, so publishing a group of a thousand objects
/// costs a thousand refcount bumps and no element bytes.
fn element_at(elements: &Array, position: usize) -> Result<Value, EngineRunError> {
    Ok(elements
        .get(position)
        .ok_or_else(|| EngineRunError::internal_contract("a keyed entry naming a position with no element"))?
        .clone())
}

#[cfg(test)]
mod tests {
    use super::{KeyValue, KeyedEntry, KeyedGroups, selection_replaces, sort_entries, sorted_key_order_cache};
    use alloc::vec;
    use alloc::vec::Vec;
    use jqf_data::{Number, ObjectBuilder, Value};

    fn number(spelling: &str) -> Value {
        Value::Number(Number::try_json_literal(spelling).expect("literal"))
    }

    fn object(entries: &[(&str, Value)]) -> Value {
        let mut builder = ObjectBuilder::try_with_capacity(entries.len()).expect("builder");
        for (key, value) in entries {
            builder
                .try_insert_last(jqf_data::ObjectKey::try_from_str(key).expect("key"), value.clone())
                .expect("insert");
        }
        Value::Object(builder.try_finish().expect("object"))
    }

    fn nan() -> Value {
        Value::Number(Number::float(jqf_data::Float::new(f64::NAN)))
    }

    /// Files `(key-spelling, tag)` pairs in order and returns the published group tags — the streaming accumulator's
    /// whole observable surface.
    fn filed_groups(rows: &[(&str, &str)], nan_keys: &[usize]) -> Vec<Vec<&'static str>> {
        let mut groups = KeyedGroups::new();
        let resources = test_resources();
        for (index, (key_spelling, tag)) in rows.iter().enumerate() {
            let key = if nan_keys.contains(&index) {
                KeyValue::One(nan())
            } else {
                KeyValue::One(number(key_spelling))
            };
            let member = Value::try_string(tag).expect("member");
            groups.file(key, member, &resources).expect("file");
        }
        match groups.publish().expect("publish") {
            Value::Array(groups) => groups
                .iter()
                .map(|group| match group.untagged() {
                    Value::Array(members) => members
                        .iter()
                        .map(|member| match member.untagged() {
                            Value::String(text) => text_tag(text),
                            other => panic!("unexpected member {other:?}"),
                        })
                        .collect(),
                    other => panic!("unexpected group {other:?}"),
                })
                .collect(),
            other => panic!("unexpected publish {other:?}"),
        }
    }

    fn text_tag(text: &str) -> &'static str {
        for candidate in ["a", "b", "c", "d"] {
            if text == candidate {
                return candidate;
            }
        }
        panic!("unexpected member text {text:?}")
    }

    fn test_resources() -> jqf_resource::ResourceContext<'static> {
        static CONTROL: jqf_resource::ContinueControl = jqf_resource::ContinueControl;
        jqf_resource::ResourceContext::new(
            jqf_resource::RequestAccount::try_new(jqf_resource::ResourceLimits::new(
                u64::MAX,
                u64::MAX,
                u64::MAX,
                u64::MAX,
                u32::MAX,
            ))
            .expect("account"),
            &CONTROL,
            jqf_resource::WorkMeter::try_new_v1(4096).expect("work"),
        )
        .expect("resources")
    }

    #[test]
    fn keyed_groups_publish_in_key_order_with_members_in_input_order() {
        let groups = filed_groups(&[("2", "a"), ("1", "b"), ("2", "c"), ("1", "d")], &[]);
        assert_eq!(groups, vec![vec!["b", "d"], vec!["a", "c"]]);
    }

    #[test]
    fn keyed_groups_keep_two_nans_apart_in_first_seen_order_below_numbers() {
        // NaN sorts below every number and two NaNs stay distinct groups.
        let groups = filed_groups(&[("5", "a"), ("ignored", "b"), ("3", "c"), ("ignored2", "d")], &[1, 3]);
        assert_eq!(groups, vec![vec!["b"], vec!["d"], vec!["c"], vec!["a"]]);
    }

    #[test]
    fn keyed_groups_merge_negative_and_positive_zero() {
        // Both laws call -0 and 0 equal: one group, input order. The exact decimal `-0` spells through the
        // retained-zero literal path.
        let mut groups = KeyedGroups::new();
        let resources = test_resources();
        for spelling in ["0", "-0"] {
            groups
                .file(
                    KeyValue::One(Number::try_json_literal(spelling).map(Value::Number).expect("zero")),
                    Value::Null,
                    &resources,
                )
                .expect("file");
        }
        assert_eq!(groups_len(groups), 1);
    }

    fn groups_len(groups: KeyedGroups) -> usize {
        match groups.publish().expect("publish") {
            Value::Array(published) => published.len(),
            other => panic!("unexpected publish {other:?}"),
        }
    }

    #[test]
    fn empty_accumulator_publishes_the_empty_array() {
        let groups = KeyedGroups::new();
        assert!(groups.is_empty());
        assert!(matches!(groups.publish().expect("publish"), Value::Array(published) if published.is_empty()));
    }

    #[test]
    fn selection_replaces_reproduces_the_extreme_tie_laws() {
        let one = KeyValue::One(number("1"));
        let two = KeyValue::One(number("2"));
        // min_by keeps the incumbent on a tie and on a larger candidate.
        assert!(!selection_replaces(false, &one, &one).expect("compare"));
        assert!(!selection_replaces(false, &two, &one).expect("compare"));
        assert!(selection_replaces(false, &one, &two).expect("compare"));
        // max_by replaces on a tie and on a larger candidate.
        assert!(selection_replaces(true, &two, &two).expect("compare"));
        assert!(selection_replaces(true, &two, &one).expect("compare"));
        assert!(!selection_replaces(true, &one, &two).expect("compare"));
        // Two NaNs compare Less both ways under the observable law: a later NaN replaces an earlier one for min_by,
        // never for max_by.
        let first_nan = KeyValue::One(nan());
        let second_nan = KeyValue::One(nan());
        assert!(selection_replaces(false, &second_nan, &first_nan).expect("compare"));
        assert!(!selection_replaces(true, &second_nan, &first_nan).expect("compare"));
    }

    #[test]
    fn object_key_order_cache_is_empty_when_no_entry_is_an_object() {
        let entries = [
            KeyedEntry {
                key: KeyValue::Bare(number("2")),
                position: 0,
            },
            KeyedEntry {
                key: KeyValue::Bare(number("1")),
                position: 1,
            },
        ];
        assert!(sorted_key_order_cache(&entries).is_empty());
    }

    #[test]
    fn object_key_order_cache_fills_object_positions() {
        let entries = [
            KeyedEntry {
                key: KeyValue::Bare(object(&[("b", number("1")), ("a", number("2"))])),
                position: 0,
            },
            KeyedEntry {
                key: KeyValue::Bare(number("1")),
                position: 1,
            },
        ];
        let cache = sorted_key_order_cache(&entries);
        assert_eq!(cache.len(), 2);
        assert!(cache[0].is_some());
        assert!(cache[1].is_none());
    }

    #[test]
    fn sort_entries_on_numbers_is_stable() {
        let mut entries = [
            KeyedEntry {
                key: KeyValue::Bare(number("2")),
                position: 0,
            },
            KeyedEntry {
                key: KeyValue::Bare(number("1")),
                position: 1,
            },
            KeyedEntry {
                key: KeyValue::Bare(number("1")),
                position: 2,
            },
        ];
        sort_entries(&mut entries).expect("sort");
        assert_eq!(entries[0].position, 1);
        assert_eq!(entries[1].position, 2);
        assert_eq!(entries[2].position, 0);
    }
}
