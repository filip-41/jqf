//! `top_k` — true partial-sort (O(n log k)) beyond the reference.
//!
//! One job: own the family and overload records for `top_k`, plus the evaluator payload that implements the
//! bounded-heap partial sort. The sort reuses the ordering family's shared `total_cmp` comparison so the partial sort's
//! notion of order cannot drift from the full sort's.
//!
//! The `by?` projection follows `sort_by`'s exact fold rule: zero results → `null`, one → that value, multiple →
//! an array.
//!
//! **Where the algorithmic win actually lands.** The whole-value door pays off:
//! `top_k(10)` is 1.81x `sort | .[0:10]` over a 2M-element flat array, and `top_k(1000)` is 2.40x. The PROJECTION door
//! does not yet: over 1M objects, `top_k(k; .v)` runs at 0.92x-0.95x of `sort_by(.v) | .[0:k]` for k in {1, 10, 1000}
//! — the per-element projection evaluation costs more than the bounded heap saves, so the O(n log k) advantage is
//! spent before it is collected. Correctness is unaffected (a 700-case randomized differential against both spellings
//! is clean); the projection path is a perf follow-up, not a claim this module gets to make.
//!
//! Negative space: it renders no message (that is `crate::error::message`), owns no tie law (that is
//! `crate::semantics::keyed`), and drives no filter per-element (that is the executor's topk frame).

use alloc::vec::Vec;
use core::cmp::Ordering;

use jqf_data::{Array, Value};
use jqf_resource::ResourceContext;

use super::id;
use crate::error::EngineRunError;
use crate::error::message;
use crate::registry::record::{
    BuiltinExample, BuiltinExecution, BuiltinFamilyId, BuiltinFamilyRecord, BuiltinOverloadId, BuiltinOverloadRecord,
    DemandTransfer, Effects, ParameterKind, SemanticRevision,
};
use crate::semantics::order::total_cmp;
use crate::semantics::path::raise;

/// Which end of the sorted order `top_k` selects.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TopKDirection {
    /// The k smallest elements.
    Smallest,
    /// The k largest elements — the tail-slice recognition's direction. The tie law is the tail's: among equal keys
    /// the LATER source elements are kept, exactly the elements `sort_by(f) | .[-k:]` takes.
    Largest,
}

// ── family / overload records ────────────────────────────

pub const FAMILIES: &[BuiltinFamilyRecord] = &[TOP_K_FAMILY];

pub const OVERLOADS: &[BuiltinOverloadRecord] = &[TOP_K_1_OVERLOAD, TOP_K_2_OVERLOAD];

/// The top-k execution payloads, aligned one-to-one with [`OVERLOADS`].
///
/// Both arities share one payload: the arity decides whether a `by` projection argument is present, and the evaluator
/// reads that from the call node, not from the dispatch payload. Every entry carries its overload id so the const
/// coverage walk in `registry::dispatch` can prove pairwise alignment.
pub const PAYLOADS: &[(u16, TopKDirection)] = &[
    (id::TOP_K_1, TopKDirection::Smallest),
    (id::TOP_K_2, TopKDirection::Smallest),
];

const TOP_K_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::TOP_K_FAMILY_ID),
    canonical_name: "top_k",
    category: "order",
    summary: "A partial-order top-k builtin: the n smallest elements, \
              O(n log k) with a bounded heap.",
    detail: "top_k(n) returns the n smallest elements by whole-value \
             comparison. top_k(n; f) returns the n smallest by a projection f; \
             the projection follows sort_by's fold rule (zero → null, one → \
             that value, multiple → array). The result is byte-identical to \
             sort_by(f) | .[0:n] — same order, same tie law — for every \
             non-negative n; a negative n answers [] and a non-number n raises, \
             because .[0:-2]'s len-relative reading is not a top-k.",
};

const TOP_K_1_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::TOP_K_1),
    family: BuiltinFamilyId::new(id::TOP_K_FAMILY_ID),
    canonical_name: "top_k",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    // The count argument fans out: `top_k(1, 2)` is two answers and `top_k(empty)` is none, exactly as `.[0:(1,2)]` and
    // `.[0:empty]` are.
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "top_k(2)",
            input: "[3,1,4,1,5]",
            expected: "[1,1]\n",
        },
        BuiltinExample {
            program: "top_k(100)",
            input: "[3,1,2]",
            expected: "[1,2,3]\n",
        },
        // A MULTI-VALUED count: one answer per count, in order. Without this example an implementation that reads only
        // one output of the count filter passes every other one.
        BuiltinExample {
            program: "top_k(1, 3)",
            input: "[3,1,4,1,5]",
            expected: "[1]\n[1,1,3]\n",
        },
    ],
};

const TOP_K_2_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::TOP_K_2),
    family: BuiltinFamilyId::new(id::TOP_K_FAMILY_ID),
    canonical_name: "top_k",
    arity: 2,
    parameters: &[ParameterKind::Filter, ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "top_k(2; .a)",
            input: "[{\"a\":3,\"i\":0},{\"a\":1,\"i\":1},{\"a\":2,\"i\":2},{\"a\":1,\"i\":3}]",
            expected: "[{\"a\":1,\"i\":1},{\"a\":1,\"i\":3}]\n",
        },
        BuiltinExample {
            program: "top_k(10; .a)",
            input: "[{\"a\":3},{\"a\":1},{\"a\":2}]",
            expected: "[{\"a\":1},{\"a\":2},{\"a\":3}]\n",
        },
        // A MULTI-VALUED count fans out here too; the projection is evaluated once and shared across the fan-out.
        BuiltinExample {
            program: "top_k(1, 2; .a)",
            input: "[{\"a\":3},{\"a\":1},{\"a\":2}]",
            expected: "[{\"a\":1}]\n[{\"a\":1},{\"a\":2}]\n",
        },
        // A MULTI-VALUED projection folds to an array, `sort_by`'s exact rule, so an implementation that keeps only one
        // output of it is caught.
        BuiltinExample {
            program: "top_k(2; .a, .b)",
            input: "[{\"a\":1,\"b\":9},{\"a\":1,\"b\":2},{\"a\":0,\"b\":5}]",
            expected: "[{\"a\":0,\"b\":5},{\"a\":1,\"b\":2}]\n",
        },
    ],
};

// ── helper utilities ───────────────────────────────

/// One entry in the bounded heap: a key and the index of the element it came from. The `index` field preserves source
/// order for stable tie-breaking.
struct HeapEntry {
    key: Value,
    index: usize,
}

/// The recognizer's INCREMENTAL heap drive: the same bounded heap `top_k` uses, exposed as a per-element `offer` so the
/// executor can run the projection and the heap in ONE pass over the input — O(k) memory, no key vector, no spill —
/// instead of collecting every key first.
///
/// [`top_k_law`] is this drive over a materialized key vector; the recognizer drives the same heap with keys produced
/// by the keyed static lane.
pub struct TopKHeap {
    heap: BoundedHeap,
}

impl TopKHeap {
    /// A fresh bounded heap of capacity `k` for one direction.
    pub fn new(k: usize, direction: TopKDirection) -> Self {
        Self {
            heap: BoundedHeap::new(k, direction),
        }
    }

    /// Offers one element's key to the heap: admitted (and pushed or root-replaced) when it beats the current worst,
    /// dropped otherwise. The heap's capacity is fixed at `new`, so an offer never allocates.
    pub fn offer(&mut self, key: Value, index: usize) -> Result<(), EngineRunError> {
        let entry = HeapEntry { key, index };
        if self.heap.should_admit(&entry.key, entry.index)? {
            if self.heap.is_full() {
                self.heap.replace_root(entry)?;
            } else {
                self.heap.push(entry)?;
            }
        }
        Ok(())
    }

    /// Consumes the heap and publishes the sorted top-k elements.
    pub fn finish(self, elements: &Array, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
        build_result(self.heap, elements, resources)
    }
}

/// Builds the bounded heap a recognizer drive uses for one count value:
/// resolves the count under the slice end-bound law (`ceil`, exactly as `resolve_count` does), clamps it to the array's
/// length, and sizes the heap.
///
/// `None` for a count clamped to zero — an empty array (or a zero count) — which the caller publishes as the empty
/// array, the slice's own answer.
pub fn heap_for(count: &Value, array_len: usize, direction: TopKDirection) -> Result<Option<TopKHeap>, EngineRunError> {
    let k = clamp_count(resolve_count(count)?, array_len);
    Ok((k > 0).then(|| TopKHeap::new(k, direction)))
}

/// A bounded max-heap for k-smallest (or min-heap for k-largest).
///
/// The heap has capacity `k`. For k-smallest: we keep a max-heap — the root is the LARGEST element currently in the
/// top-k. An incoming element displaces the root if it is smaller. For k-largest: we keep a min-heap — the root is
/// the SMALLEST element currently in the top-k; an incoming element displaces the root if it is larger.
struct BoundedHeap {
    data: Vec<HeapEntry>,
    direction: TopKDirection,
    /// The semantic bound `k`: the heap holds at most this many entries.
    /// `Vec::with_capacity` is documented as free to allocate MORE than requested, so `len == capacity` is not the
    /// bound — a backing store that rounded up would admit a k+1st element and `build_result` emits every entry.
    k: usize,
}

impl BoundedHeap {
    fn new(capacity: usize, direction: TopKDirection) -> Self {
        Self {
            data: Vec::with_capacity(capacity),
            direction,
            k: capacity,
        }
    }

    fn is_full(&self) -> bool {
        self.data.len() >= self.k
    }

    /// Returns `true` if the incoming element should be added/replace the root.
    fn should_admit(&self, incoming_key: &Value, incoming_index: usize) -> Result<bool, EngineRunError> {
        if !self.is_full() {
            return Ok(true);
        }
        let root = &self.data[0];
        let ord = total_cmp(incoming_key, &root.key)
            .map_err(|_| EngineRunError::internal_contract("comparison too deep in top_k heap"))?;
        match self.direction {
            TopKDirection::Smallest => {
                // We want k smallest. Incoming is "better" if its key is smaller, or same key but earlier index.
                match ord {
                    Ordering::Less => Ok(true),
                    Ordering::Greater => Ok(false),
                    Ordering::Equal => Ok(incoming_index < root.index),
                }
            }
            TopKDirection::Largest => {
                // We want k largest. Incoming is "better" if its key is larger, or same key but LATER index — the
                // tail of the stable sort keeps the last k of a tied run, so the heap must keep the latest indices
                // among equal keys.
                match ord {
                    Ordering::Greater => Ok(true),
                    Ordering::Less => Ok(false),
                    Ordering::Equal => Ok(incoming_index > root.index),
                }
            }
        }
    }

    /// Push an entry onto the heap. The caller must have called `should_admit` first and ensured there is room: a slip
    /// past the `is_full` gate would silently grow the heap beyond `k`, which is what `top_k_never_emits_more_than_k`
    /// pins.
    fn push(&mut self, entry: HeapEntry) -> Result<(), EngineRunError> {
        self.data.push(entry);
        self.sift_up(self.data.len() - 1)
    }

    /// Replaces the root with a new entry (when the heap is full and [`Self::should_admit`] said yes).
    fn replace_root(&mut self, entry: HeapEntry) -> Result<(), EngineRunError> {
        self.data[0] = entry;
        self.sift_down(0)
    }

    fn sift_up(&mut self, mut idx: usize) -> Result<(), EngineRunError> {
        while idx > 0 {
            let parent = (idx - 1) / 2;
            if self.heap_less(parent, idx)? {
                break;
            }
            self.data.swap(parent, idx);
            idx = parent;
        }
        Ok(())
    }

    fn sift_down(&mut self, mut idx: usize) -> Result<(), EngineRunError> {
        loop {
            let left = 2 * idx + 1;
            let right = 2 * idx + 2;
            let mut best = idx;
            if left < self.data.len() && !self.heap_less(best, left)? {
                best = left;
            }
            if right < self.data.len() && !self.heap_less(best, right)? {
                best = right;
            }
            if best == idx {
                break;
            }
            self.data.swap(idx, best);
            idx = best;
        }
        Ok(())
    }

    /// Returns true if `parent` has higher heap priority than `child`.
    ///
    /// For k-smallest (max-heap): "higher priority" means larger/worse — the root is the element we'd replace first.
    /// For k-largest (min-heap): "higher priority" means smaller/worse.
    ///
    /// Comparison is by (key, index) lexicographically — index breaks ties so the heap preserves source order (stable
    /// tie-breaking).
    ///
    /// The too-deep error is the SAME one the admission gate raises: an admitted pair can still out-depth the
    /// comparison ceiling against each other (admission only ever compared an entrant against a shallower root), so
    /// sifting must propagate it, not flatten it to `Equal`.
    fn heap_less(&self, parent: usize, child: usize) -> Result<bool, EngineRunError> {
        let pk = &self.data[parent];
        let ck = &self.data[child];
        let ord = total_cmp(&pk.key, &ck.key)
            .map_err(|_| EngineRunError::internal_contract("comparison too deep in top_k heap"))?;
        match self.direction {
            TopKDirection::Smallest => Ok(match ord {
                // Max-heap: larger key = higher priority = stays above
                Ordering::Greater => true,
                Ordering::Less => false,
                // Equal keys: later index = higher priority (worse) = stays above
                Ordering::Equal => pk.index >= ck.index,
            }),
            TopKDirection::Largest => Ok(match ord {
                // Min-heap: smaller key = higher priority = stays above
                Ordering::Less => true,
                Ordering::Greater => false,
                // Equal keys: EARLIER index = higher priority (worse) = stays above — the element the tail would shed
                // first, which is what makes the admission rule keep the latest indices.
                Ordering::Equal => pk.index < ck.index,
            }),
        }
    }

    /// Consume the heap and return entries sorted in ascending order, with ties broken by index (stable).
    fn into_sorted(mut self) -> Result<Vec<HeapEntry>, EngineRunError> {
        // `sort_by`'s closure cannot return an error, so a too-deep comparison is latched and raised after the sort;
        // the partial order it leaves behind is discarded with the `Err`.
        let too_deep = core::cell::Cell::new(None);
        // The entries are heap-ordered; the ONE law that matters is the stable key-then-index order `build_result`
        // emits, which the sort below owns outright.
        self.data.sort_by(|a, b| {
            let ord = total_cmp(&a.key, &b.key).unwrap_or_else(|_| {
                too_deep.set(Some(EngineRunError::internal_contract(
                    "comparison too deep in top_k heap",
                )));
                Ordering::Equal
            });
            ord.then_with(|| a.index.cmp(&b.index))
        });
        match too_deep.take() {
            Some(err) => Err(err),
            None => Ok(self.data),
        }
    }
}

/// Build the top-k result from a bounded heap over the original array elements.
///
/// The heap contains `HeapEntry { key, index }`. We extract entries in key-ascending order with stable tie-breaking,
/// then return the original elements at those indices.
fn build_result(
    heap: BoundedHeap,
    elements: &Array,
    _resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let sorted_entries = heap.into_sorted()?;
    let mut out = Array::try_with_capacity(sorted_entries.len()).map_err(|_| EngineRunError::allocation_failure())?;
    for entry in sorted_entries {
        let element = elements
            .get(entry.index)
            .ok_or_else(|| EngineRunError::internal_contract("top_k heap index out of bounds"))?
            .clone();
        out.try_push(element)
            .map_err(|_| EngineRunError::allocation_failure())?;
    }
    Ok(Value::Array(out))
}

/// Compute the projection key for a single element.
///
/// `key_outputs` is the collected outputs of the projection filter applied to that element. Folded per `sort_by`'s law.
fn key_for_element(key_outputs: &[Value], _resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    match key_outputs.len() {
        0 => Ok(Value::Null),
        1 => Ok(key_outputs[0].clone()),
        _ => {
            let array = Array::try_from_vec(key_outputs.to_vec()).map_err(|_| EngineRunError::allocation_failure())?;
            Ok(Value::Array(array))
        }
    }
}

// ── the evaluator entry point ────────────────────────────

/// The `top_k` evaluator: receives the materialized input array and the per-element keys (as an array-of-arrays of
/// projection outputs), plus the count. Returns the k smallest/largest elements.
///
/// `count_value` is the evaluated n filter output.
/// `projection_keys` is an array of arrays: `projection_keys[i]` is the array of ALL outputs the projection filter
/// produced for element i.
/// When there is no projection, `projection_keys` is empty and elements are compared by whole value.
pub fn top_k_law(
    direction: TopKDirection,
    input: &Value,
    count_value: &Value,
    projection_keys: &[Vec<Value>],
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let Value::Array(array) = input.untagged() else {
        let operand = message::dump_trunc_owned(input)?;
        let text = message::not_sortable_message(input.kind(), &operand)?;
        return Err(raise(&text, resources));
    };

    let n = resolve_count(count_value)?;
    let k = clamp_count(n, array.len());

    if k == 0 {
        return Ok(Value::Array(
            Array::try_new().map_err(|_| EngineRunError::allocation_failure())?,
        ));
    }

    let has_projection = !projection_keys.is_empty();

    let mut heap = TopKHeap::new(k, direction);

    for (index, element) in array.iter().enumerate() {
        let key: Value = if has_projection {
            let outputs = projection_keys
                .get(index)
                .ok_or_else(|| EngineRunError::internal_contract("top_k: fewer key vecs than elements"))?;
            key_for_element(outputs, resources)?
        } else {
            // Whole-value comparison: element is its own key.
            element.clone()
        };

        heap.offer(key, index)?;
    }

    heap.finish(array, resources)
}

/// Resolves the count filter's output to a element count, following the `.[0:k]` END-BOUND law that `top_k` is defined
/// against.
///
/// The count is read as an f64 and rounded UP, exactly as the slice does:
/// `sort | .[0:2.7]` answers three elements, so `top_k(2.7)` must answer three too. Reading it as an f64 rather than as
/// an integer is what makes that work — jqf's `2.7` is an exact DECIMAL, not a float, and an `as_integer()` view of
/// it is `None`. The previous spelling folded that `None` to `0` and answered `[]` for every fractional count.
///
/// Two deliberate departures from `.[0:k]`, both because a top-k has no len-relative reading:
///
/// - a NEGATIVE count answers `[]`. `.[0:-2]` means "all but the last two",
///   which is not a top-k of anything.
/// - a NON-NUMBER count RAISES [`EngineRunError::SliceIndices`] — the same
///   "Array/string slice indices must be integers" sentence `.[0:"x"]` raises.
///   `null` raises with them: `.[0:null]` is an OPEN bound (the whole array), and silently answering everything for
///   `top_k(null)` is a footgun. This is a user error about the argument, so it must not be an internal-contract
///   violation; that class is for engine bugs and it is what the caller saw before (`codec failed:
///   InternalContractViolation`).
fn resolve_count(value: &Value) -> Result<usize, EngineRunError> {
    let Value::Number(number) = value.untagged() else {
        return Err(EngineRunError::SliceIndices);
    };
    let count = crate::semantics::order::to_f64(number);
    if count.is_nan() || count <= 0.0 {
        return Ok(0);
    }
    // `as` saturates at usize::MAX for an out-of-range float, which is the clamp `clamp_count` wants anyway.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the ceil is non-negative and finite here, and `as` saturates rather than wraps"
    )]
    Ok(count.ceil() as usize)
}

/// Clamps the requested count to what the array can supply.
fn clamp_count(k: usize, array_len: usize) -> usize {
    k.min(array_len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;
    use alloc::string::String;
    use alloc::vec;
    use jqf_data::{Array, Number, Value};
    use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

    static CONTROL: ContinueControl = ContinueControl;

    fn ledger() -> ResourceContext<'static> {
        let limits = ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
        let account = RequestAccount::try_new(limits).expect("test account");
        let work = WorkMeter::try_new_v1(1).expect("test work meter");
        ResourceContext::new(account, &CONTROL, work).expect("test ledger")
    }

    /// An exact DECIMAL number, which is what jqf's `2.7` literal actually is — the distinction the count law turns
    /// on.
    fn number_dec(spelling: &str) -> Value {
        Value::Number(Number::decimal(
            jqf_data::Decimal::parse(spelling).expect("decimal spelling"),
        ))
    }

    fn number_int(n: i64) -> Value {
        Value::Number(Number::integer(jqf_data::Integer::from_i64(n)))
    }

    fn array_of_ints(values: &[i64]) -> Value {
        let _r = &ledger();
        let arr = Array::try_from_vec(values.iter().map(|&n| number_int(n)).collect::<Vec<_>>()).expect("array");
        Value::Array(arr)
    }

    fn run_topk(
        direction: TopKDirection,
        input: &Value,
        k: usize,
        keys: &[Vec<Value>],
    ) -> Result<Value, EngineRunError> {
        let law = direction;
        let count = Value::Number(Number::integer(jqf_data::Integer::from_i64(
            i64::try_from(k).expect("k fits i64"),
        )));
        top_k_law(law, input, &count, keys, &ledger())
    }

    /// Sifting propagates the SAME too-deep error the admission gate raises:
    /// two admitted entries can each have passed admission against shallower roots yet out-depth the comparison ceiling
    /// against EACH OTHER, so `heap_less` must raise it rather than flatten the failure to `Equal` and silently reorder
    /// what admission refuses loudly.
    #[test]
    fn top_k_heap_sift_propagates_too_deep_comparison() {
        extern crate std;

        // One level past the comparison row's ceiling (10000 still answers).
        let deep = || {
            let mut value = number_int(0);
            for _ in 0..10_001 {
                let arr = Array::try_from_vec(vec![value]).expect("nested array");
                value = Value::Array(arr);
            }
            value
        };
        let input = Value::Array(Array::try_from_vec(vec![deep(), deep(), number_int(7)]).expect("array"));
        // Runs on a big-stack thread: the comparison recursion and the drop of the nested values are the request
        // thread's job, not this test thread's (the same reasoning the spill comparator test pins).
        std::thread::Builder::new()
            .stack_size(256 << 20)
            .spawn(move || {
                // k=2: the second push sifts the two over-ceiling entries against each other before any shallow element
                // is offered.
                let err = run_topk(TopKDirection::Smallest, &input, 2, &[]).expect_err("too-deep pair must raise");
                assert!(matches!(
                    err,
                    EngineRunError::Codec(codec)
                        if matches!(
                            codec.kind(),
                            jqf_codec_core::CodecFailureKind::InternalContractViolation { .. }
                        )
                ));
            })
            .expect("spawn")
            .join()
            .expect("big-stack thread");
    }

    #[test]
    fn top_k_zero_returns_empty() {
        let input = array_of_ints(&[3, 1, 2]);
        let result = run_topk(TopKDirection::Smallest, &input, 0, &[]).expect("top_k(0)");
        let Value::Array(arr) = result.untagged() else {
            panic!("expected array")
        };
        assert_eq!(arr.len(), 0);
    }

    #[test]
    fn top_k_larger_than_array_returns_all_sorted() {
        let input = array_of_ints(&[3, 1, 4, 1, 5, 9, 2, 6]);
        let result = run_topk(TopKDirection::Smallest, &input, 100, &[]).expect("top_k(100)");
        let Value::Array(arr) = result.untagged() else {
            panic!("expected array")
        };
        assert_eq!(arr.len(), 8);
        // Verify sorted order
        let nums: Vec<i64> = arr
            .iter()
            .map(|v| {
                let Value::Number(n) = v.untagged() else {
                    panic!("expected number")
                };
                n.to_i64().unwrap()
            })
            .collect();
        assert_eq!(nums, vec![1, 1, 2, 3, 4, 5, 6, 9]);
    }

    #[test]
    fn top_k_smallest_basic() {
        let input = array_of_ints(&[3, 1, 4, 1, 5, 9, 2, 6]);
        let result = run_topk(TopKDirection::Smallest, &input, 3, &[]).expect("top_k(3)");
        let Value::Array(arr) = result.untagged() else {
            panic!("expected array")
        };
        assert_eq!(arr.len(), 3);
        let nums: Vec<i64> = arr
            .iter()
            .map(|v| {
                let Value::Number(n) = v.untagged() else {
                    panic!("expected number")
                };
                n.to_i64().unwrap()
            })
            .collect();
        assert_eq!(nums, vec![1, 1, 2]);
    }

    /// `BoundedHeap::is_full` must test the k BOUND, never the backing store's actual capacity. `Vec::with_capacity(k)`
    /// is documented as free to allocate MORE than requested, so a `len == capacity` test admits a k+1st element under
    /// an over-allocating allocator, and `build_result` emits every entry with no truncate — `top_k(3; f)` then
    /// publishes four. The invariant below is the contract, checked over both directions and several k; it is the
    /// standing guard for allocators that round up, and it pins `is_full` against a slip back to a capacity-based test.
    #[test]
    fn top_k_never_emits_more_than_k() {
        // A long flat array so every admission decision is a displacement.
        let input = array_of_ints(&[9, 2, 8, 3, 7, 4, 6, 5, 1, 0, 5, 5, 5, 5, 5, 5]);
        for k in [1usize, 2, 3, 5, 7] {
            for direction in [TopKDirection::Smallest, TopKDirection::Largest] {
                let result = run_topk(direction, &input, k, &[]).expect("top_k");
                let Value::Array(arr) = result.untagged() else {
                    panic!("expected array")
                };
                assert!(
                    arr.len() <= k,
                    "top_k({k}) must never emit more than {k}, got {}",
                    arr.len()
                );
            }
        }
        // The heap's own bound: full at k entries, before any allocator rounding could matter.
        for k in [1usize, 3] {
            let mut heap = BoundedHeap::new(k, TopKDirection::Smallest);
            for i in 0..k {
                assert!(!heap.is_full(), "heap below k must not report full");
                heap.data.push(HeapEntry {
                    key: number_int(i64::try_from(i).expect("k fits i64")),
                    index: i,
                });
            }
            assert!(heap.is_full(), "heap at k entries must report full");
        }
    }

    #[test]
    fn top_k_largest_basic() {
        let input = array_of_ints(&[3, 1, 4, 1, 5, 9, 2, 6]);
        let result = run_topk(TopKDirection::Largest, &input, 2, &[]).expect("top_k largest(2)");
        let Value::Array(arr) = result.untagged() else {
            panic!("expected array")
        };
        assert_eq!(arr.len(), 2);
        let nums: Vec<i64> = arr
            .iter()
            .map(|v| {
                let Value::Number(n) = v.untagged() else {
                    panic!("expected number")
                };
                n.to_i64().unwrap()
            })
            .collect();
        assert_eq!(nums, vec![6, 9]);
    }

    #[test]
    fn top_k_stable_for_equal_keys() {
        let r = &ledger();
        // Create elements distinguished by index but with equal sort keys
        let mut elements = Array::try_new().expect("array");
        // Tag each with stable-id so we can verify source order preserved
        for i in 0..5 {
            let mut obj = jqf_data::ObjectBuilder::new();
            obj.try_insert_last(jqf_data::ObjectKey::try_from_str("id").expect("key"), number_int(i))
                .expect("insert");
            obj.try_insert_last(
                jqf_data::ObjectKey::try_from_str("key").expect("key"),
                number_int(1), // ALL same key
            )
            .expect("insert");
            elements
                .try_push(Value::Object(obj.try_finish().expect("obj")))
                .expect("push");
        }
        let input = Value::Array(elements);

        let keys: Vec<Vec<Value>> = (0..5)
            .map(|_| vec![number_int(1)]) // all equal keys
            .collect();

        let law = TopKDirection::Smallest;
        let count = Value::Number(Number::integer(jqf_data::Integer::from_i64(3)));
        let result = top_k_law(law, &input, &count, &keys, r).expect("top_k with equal keys");

        let Value::Array(arr) = result.untagged() else {
            panic!("expected array")
        };
        assert_eq!(arr.len(), 3);
        // Source order must be preserved: elements with id 0, 1, 2
        let ids: Vec<i64> = arr
            .iter()
            .map(|v| {
                let Value::Object(obj) = v.untagged() else {
                    panic!("expected object")
                };
                let Value::Number(n) = obj.get("id").unwrap().untagged() else {
                    panic!("expected number")
                };
                n.to_i64().unwrap()
            })
            .collect();
        assert_eq!(ids, vec![0, 1, 2]);
    }

    // ── randomized differential: top_k vs sort-then-slice ───────────────

    /// Simple LCG for deterministic "random" test values.
    fn next_rand(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        *state
    }

    /// Build a random array of objects with `{.key: int, .id: int}` entries.
    fn random_object_array(size: usize, rng: &mut u64, _r: &ResourceContext<'_>) -> Value {
        let mut arr = Array::try_new().expect("array");
        for id in 0..i64::try_from(size).expect("size fits i64") {
            let key_val = i64::try_from(next_rand(rng) % 1000).expect("key fits i64");
            let mut obj = jqf_data::ObjectBuilder::new();
            obj.try_insert_last(
                jqf_data::ObjectKey::try_from_str("key").expect("key"),
                number_int(key_val),
            )
            .expect("insert key");
            obj.try_insert_last(jqf_data::ObjectKey::try_from_str("id").expect("key"), number_int(id))
                .expect("insert id");
            arr.try_push(Value::Object(obj.try_finish().expect("obj")))
                .expect("push");
        }
        Value::Array(arr)
    }

    /// Compute `sort_by(.key) | .[0:k]` and return the result as a Value array.
    fn full_sort_then_slice(input: &Value, k: usize, _r: &ResourceContext<'_>) -> Value {
        let Value::Array(arr) = input.untagged() else {
            panic!("not an array");
        };

        // Collect (key, index) pairs
        let mut entries: Vec<(Value, usize)> = Vec::with_capacity(arr.len());
        for (i, elem) in arr.iter().enumerate() {
            let key = match elem.untagged() {
                Value::Object(obj) => obj.get("key").unwrap().clone(),
                _ => elem.clone(),
            };
            entries.push((key, i));
        }

        // Sort by key, then by index (stable)
        entries.sort_by(|a, b| {
            total_cmp(&a.0, &b.0)
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.1.cmp(&b.1))
        });

        let limit = k.min(entries.len());
        let mut result = Array::try_with_capacity(limit).expect("result array");
        for entry in entries.iter().take(limit) {
            result.try_push(arr.get(entry.1).unwrap().clone()).expect("push");
        }
        Value::Array(result)
    }

    /// Compute `sort | .[0:k]` (whole-value comparison)
    fn full_sort_then_slice_by_value(input: &Value, k: usize, _r: &ResourceContext<'_>) -> Value {
        let Value::Array(arr) = input.untagged() else {
            panic!("not an array");
        };

        let mut entries: Vec<(Value, usize)> = Vec::with_capacity(arr.len());
        for (i, elem) in arr.iter().enumerate() {
            entries.push((elem.clone(), i));
        }

        entries.sort_by(|a, b| {
            total_cmp(&a.0, &b.0)
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.1.cmp(&b.1))
        });

        let limit = k.min(entries.len());
        let mut result = Array::try_with_capacity(limit).expect("result array");
        for entry in entries.iter().take(limit) {
            result.try_push(arr.get(entry.1).unwrap().clone()).expect("push");
        }
        Value::Array(result)
    }

    /// Assert two Values are equal by their JSON rendering.
    fn assert_value_eq(left: &Value, right: &Value, context: &str) {
        let left_json = json_string(left);
        let right_json = json_string(right);
        assert_eq!(left_json, right_json, "mismatch: {context}");
    }

    /// Fast JSON string rendering for test assertions.
    fn json_string(value: &Value) -> String {
        use alloc::string::ToString;
        match value.untagged() {
            Value::Null => "null".to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => n.to_integer().map_or_else(
                || {
                    use alloc::format;
                    format!("{n:?}")
                },
                |i| i.as_str().to_string(),
            ),
            Value::String(s) => {
                use alloc::format;
                format!("\"{}\"", s.as_str())
            }
            Value::Array(arr) => {
                let mut s = String::from("[");
                for (i, elem) in arr.iter().enumerate() {
                    if i > 0 {
                        s.push(',');
                    }
                    s.push_str(&json_string(elem));
                }
                s.push(']');
                s
            }
            Value::Object(obj) => {
                let mut keys: Vec<&str> = Vec::new();
                for idx in 0..obj.len() {
                    if let Some(entry) = obj.get_index(idx) {
                        keys.push(entry.key());
                    }
                }
                keys.sort_unstable();
                let mut s = String::from("{");
                for (i, key) in keys.iter().enumerate() {
                    if i > 0 {
                        s.push(',');
                    }
                    s.push('"');
                    s.push_str(key);
                    s.push_str("\":");
                    s.push_str(&json_string(obj.get(key).unwrap()));
                }
                s.push('}');
                s
            }
            _ => {
                use alloc::format;
                format!("{value:?}")
            }
        }
    }

    #[test]
    fn randomized_differential_no_projection() {
        let r = &ledger();
        let mut rng: u64 = 42;
        for trial in 0..200 {
            let size = usize::try_from(next_rand(&mut rng) % 200).expect("size fits usize") + 1;
            // sometimes larger than the array, which is the clamp case
            let k = usize::try_from(next_rand(&mut rng)).expect("k fits usize") % (size + 5);
            let input = random_object_array(size, &mut rng, r);

            // top_k(k) (no projection — sort by whole value)
            let law = TopKDirection::Smallest;
            let count = number_int(i64::try_from(k).expect("k fits i64"));

            let topk_result = top_k_law(law, &input, &count, &[], r).expect("top_k should not fail");

            let fullsort_result = full_sort_then_slice_by_value(&input, k, r);

            assert_value_eq(
                &topk_result,
                &fullsort_result,
                &format!("no-projection trial {trial}: size={size} k={k}"),
            );
        }
    }

    #[test]
    fn randomized_differential_with_projection() {
        let r = &ledger();
        let mut rng: u64 = 99;
        for trial in 0..300 {
            let size = usize::try_from(next_rand(&mut rng) % 150).expect("size fits usize") + 1;
            let k = usize::try_from(next_rand(&mut rng)).expect("k fits usize") % (size + 5);
            let input = random_object_array(size, &mut rng, r);

            // Compute projection keys: .key for each element
            let Value::Array(arr) = input.untagged() else {
                panic!("not an array");
            };
            let mut projection_keys: Vec<Vec<Value>> = Vec::new();
            for idx in 0..arr.len() {
                let elem = arr.get(idx).unwrap();
                let key = match elem.untagged() {
                    Value::Object(obj) => obj.get("key").unwrap().clone(),
                    _ => elem.clone(),
                };
                projection_keys.push(vec![key]);
            }

            let law = TopKDirection::Smallest;
            let count = number_int(i64::try_from(k).expect("k fits i64"));

            let topk_result = top_k_law(law, &input, &count, &projection_keys, r).expect("top_k should not fail");

            let fullsort_result = full_sort_then_slice(&input, k, r);

            assert_value_eq(
                &topk_result,
                &fullsort_result,
                &format!("projection trial {trial}: size={size} k={k}"),
            );
        }
    }

    /// Compute `sort_by(.key) | .[-k:]` and return the result as a Value array.
    fn full_sort_then_tail_slice(input: &Value, k: usize, _r: &ResourceContext<'_>) -> Value {
        let Value::Array(arr) = input.untagged() else {
            panic!("not an array");
        };

        let mut entries: Vec<(Value, usize)> = Vec::with_capacity(arr.len());
        for (i, elem) in arr.iter().enumerate() {
            let key = match elem.untagged() {
                Value::Object(obj) => obj.get("key").unwrap().clone(),
                _ => elem.clone(),
            };
            entries.push((key, i));
        }

        entries.sort_by(|a, b| {
            total_cmp(&a.0, &b.0)
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.1.cmp(&b.1))
        });

        let skip = entries.len().saturating_sub(k);
        let mut result = Array::try_with_capacity(entries.len().saturating_sub(skip)).expect("result array");
        for entry in entries.iter().skip(skip) {
            result.try_push(arr.get(entry.1).unwrap().clone()).expect("push");
        }
        Value::Array(result)
    }

    /// Compute `sort | .[-k:]` (whole-value comparison).
    fn full_sort_then_tail_slice_by_value(input: &Value, k: usize, _r: &ResourceContext<'_>) -> Value {
        let Value::Array(arr) = input.untagged() else {
            panic!("not an array");
        };

        let mut entries: Vec<(Value, usize)> = Vec::with_capacity(arr.len());
        for (i, elem) in arr.iter().enumerate() {
            entries.push((elem.clone(), i));
        }

        entries.sort_by(|a, b| {
            total_cmp(&a.0, &b.0)
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.1.cmp(&b.1))
        });

        let skip = entries.len().saturating_sub(k);
        let mut result = Array::try_with_capacity(entries.len().saturating_sub(skip)).expect("result array");
        for entry in entries.iter().skip(skip) {
            result.try_push(arr.get(entry.1).unwrap().clone()).expect("push");
        }
        Value::Array(result)
    }

    /// The LARGEST direction must reproduce `sort_by(.key) | .[-k:]` exactly, including the tie law: among equal keys
    /// the tail keeps the LATER source elements, which is what makes a tail-slice recognition byte-identical (the
    /// recognizer door).
    #[test]
    fn randomized_differential_largest() {
        let r = &ledger();
        let mut rng: u64 = 7;
        for trial in 0..300 {
            let size = usize::try_from(next_rand(&mut rng) % 150).expect("size fits usize") + 1;
            let k = usize::try_from(next_rand(&mut rng)).expect("k fits usize") % (size + 5);
            let input = random_object_array(size, &mut rng, r);

            // Compute projection keys: .key for each element
            let Value::Array(arr) = input.untagged() else {
                panic!("not an array");
            };
            let mut projection_keys: Vec<Vec<Value>> = Vec::new();
            for idx in 0..arr.len() {
                let elem = arr.get(idx).unwrap();
                let key = match elem.untagged() {
                    Value::Object(obj) => obj.get("key").unwrap().clone(),
                    _ => elem.clone(),
                };
                projection_keys.push(vec![key]);
            }

            let law = TopKDirection::Largest;
            let count = number_int(i64::try_from(k).expect("k fits i64"));

            let topk_result = top_k_law(law, &input, &count, &projection_keys, r).expect("top_k should not fail");

            let fullsort_result = full_sort_then_tail_slice(&input, k, r);

            assert_value_eq(
                &topk_result,
                &fullsort_result,
                &format!("largest projection trial {trial}: size={size} k={k}"),
            );
        }
    }

    /// The LARGEST direction over whole-value comparison, ties included: an all-equal-key array's tail must keep the
    /// LAST k elements, exactly as `sort | .[-k:]` does.
    #[test]
    fn randomized_differential_largest_by_value() {
        let r = &ledger();
        let mut rng: u64 = 77;
        for trial in 0..200 {
            let size = usize::try_from(next_rand(&mut rng) % 150).expect("size fits usize") + 1;
            let k = usize::try_from(next_rand(&mut rng)).expect("k fits usize") % (size + 5);
            let input = random_object_array(size, &mut rng, r);

            let law = TopKDirection::Largest;
            let count = number_int(i64::try_from(k).expect("k fits i64"));

            let topk_result = top_k_law(law, &input, &count, &[], r).expect("top_k should not fail");
            let fullsort_result = full_sort_then_tail_slice_by_value(&input, k, r);

            assert_value_eq(
                &topk_result,
                &fullsort_result,
                &format!("largest whole-value trial {trial}: size={size} k={k}"),
            );
        }
    }

    #[test]
    fn differential_many_equal_keys() {
        let r = &ledger();
        // All elements have the same key: must preserve source order (stable).
        let mut arr = Array::try_new().expect("array");
        for id in 0_i64..100 {
            let mut obj = jqf_data::ObjectBuilder::new();
            obj.try_insert_last(
                jqf_data::ObjectKey::try_from_str("key").expect("key"),
                number_int(42), // ALL same key
            )
            .expect("insert");
            obj.try_insert_last(jqf_data::ObjectKey::try_from_str("id").expect("key"), number_int(id))
                .expect("insert");
            arr.try_push(Value::Object(obj.try_finish().expect("obj")))
                .expect("push");
        }
        let input = Value::Array(arr);

        // All keys are the same
        let projection_keys: Vec<Vec<Value>> = (0..100).map(|_| vec![number_int(42)]).collect();

        let law = TopKDirection::Smallest;
        let count = number_int(10);

        let topk_result = top_k_law(law, &input, &count, &projection_keys, r).expect("top_k should not fail");

        let fullsort_result = full_sort_then_slice(&input, 10, r);

        assert_value_eq(&topk_result, &fullsort_result, "many equal keys");

        // Verify source order preserved: ids 0..9
        let Value::Array(topk_arr) = topk_result.untagged() else {
            panic!("expected array");
        };
        assert_eq!(topk_arr.len(), 10);
        let ids: Vec<i64> = topk_arr
            .iter()
            .map(|v| {
                let Value::Object(obj) = v.untagged() else {
                    panic!("expected object")
                };
                let Value::Number(n) = obj.get("id").unwrap().untagged() else {
                    panic!("expected number")
                };
                n.to_i64().unwrap()
            })
            .collect();
        assert_eq!(ids, (0..10).collect::<Vec<i64>>(), "stable order preserved");
    }

    /// The count follows `.[0:k]`'s END-BOUND law, which is a CEIL over an f64 read — not an integer read. jqf's
    /// `2.7` is an exact DECIMAL, so an `as_integer()` view of it is `None`; reading it that way and folding the `None`
    /// to zero answered `[]` for every fractional count.
    #[test]
    fn a_fractional_count_ceils_the_way_the_slice_end_bound_does() {
        assert_eq!(resolve_count(&number_dec("2.7")).expect("2.7"), 3);
        assert_eq!(resolve_count(&number_dec("3.0")).expect("3.0"), 3);
        assert_eq!(resolve_count(&number_dec("0.1")).expect("0.1"), 1);
        assert_eq!(resolve_count(&number_int(3)).expect("3"), 3);
        assert_eq!(resolve_count(&number_int(0)).expect("0"), 0);
    }

    /// The two deliberate departures from `.[0:k]`: a negative count is empty rather than len-relative, and a
    /// non-number count RAISES the slice-index error rather than an internal-contract violation — it is a user error
    /// about the argument, not an engine bug.
    #[test]
    fn a_negative_count_is_empty_and_a_non_number_count_raises() {
        assert_eq!(resolve_count(&number_int(-2)).expect("-2"), 0);
        for bad in [Value::Null, Value::Bool(true), array_of_ints(&[1])] {
            assert!(
                matches!(resolve_count(&bad), Err(EngineRunError::SliceIndices)),
                "a non-number count must raise the slice-index error, not an internal contract"
            );
        }
    }
}
