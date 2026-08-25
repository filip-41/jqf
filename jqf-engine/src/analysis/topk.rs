//! The PARTIAL-SORT table — which `sort`/`sort_by(f)` calls may be answered
//! with the bounded-heap partial sort (`top_k`) instead of a full sort.
//!
//! `sort | .[0:k]` is O(n log n): the top-k spelling fully sorts the array to
//! answer a k-element question. The builtin door shipped `top_k`
//! (O(n log k) with a bounded heap, byte-identical to `sort_by(f) | .[0:n]`);
//! this table is the RECOGNIZER door — it names the graph shapes where the
//! executor may substitute the partial sort for the full one without changing
//! a single emitted byte.
//!
//! Like [`join`](super::join) and [`count`](super::count) this is a CLOSED
//! table with a declining default: a shape that is not a row below is not a
//! top-k, full stop. A new shape joins by adding a row and its soundness
//! argument, never by widening a default.
//!
//! # The rows
//!
//! | shape | example | why it is the k-smallest/largest |
//! | --- | --- | --- |
//! | `FlatMap{ Call(SORT \| SORT_BY), Stage{Current, [Slice]} }` | `sort_by(.x) \| .[0:10]`, `sort \| .[-5:]` | the sort's output is the slice's ONLY input, the slice is a constant-bound head/tail, and the slice is the sort's SOLE consumer (the sort call is the `FlatMap`'s upstream, and the arena is a tree) |
//! | `FlatMap{ Call(SORT \| SORT_BY), FlatMap{Call(REVERSE), Stage{Current, [Slice]}} }` | `sort_by(.x) \| reverse \| .[0:10]`, `sort \| reverse \| .[0:10]` | the REVERSE is the sort's sole consumer and the head slice is the REVERSE's sole consumer: the k-LARGEST with the REVERSED emission order (descending, ties in reversed source order) |
//! | `FlatMap{ Call(SORT), Stage{Current, [Index(0)]} }` | `sort \| .[0]`, `sort \| first` | a k=1 partial sort publishing the single smallest element (`null` over an empty array) |
//! | `FlatMap{ Call(SORT), Stage{Current, [Index(-1)]} }` | `sort \| .[-1]`, `sort \| last` | the same, publishing the single LARGEST element |
//! | `FlatMap{ Call(SORT), FlatMap{Call(REVERSE), Stage{Current, [Index(0) \| Index(-1)]}} }` | `sort \| reverse \| .[0]`, `sort \| reverse \| .[-1]` | the reversed spellings of the two index rows: `reverse` of a stable ascending sort flips position 0 with position -1 exactly as it flips the slice ends, so position 0 of the reversed array IS `.[-1]` of the sorted one (largest, later-index ties) and position -1 IS `.[0]` (smallest, earlier-index ties). PLAIN `sort` only — a `sort_by` index consumer stays a recorded non-row below, and admitting it through the reverse hop would contradict that non-row's rationale |
//!
//! The head row `.[0:k]` (start `Open`/`null`/a literal in `[0, 1)`, end a
//! literal `k > 0`) routes to the k-SMALLEST partial sort; the tail row
//! `.[-k:]` (start a literal `-k < 0`, end `Open`/`null`) routes to the
//! k-LARGEST; the reversed row is a head slice whose elements come from the
//! LARGEST end and publish DESCENDING. All reproduce `sort | .[s:e]` bytes
//! exactly:
//!
//! 1. **The slice bound law is the `top_k` count law, by construction.** The
//!    executor hands `top_k`'s `resolve_count` the AUTHORED literal: a head
//!    slice's end bound (which `resolve_slice_bound` ceils, and `resolve_count`
//!    ceils the same way) or a tail slice's NEGATED start bound (the tail
//!    length `len - floor(len + s)` is `ceil(-s)` for every `len >= k`, and
//!    `resolve_count` ceils `-s`). A fractional bound (`.[0:2.7]`, `.[-3.7:]`)
//!    therefore answers exactly what the slice answers. The negative-count
//!    departures can never fire: this table admits only bounds whose resolved
//!    count is `>= 1`.
//! 2. **The clamp law is shared.** The slice clamps to the container's edges
//!    and `top_k` clamps `k` to the array's length, so `k >= len` answers the
//!    whole sorted array on both paths, and the empty array answers `[]` on
//!    both.
//! 3. **The tie law is shared.** The full sort is stable (equal keys keep
//!    source order); the `top_k` heap keeps exactly the elements the slice
//!    would — earliest indices among equal keys for the head, latest for the
//!    tail — and publishes them ascending, which is the slice's order.
//! 4. **The `by` projection is the same fold.** `sort_by(f)` folds `f`'s
//!    outputs per element (zero → `null`, one → the value, many → an array),
//!    and `top_k(n; f)` applies the same fold to the same per-element outputs
//!    evaluated in the same order, so an element whose key raises raises at
//!    the same point on both paths.
//! 5. **The reversed row flips only the emission.** `reverse` of a stable sort
//!    is the same permutation reversed: its first `k` elements are the tail
//!    slice's `k` elements — the k-LARGEST, ties keeping LATER source
//!    indices — so the LARGEST heap keeps the same set, and publishing it in
//!    REVERSE order reproduces the reversed sort's descending order with ties
//!    in reversed source order, byte for byte.
//! 6. **The index rows are k=1.** `.[0]` of the sorted array is its smallest
//!    element and `.[-1]` its largest, so the k=1 heap publishes the same
//!    single element; over an EMPTY array the k=1 heap is empty and the row
//!    publishes `null`, exactly what the index step answers, and over a
//!    non-array the not-sortable raise is the sort's own (the plain `sort`
//!    row's side condition takes the partial-sort path for every input, and
//!    `top_k` raises the same message `sort` does).
//! 7. **The reversed index rows flip only the position.** `reverse` of a
//!    stable ascending sort is the same permutation reversed, so its first
//!    element is the sorted array's LAST (the largest, ties keeping LATER
//!    source indices — the tail law) and its last element is the sorted
//!    array's FIRST (the smallest, ties keeping EARLIER source indices — the
//!    head law). Each reversed spelling therefore reuses the direct row's
//!    consumer contract wholesale: same k=1 count, same direction, same
//!    tie law, same empty-array `null`, same non-array raise.
//!
//! A `?` on the slice step is admitted with a soundness argument: the sort's
//! output is an ARRAY or a raise, and an array slice never fails, so the
//! optional flag is unobservable after a successful sort — `sort | .[0:3]?`
//! and `sort | .[0:3]` are the same program.
//!
//! # The run-time side condition (the object arm)
//!
//! The `sort_by` row is eligible only PROVISIONALLY, over ARRAY inputs. On an
//! OBJECT input `sort_by` (spelled `map([f])` then the both-arrays test)
//! collects a key per VALUE and fails with the two-arrays message
//! (`object (…) and array ([[…]]) cannot be sorted`), while `top_k`'s
//! not-sortable raise would be a different message. The executor checks the
//! input's kind before the partial sort: an ARRAY takes the partial-sort path,
//! anything else falls back to the FLOOR (the ordinary `sort_by` evaluation),
//! which reproduces the object arm byte for byte. The plain `sort` row needs
//! no fallback: `sort` rejects a non-array with the same not-sortable message
//! `top_k` raises.
//!
//! # Deliberate non-rows
//!
//! - **A slice on a NON-sort source** (`map(f) | .[0:3]`, `.a[0:3]`): the row
//!   matches only `Call(SORT|SORT_BY)` upstreams (directly, or through the one
//!   `reverse` hop).
//! - **A NON-constant bound** (`.[0:$k]`, `.[0:null]`, `.[0:]`, `.[3:]`,
//!   `.[0:-2]`): a bound variable is runtime-input-dependent, an omitted or
//!   null end is the whole array, a positive start is a middle slice, and a
//!   negative end is len-relative ("all but the last k"), which is not a top-k
//!   of anything. Later constant-folding may widen the provable set; this row
//!   recognizes literals as authored and nothing else.
//! - **A consumer that is not the sole one** (`sort | .[0:3] | .a`, a
//!   `Choice`/`Bind`/collect body): the body must be exactly the one slice or
//!   index step, and the sort call must be the `FlatMap`'s upstream directly
//!   (or through the one `reverse` hop).
//! - **A tail slice over a reversed sort** (`sort_by(.x) | reverse | .[-k:]`):
//!   that is the k SMALLEST, which the plain head slice already serves with one
//!   fewer hop.
//! - **A `sort_by` INDEX consumer** (`sort_by(.x) | .[0]`, and its reversed
//!   twin `sort_by(.x) | reverse | .[0]`): the single-element rows are the
//!   whole-value sort's only (a k=1 projection's win is a fraction of the
//!   slice rows' and the task's scope names `sort | .[0]`). The reversed
//!   index row admits PLAIN `sort` through the reverse hop only; admitting
//!   `sort_by` there would retire this non-row implicitly, which a closed
//!   table never does.
//! - **`sort | last(k)`**: `last(g)` is "the LAST value `g` produces"
//!   (`[g] | .[-1:] | .[]`), never "the last k elements", so `sort | last(3)`
//!   answers `3` and is not a head/tail slice of the sort at all. The
//!   arity-0 `last` (`.[-1]`) IS an `Index` step and now routes (the k=1 tail
//!   row); the k=1 tail slice spelling that also routes is `.[-1:]`.

use jqf_data::{Number, Value};

use crate::analysis::{PartialSort, TopKConsumer};
use crate::program::{ProgramNode, SliceBound, StepAccess};

/// The count `Value` the executor hands `top_k` for one recognized row: the
/// HEAD slice's authored end literal, the TAIL slice's NEGATED start literal,
/// or the constant `1` of a single-element index read. `top_k`'s
/// `resolve_count` ceils it exactly as `resolve_slice_bound` ceils the same
/// reading, so a fractional bound (`.[0:2.7]`, `.[-3.7:]`) stays exact.
///
/// The value is cloned fresh from the arena per run — the row itself carries
/// only node ids.
pub(crate) fn topk_count(nodes: &[ProgramNode], row: PartialSort) -> Option<Value> {
    let ProgramNode::Stage { steps, .. } = &nodes[row.slice_stage().index()] else {
        return None;
    };
    match row.consumer() {
        // Head slices (`.[0:k]`, and the reversed row's `.[0:k]`): the count
        // is the authored END literal.
        TopKConsumer::HeadSlice | TopKConsumer::ReversedHeadSlice => {
            let StepAccess::Slice(bounds) = steps.first()?.access() else {
                return None;
            };
            Some(Value::Number(literal_bound(&bounds.end)?.clone()))
        }
        // Tail slices (`.[-k:]`): the count is the NEGATED START literal.
        TopKConsumer::TailSlice => {
            let StepAccess::Slice(bounds) = steps.first()?.access() else {
                return None;
            };
            Some(Value::Number(literal_bound(&bounds.start)?.try_negated().ok()?))
        }
        // `.[0]` and `.[-1]` are single-element reads: k is always 1.
        TopKConsumer::FirstIndex | TopKConsumer::LastIndex => {
            Some(Value::Number(Number::integer(jqf_data::Integer::from_i64(1))))
        }
    }
}

/// The authored `Number` of a literal slice bound, or `None` for an omitted,
/// `null`, variable, or non-number bound.
fn literal_bound(bound: &SliceBound) -> Option<&Number> {
    match bound {
        SliceBound::Literal(Value::Number(number)) => Some(number),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::PartialSort;
    use alloc::boxed::Box;
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;

    use crate::analysis::TopKConsumer;
    use crate::program::{ProgramNode, ProgramNodeId, SliceBound, SliceBounds, StageStart, StageStep, StepAccess};
    use jqf_builtins::registry::builtins::top_k::TopKDirection;
    use jqf_builtins::registry::{BuiltinOverloadId, SemanticRevision};

    fn nid(index: usize) -> ProgramNodeId {
        ProgramNodeId::from_index(index).expect("node id")
    }

    fn number(spelling: &str) -> jqf_data::Value {
        jqf_data::Value::Number(jqf_data::Number::try_json_literal(spelling).expect("number literal"))
    }

    fn slice(start: SliceBound, end: SliceBound, optional: bool) -> StageStep {
        StageStep::new(StepAccess::Slice(Box::new(SliceBounds { start, end })), optional)
    }

    fn sort_call() -> ProgramNode {
        ProgramNode::call(
            BuiltinOverloadId::new(jqf_builtins::registry::builtins::id::SORT),
            SemanticRevision::new(1),
            Vec::new(),
        )
    }

    fn sort_by_call(key_graph: ProgramNodeId) -> ProgramNode {
        ProgramNode::call(
            BuiltinOverloadId::new(jqf_builtins::registry::builtins::id::SORT_BY),
            SemanticRevision::new(1),
            vec![key_graph],
        )
    }

    /// The row T1 arena for `sort | .[0:3]`: 0 sort call, 1 slice stage,
    /// 2 flat map.
    fn sort_head_arena(end: SliceBound) -> Vec<ProgramNode> {
        vec![
            sort_call(),
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: vec![slice(SliceBound::Literal(number("0")), end, false)],
            },
            ProgramNode::FlatMap {
                upstream: nid(0),
                body: nid(1),
            },
        ]
    }

    fn row(arena: &[ProgramNode]) -> Option<PartialSort> {
        crate::analysis::partial_sorts(arena).into_iter().next()
    }

    #[test]
    fn row_t1_recognizes_sort_head_slice() {
        let arena = sort_head_arena(SliceBound::Literal(number("3")));
        let row = row(&arena).expect("sort | .[0:3] must be recognized");
        assert_eq!(row.flat_map(), nid(2));
        assert_eq!(row.sort_call(), nid(0));
        assert_eq!(row.slice_stage(), nid(1));
        assert_eq!(row.consumer(), TopKConsumer::HeadSlice);
        assert_eq!(row.direction(), TopKDirection::Smallest);
    }

    #[test]
    fn an_omitted_head_start_is_a_row() {
        // `sort | .[:3]` — `.[:3]` is `.[0:3]`; Open start floors to 0.
        let mut arena = sort_head_arena(SliceBound::Literal(number("3")));
        arena[1] = ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![slice(SliceBound::Open, SliceBound::Literal(number("3")), false)],
        };
        assert!(row(&arena).is_some());
    }

    #[test]
    fn a_fractional_head_end_is_a_row() {
        // `sort | .[0:2.7]` answers THREE elements — the end-bound ceil — and
        // top_k(2.7) ceils the same way.
        let arena = sort_head_arena(SliceBound::Literal(number("2.7")));
        assert_eq!(
            row(&arena).expect("fractional end").direction(),
            TopKDirection::Smallest
        );
    }

    #[test]
    fn row_t1_recognizes_sort_tail_slice() {
        // `sort | .[-5:]` — the k-largest row.
        let arena = vec![
            sort_call(),
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: vec![slice(SliceBound::Literal(number("-5")), SliceBound::Open, false)],
            },
            ProgramNode::FlatMap {
                upstream: nid(0),
                body: nid(1),
            },
        ];
        let row = row(&arena).expect("sort | .[-5:] must be recognized");
        assert_eq!(row.direction(), TopKDirection::Largest);
        assert_eq!(row.consumer(), TopKConsumer::TailSlice);
    }

    #[test]
    fn row_t1_recognizes_sort_by_head_slice() {
        // `sort_by(.x) | .[0:10]` — the sort_by row, one filter argument.
        let arena = vec![
            sort_by_call(nid(3)),
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: vec![slice(
                    SliceBound::Literal(number("0")),
                    SliceBound::Literal(number("10")),
                    false,
                )],
            },
            ProgramNode::FlatMap {
                upstream: nid(0),
                body: nid(1),
            },
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: vec![StageStep::new(StepAccess::Key(String::from("x")), false)],
            },
        ];
        let row = row(&arena).expect("sort_by(.x) | .[0:10] must be recognized");
        assert_eq!(row.direction(), TopKDirection::Smallest);
        assert_eq!(row.consumer(), TopKConsumer::HeadSlice);
    }

    #[test]
    fn an_optional_slice_step_is_a_row() {
        // `sort | .[0:3]?` — the `?` is unobservable after a sort: the sort's
        // output is an array or a raise, and an array slice never fails.
        let mut arena = sort_head_arena(SliceBound::Literal(number("3")));
        arena[1] = ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![slice(
                SliceBound::Literal(number("0")),
                SliceBound::Literal(number("3")),
                true,
            )],
        };
        assert!(row(&arena).is_some());
    }

    #[test]
    fn a_slice_on_a_non_sort_source_declines() {
        // `map(f) | .[0:3]` — the upstream is a collect, not a sort call.
        let mut arena = sort_head_arena(SliceBound::Literal(number("3")));
        arena[0] = ProgramNode::CollectArray { body: None };
        assert!(crate::analysis::partial_sorts(&arena).is_empty());
    }

    #[test]
    fn a_non_constant_bound_declines() {
        // `sort | .[0:$k]` — a bound variable is runtime-input-dependent.
        let arena = sort_head_arena(SliceBound::Var(0));
        assert!(crate::analysis::partial_sorts(&arena).is_empty());
        // `sort | .[0:null]` — an authored null end is an OPEN bound: the whole
        // array, not a bounded head.
        let arena = sort_head_arena(SliceBound::Literal(jqf_data::Value::Null));
        assert!(crate::analysis::partial_sorts(&arena).is_empty());
        // `sort | .[0:]` — an omitted end is the whole array.
        let arena = sort_head_arena(SliceBound::Open);
        assert!(crate::analysis::partial_sorts(&arena).is_empty());
    }

    #[test]
    fn a_len_relative_negative_end_declines() {
        // `sort | .[0:-2]` means "all but the last two" — not a top-k.
        let arena = sort_head_arena(SliceBound::Literal(number("-2")));
        assert!(crate::analysis::partial_sorts(&arena).is_empty());
    }

    #[test]
    fn a_zero_or_negative_head_end_declines() {
        // `sort | .[0:0]` is the empty head — a zero-win degenerate shape, and
        // `.[0:-0.5]` wraps to the whole array. Both decline.
        let arena = sort_head_arena(SliceBound::Literal(number("0")));
        assert!(crate::analysis::partial_sorts(&arena).is_empty());
        // `.[0:-0.5]`: the strictly-negative end ceils to zero, which the
        // boundary law normalizes OPEN — the whole array, not a bounded head.
        let arena = sort_head_arena(SliceBound::Literal(number("-0.5")));
        assert!(crate::analysis::partial_sorts(&arena).is_empty());
    }

    #[test]
    fn a_middle_or_whole_slice_declines() {
        // `sort | .[3:]` — a positive start is a middle slice, not a tail.
        let arena = vec![
            sort_call(),
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: vec![slice(SliceBound::Literal(number("3")), SliceBound::Open, false)],
            },
            ProgramNode::FlatMap {
                upstream: nid(0),
                body: nid(1),
            },
        ];
        assert!(crate::analysis::partial_sorts(&arena).is_empty());
        // `sort | .[:]` — both bounds open: the whole array.
        let mut arena = sort_head_arena(SliceBound::Open);
        arena[1] = ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![slice(SliceBound::Open, SliceBound::Open, false)],
        };
        assert!(crate::analysis::partial_sorts(&arena).is_empty());
    }

    #[test]
    fn a_consumer_that_is_not_the_sole_one_declines() {
        // `sort | .[0:3] | .a` — a residual step after the slice.
        let mut arena = sort_head_arena(SliceBound::Literal(number("3")));
        arena[1] = ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![
                slice(
                    SliceBound::Literal(number("0")),
                    SliceBound::Literal(number("3")),
                    false,
                ),
                StageStep::new(StepAccess::Key(String::from("a")), false),
            ],
        };
        assert!(crate::analysis::partial_sorts(&arena).is_empty());
        // `sort | (.[0:3], .[1])` — a comma body is two consumers.
        let mut arena = sort_head_arena(SliceBound::Literal(number("3")));
        arena[1] = ProgramNode::Choice {
            left: nid(1),
            right: nid(1),
        };
        assert!(crate::analysis::partial_sorts(&arena).is_empty());
    }

    #[test]
    fn a_nested_context_is_still_a_row() {
        // `[sort | .[0:3]] | length` — the row matches any FlatMap with the
        // sort+slice shape, wherever it sits in the arena.
        let mut arena = sort_head_arena(SliceBound::Literal(number("3")));
        arena.push(ProgramNode::CollectArray { body: Some(nid(2)) });
        arena.push(ProgramNode::call(
            BuiltinOverloadId::new(jqf_builtins::registry::builtins::id::LENGTH),
            SemanticRevision::new(1),
            Vec::new(),
        ));
        let rows = crate::analysis::partial_sorts(&arena);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].flat_map(), nid(2));
    }

    #[test]
    fn last_is_not_a_head_or_tail_slice() {
        // `sort | last(3)` lowers to `sort | ([3] | .[-1:] | .[])` — the sort's
        // consumer is a COLLECT, not a slice, and `last(g)` is "the last value
        // g produces", never a top-k. Decline.
        let mut arena = sort_head_arena(SliceBound::Literal(number("3")));
        arena[1] = ProgramNode::CollectArray { body: Some(nid(1)) };
        assert!(crate::analysis::partial_sorts(&arena).is_empty());
    }

    #[test]
    fn an_index_consumer_is_a_k1_row_for_plain_sort() {
        // `sort | .[0]` (and the prelude `first`, which lowers to it): the
        // single SMALLEST element — a k=1 partial sort.
        let arena = vec![
            sort_call(),
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: vec![StageStep::new(StepAccess::Index(0), false)],
            },
            ProgramNode::FlatMap {
                upstream: nid(0),
                body: nid(1),
            },
        ];
        let first_row = row(&arena).expect("sort | .[0] must be recognized");
        assert_eq!(first_row.consumer(), TopKConsumer::FirstIndex);
        assert_eq!(first_row.direction(), TopKDirection::Smallest);
        // `sort | .[-1]` (and `last`): the single LARGEST element.
        let mut arena = arena;
        arena[1] = ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![StageStep::new(StepAccess::Index(-1), false)],
        };
        let last_row = row(&arena).expect("sort | .[-1] must be recognized");
        assert_eq!(last_row.consumer(), TopKConsumer::LastIndex);
        assert_eq!(last_row.direction(), TopKDirection::Largest);
        // Any other index is not a row (`.[2]` is a middle element, `.[-2]`
        // the second-from-last — neither is an end read).
        for index in [2, -2] {
            arena[1] = ProgramNode::Stage {
                start: StageStart::Current,
                steps: vec![StageStep::new(StepAccess::Index(index), false)],
            };
            assert!(
                crate::analysis::partial_sorts(&arena).is_empty(),
                "sort | .[{index}] must decline"
            );
        }
    }

    #[test]
    fn the_reversed_sort_head_slice_is_a_k_largest_row() {
        // `sort_by(.x) | reverse | .[0:2]` — pipes lower RIGHT-nested, so the
        // arena is `FlatMap{Call(SORT_BY), FlatMap{Call(REVERSE), Stage[Slice]}}`:
        // the REVERSE is the sort's sole consumer and the head slice is the
        // REVERSE's sole consumer — the k-LARGEST with the reversed emission.
        let arena = vec![
            sort_by_call(nid(5)),
            ProgramNode::call(
                BuiltinOverloadId::new(jqf_builtins::registry::builtins::id::REVERSE),
                SemanticRevision::new(1),
                Vec::new(),
            ),
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: vec![slice(
                    SliceBound::Literal(number("0")),
                    SliceBound::Literal(number("2")),
                    false,
                )],
            },
            ProgramNode::FlatMap {
                upstream: nid(1),
                body: nid(2),
            },
            ProgramNode::FlatMap {
                upstream: nid(0),
                body: nid(3),
            },
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: vec![StageStep::new(StepAccess::Key(String::from("x")), false)],
            },
        ];
        let rows = crate::analysis::partial_sorts(&arena);
        assert_eq!(rows.len(), 1, "exactly one row for the reversed shape");
        let row = rows[0];
        assert_eq!(row.flat_map(), nid(4));
        assert_eq!(row.sort_call(), nid(0));
        assert_eq!(row.slice_stage(), nid(2));
        assert_eq!(row.consumer(), TopKConsumer::ReversedHeadSlice);
        assert_eq!(row.direction(), TopKDirection::Largest);
    }

    #[test]
    fn a_tail_slice_over_a_reversed_sort_declines() {
        // `sort_by(.x) | reverse | .[-2:]` is the k SMALLEST — the plain head
        // slice already serves that with one fewer hop.
        let arena = vec![
            sort_by_call(nid(5)),
            ProgramNode::call(
                BuiltinOverloadId::new(jqf_builtins::registry::builtins::id::REVERSE),
                SemanticRevision::new(1),
                Vec::new(),
            ),
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: vec![slice(SliceBound::Literal(number("-2")), SliceBound::Open, false)],
            },
            ProgramNode::FlatMap {
                upstream: nid(1),
                body: nid(2),
            },
            ProgramNode::FlatMap {
                upstream: nid(0),
                body: nid(3),
            },
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: Vec::new(),
            },
        ];
        assert!(crate::analysis::partial_sorts(&arena).is_empty());
    }

    #[test]
    fn a_reversed_whole_value_sort_is_a_row_too() {
        // `sort | reverse | .[0:2]` is the same reversed row without a
        // projection: the k-LARGEST by WHOLE value, published descending. The
        // whole-value `sort` is stable exactly as `sort_by` is, so the
        // reversed tie law holds identically.
        let arena = vec![
            sort_call(),
            ProgramNode::call(
                BuiltinOverloadId::new(jqf_builtins::registry::builtins::id::REVERSE),
                SemanticRevision::new(1),
                Vec::new(),
            ),
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: vec![slice(
                    SliceBound::Literal(number("0")),
                    SliceBound::Literal(number("2")),
                    false,
                )],
            },
            ProgramNode::FlatMap {
                upstream: nid(1),
                body: nid(2),
            },
            ProgramNode::FlatMap {
                upstream: nid(0),
                body: nid(3),
            },
        ];
        let rows = crate::analysis::partial_sorts(&arena);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].consumer(), TopKConsumer::ReversedHeadSlice);
        assert_eq!(rows[0].direction(), TopKDirection::Largest);
    }

    #[test]
    fn a_sort_by_index_consumer_declines() {
        // `sort_by(.x) | .[0]` — the index rows are the whole-value sort's
        // only; a k=1 projection's win is a fraction of the slice rows'.
        let arena = vec![
            sort_by_call(nid(3)),
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: vec![StageStep::new(StepAccess::Index(0), false)],
            },
            ProgramNode::FlatMap {
                upstream: nid(0),
                body: nid(1),
            },
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: Vec::new(),
            },
        ];
        assert!(crate::analysis::partial_sorts(&arena).is_empty());
    }
}
