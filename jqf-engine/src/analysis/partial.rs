//! The PARTIAL-SORT table: which `sort`/`sort_by` calls whose sole consumer is
//! a provably-constant head/tail slice (or a single-element index, or a
//! `reverse` hop) the executor may answer with the bounded-heap `top_k` partial
//! sort instead of a full sort.
//!
//! The [`PartialSort`] row is an arena fact — `Program` stores the table
//! (`topk: Vec<PartialSort>`) — so its home is beside the arena; the sibling
//! [`super::topk`] keeps the executor-facing `topk_count` reading and imports
//! the row from here.

#[cfg(test)]
use alloc::vec::Vec;

use jqf_data::Value;

use crate::program::{ProgramNode, ProgramNodeId, SliceBound, StageStart, StepAccess};
use jqf_builtins::registry::builtins::id;
use jqf_builtins::registry::builtins::top_k::TopKDirection;

/// One recognized partial-sort row, addressed entirely by arena node id.
///
/// Every field is a node the executor re-reads from the arena when it runs
/// (the sort call's overload and `by` graph, the slice stage's authored
/// bounds), so the table carries no copy of the program and costs five `u32`s
/// per row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PartialSort {
    /// The `FlatMap` whose body is the sort output's consumer shape — the
    /// `FlatMap{Call(SORT|SORT_BY), Stage[Slice]}` the executor intercepts, or
    /// its `FlatMap{FlatMap{Call(SORT_BY), Call(REVERSE)}, Stage[Slice]}` twin.
    flat_map: ProgramNodeId,
    /// The upstream `Call` node — the sort the partial sort replaces. For the
    /// reversed row it is the `SORT_BY` call nested one `FlatMap` deeper.
    sort_call: ProgramNodeId,
    /// The body `Stage` node holding the single consumer step (a slice or an
    /// index).
    slice_stage: ProgramNodeId,
    /// The shape of the sort output's sole consumer — the row's second
    /// dimension beside the sort call itself.
    consumer: TopKConsumer,
}

/// The shape of the sorted output's SOLE consumer — what the executor must
/// publish from the bounded-heap result.
///
/// Each variant names the full semantics: which end of the sorted order the
/// elements come from, and the emission order of the published array.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TopKConsumer {
    /// `.[0:k]` — a head slice: the k SMALLEST, published ascending.
    HeadSlice,
    /// `.[-k:]` — a tail slice: the k LARGEST, published ascending (the tail
    /// tie law: among equal keys the LATER source elements).
    TailSlice,
    /// `sort_by(f) | reverse | .[0:k]` — a head slice over the REVERSED sort:
    /// the k LARGEST, published DESCENDING (the reversed sort's tie order —
    /// among equal keys the LATER source elements come FIRST).
    ReversedHeadSlice,
    /// `.[0]` (and the prelude `first`, which lowers to it) — the single
    /// SMALLEST element, or `null` over an empty array. Also the reversed
    /// spelling `sort | reverse | .[-1]`: position -1 of the reversed array
    /// is the same element (smallest, earliest source index among ties).
    FirstIndex,
    /// `.[-1]` (and the prelude `last`, which lowers to it) — the single
    /// LARGEST element, or `null` over an empty array. Also the reversed
    /// spelling `sort | reverse | .[0]`: position 0 of the reversed array
    /// is the same element (largest, later source index among ties).
    LastIndex,
}

impl TopKConsumer {
    /// The heap direction: which end of the sorted order the kept elements
    /// come from. The tail, reversed-head, and last-index rows keep the
    /// LARGEST (the tail admission law — later source indices among equal
    /// keys); the others the SMALLEST.
    pub const fn direction(self) -> TopKDirection {
        match self {
            TopKConsumer::HeadSlice | TopKConsumer::FirstIndex => TopKDirection::Smallest,
            TopKConsumer::TailSlice | TopKConsumer::ReversedHeadSlice | TopKConsumer::LastIndex => {
                TopKDirection::Largest
            }
        }
    }
}

impl PartialSort {
    /// The `FlatMap` node the executor intercepts.
    pub const fn flat_map(self) -> ProgramNodeId {
        self.flat_map
    }

    /// The sort `Call` the partial sort replaces (the `SORT_BY` call nested one
    /// `FlatMap` deeper for the reversed row).
    pub const fn sort_call(self) -> ProgramNodeId {
        self.sort_call
    }

    /// The body `Stage` whose single consumer step is the sort output's sole
    /// consumer.
    pub const fn slice_stage(self) -> ProgramNodeId {
        self.slice_stage
    }

    /// Which end of the sorted order the elements come from.
    pub const fn direction(self) -> TopKDirection {
        self.consumer.direction()
    }

    /// The shape of the sort output's sole consumer.
    pub const fn consumer(self) -> TopKConsumer {
        self.consumer
    }
}

/// Every partial-sort row in `nodes`, in arena order.
///
/// An allocation failure yields an EMPTY table rather than an error: the table
/// is a pure optimization, and a program the full sort can run must never fail
/// because the optimizer could not allocate.
#[cfg(test)]
pub fn partial_sorts(nodes: &[ProgramNode]) -> Vec<PartialSort> {
    let mut found = Vec::new();
    for index in 0..nodes.len() {
        let Some(id) = ProgramNodeId::from_index(index) else {
            break;
        };
        let Some(row) = partial_sort_row(nodes, id) else {
            continue;
        };
        if found.try_reserve(1).is_err() {
            return Vec::new();
        }
        found.push(row);
    }
    found
}

/// Row T1: `FlatMap{ Call(SORT|SORT_BY), Stage{Current, [Slice]} }` with a
/// provably-constant head/tail slice, plus the reversed-sort twin
/// `FlatMap{ Call(SORT_BY), FlatMap{Call(REVERSE), Stage{Current, [Slice]}} }`
/// (pipes lower RIGHT-nested, so the sort call is the outer upstream and the
/// reverse hop is the inner flat map's upstream) and the
/// `Index(0)`/`Index(-1)` consumers.
#[allow(
    clippy::match_same_arms,
    reason = "the paired index arms are DIFFERENT spellings mapping to one shared consumer \
              contract (`sort | .[-1]` and `sort | reverse | .[0]` both answer the largest \
              element with the tail tie law); merging them would erase which spelling each \
              row names"
)]
#[cfg_attr(
    not(debug_assertions),
    allow(unused_variables, reason = "the revision pin is a debug-only assert")
)]
pub(crate) fn partial_sort_row(nodes: &[ProgramNode], id: ProgramNodeId) -> Option<PartialSort> {
    let ProgramNode::FlatMap { upstream, body } = &nodes[id.index()] else {
        return None;
    };
    // The reversed row's body is itself `FlatMap{Call(REVERSE), Stage[Slice]}`:
    // the REVERSE is the sort's sole consumer and the head slice is the
    // REVERSE's sole consumer. The row's `sort_call` is the outer upstream.
    let (sort_call, reversed, slice_stage) = match (&nodes[upstream.index()], &nodes[body.index()]) {
        (ProgramNode::Call { .. }, ProgramNode::Stage { .. }) => (*upstream, false, *body),
        (
            ProgramNode::Call { .. },
            ProgramNode::FlatMap {
                upstream: reverse_call,
                body: inner_body,
            },
        ) => {
            let ProgramNode::Call { overload, .. } = &nodes[reverse_call.index()] else {
                return None;
            };
            if overload.get() != id::REVERSE {
                return None;
            }
            (*upstream, true, *inner_body)
        }
        _ => return None,
    };
    let ProgramNode::Call { overload, revision, .. } = &nodes[sort_call.index()] else {
        return None;
    };
    let sort_overload = overload.get();
    if sort_overload != id::SORT && sort_overload != id::SORT_BY {
        return None;
    }
    // The rows' assumptions — the stable-sort tie law, the `by`-projection
    // fold, the shared not-sortable raise — are pinned to ONE semantic
    // revision of the matched overload. The match itself stays ID-ONLY
    // (house-consistent with every other recognizer), but a registry bump of
    // sort/sort_by semantics under a new revision must not leave these rows
    // firing on stale assumptions until a differential divergence catches
    // it: the coupling is asserted, explicitly, in debug builds.
    #[cfg(debug_assertions)]
    {
        let record = if sort_overload == id::SORT {
            jqf_builtins::registry::resolve_builtin("sort", 0)
        } else {
            jqf_builtins::registry::resolve_builtin("sort_by", 1)
        }
        .expect("the recognizer matched an unregistered overload");
        assert_eq!(
            *revision, record.semantic_revision,
            "sort/sort_by semantics moved under the partial-sort recognizer"
        );
    }
    let ProgramNode::Stage {
        start: StageStart::Current,
        steps,
    } = &nodes[slice_stage.index()]
    else {
        return None;
    };
    // The consumer must be the sort output's ONLY consumer: exactly one step,
    // a slice or an index. A residual after it (`.a[0:3]`, `.[0:3].a`, a second
    // slice), a `Choice`/`Bind`/collect body, or a non-`Current` start all
    // decline.
    let [consumer_step] = steps.as_slice() else {
        return None;
    };
    match consumer_step.access() {
        StepAccess::Slice(bounds) => {
            // The reversed row is a HEAD slice `.[0:k]` only: a tail slice over
            // a reversed sort (`.reverse | .[-k:]`) is the k SMALLEST, which
            // the plain head slice already serves with one fewer hop.
            let consumer = match (reversed, head_or_tail(&bounds.start, &bounds.end)) {
                (false, Some(TopKDirection::Smallest)) => TopKConsumer::HeadSlice,
                (false, Some(TopKDirection::Largest)) => TopKConsumer::TailSlice,
                (true, Some(TopKDirection::Smallest)) => TopKConsumer::ReversedHeadSlice,
                _ => return None,
            };
            Some(PartialSort {
                flat_map: id,
                sort_call,
                slice_stage,
                consumer,
            })
        }
        StepAccess::Index(index) => {
            // The single-element read is the plain whole-value sort's only —
            // direct (`sort | .[0]`, the prelude `first`) or through the one
            // reverse hop (`sort | reverse | .[0]`). A `sort_by` index
            // consumer (direct OR reversed), an out-of-range index, and a
            // non-plain-sort upstream all decline.
            if sort_overload != id::SORT {
                return None;
            }
            let consumer = match (reversed, index) {
                (false, 0) => TopKConsumer::FirstIndex,
                (false, -1) => TopKConsumer::LastIndex,
                // The REVERSED spellings reuse the same two consumers,
                // because `reverse` of a stable ascending sort flips position
                // 0 with position -1 EXACTLY as it flips the slice ends:
                // position 0 of the reversed array is the LARGEST element
                // with the tail tie law (later source index among equal
                // keys) — the LastIndex consumer's own contract — and
                // position -1 of the reversed array is the SMALLEST with the
                // head tie law (earlier source index) — FirstIndex's. Over
                // an empty array both spellings answer `null` (the index
                // step's answer, which `shape_partial_result` reproduces),
                // and over a non-array the plain-sort side condition takes
                // the partial path and raises sort's own not-sortable
                // message, byte for byte.
                (true, 0) => TopKConsumer::LastIndex,
                (true, -1) => TopKConsumer::FirstIndex,
                _ => return None,
            };
            Some(PartialSort {
                flat_map: id,
                sort_call,
                slice_stage,
                consumer,
            })
        }
        _ => None,
    }
}

/// Reads one authored slice bound pair as a top-k direction, or `None` when
/// the pair is not a provably-constant head/tail slice.
///
/// HEAD `.[0:k]`: the start resolves to position 0 for every container length
/// (omitted, `null`, or a non-negative literal that floors to zero), and the
/// end is a literal number whose resolved count is at least one. TAIL
/// `.[-k:]`: the end is omitted/`null` and the start is a strictly negative
/// literal, whose tail length `ceil(-start)` is the count. Every other pair —
/// a bound variable, a non-number literal, an end in the head that is negative
/// (len-relative), a start in the tail that is not negative — is runtime-input-
/// dependent or not a bounded head/tail, and DECLINES.
fn head_or_tail(start: &SliceBound, end: &SliceBound) -> Option<TopKDirection> {
    if head_start(start) && head_end(end) {
        return Some(TopKDirection::Smallest);
    }
    if tail_start(start) && open_end(end) {
        return Some(TopKDirection::Largest);
    }
    None
}

/// The start bound of a HEAD slice: omitted, `null`, or a literal number in
/// `[0, 1)` — the values whose slice resolution is position 0 for every
/// container length (`floor(v)` with `v >= 0` and `v < 1`).
fn head_start(bound: &SliceBound) -> bool {
    match bound {
        SliceBound::Open | SliceBound::Literal(Value::Null) => true,
        SliceBound::Literal(Value::Number(number)) => {
            let value = jqf_builtins::semantics::order::to_f64(number);
            (0.0..1.0).contains(&value)
        }
        SliceBound::Literal(_) | SliceBound::Var(_) => false,
    }
}

/// The end bound of a HEAD slice: a literal number `v > 0`, whose resolved
/// end `ceil(v)` is a constant `k >= 1` for every container length. A
/// non-positive end (`0`, `-0.0`) is an empty head and a strictly negative one
/// is len-relative; a `NaN` end behaves as `Open` (the whole array). All
/// decline.
fn head_end(bound: &SliceBound) -> bool {
    let SliceBound::Literal(Value::Number(number)) = bound else {
        return false;
    };
    let value = jqf_builtins::semantics::order::to_f64(number);
    // `NaN > 0.0` is already `false`, so the `!is_nan()` conjunct was dead.
    value > 0.0
}

/// The start bound of a TAIL slice: a literal strictly negative number `-k`,
/// whose tail length `len - floor(len + v)` is `ceil(-v)` — a constant `k >= 1`
/// for every `len >= k`, clamped to the whole array below it. A non-negative
/// start (`.[3:]`, `.[:]`, `.[0:]`) is a middle/whole slice, not a tail.
fn tail_start(bound: &SliceBound) -> bool {
    let SliceBound::Literal(Value::Number(number)) = bound else {
        return false;
    };
    jqf_builtins::semantics::order::to_f64(number) < 0.0
}

/// The end bound of a TAIL slice: omitted or `null`, which behaves as the
/// container's own end at runtime.
fn open_end(bound: &SliceBound) -> bool {
    matches!(bound, SliceBound::Open | SliceBound::Literal(Value::Null))
}
