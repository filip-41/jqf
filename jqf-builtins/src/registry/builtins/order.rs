//! Ordering builtins: `sort`, `sort_by`, `group_by`, `unique`, `unique_by`, `min`, `max`, `min_by`, `max_by`,
//! `reverse`, `bsearch`.
//!
//! One job: own the family and overload records for the vocabulary that ORDERS an array, plus the evaluator payloads
//! for the four whole-element forms and for the two that take no keys at all. The five `_by` forms drive a filter once
//! per element, so their payload is a frame in [`crate::exec`]; what they share with the four forms here is
//! [`crate::semantics::keyed`]'s table, which is where the tie law and the empty-input law live for all nine.
//!
//! **The arity-0 forms are not their `_by` twins with an identity key.** They agree on ORDER and disagree on every
//! rejection, which is the whole reason they are separate evaluators:
//!
//! ```text
//! {"a":1} | sort       # object ({"a":1}) cannot be sorted, as it is not an array
//! {"a":1} | sort_by(.) # object ({"a":1}) and array ([[1]]) cannot be sorted, …
//! {"a":1} | min        # object ({"a":1}) and object ({"a":1}) cannot be iterated over
//! {"a":1} | min_by(.)  # object ({"a":1}) and array ([[1]]) cannot be iterated over
//! null    | sort_by(.) # Cannot iterate over null (null)
//! ```
//!
//! The reference minimum hands its input to the two-operand implementation as BOTH the values and the keys, which is
//! why `min` names its operand twice; the reference sort is single-operand; and the `_by` forms reach the two-operand
//! implementation through `map([f])`, whose own iteration is what rejects a scalar first. Four message classes, and no
//! two of them are derivable from each other.
//!
//! `reverse` is not a reversal primitive: the reference defines it as `[.[length - 1 - range(0; length)]]`, so its
//! errors are LENGTH and INDEX errors — `5 | reverse` says `Cannot index number with number (4)` because `5 | length`
//! is `5`. This module reproduces that spelling's observable behaviour without needing `range`.
//!
//! Negative space: it renders no message (that is [`crate::error::message`]), owns no tie law (that is
//! [`crate::semantics::keyed`]), and drives no filter (that is the executor's keyed frame).

use alloc::string::String;

use jqf_data::{Array, Integer, Number, Value};
use jqf_resource::ResourceContext;

use super::id;
use crate::codec_result::EngineResult;
use crate::error::EngineRunError;
use crate::error::message;
use crate::registry::record::{
    BuiltinExample, BuiltinExecution, BuiltinFamilyId, BuiltinFamilyRecord, BuiltinOverloadId, BuiltinOverloadRecord,
    DemandTransfer, Effects, ParameterKind, SemanticRevision,
};
use crate::semantics::depth::comparison_error;
use crate::semantics::keyed::{KeyMode, publish, try_entries};
use crate::semantics::order::{observable_cmp, total_cmp};
use crate::semantics::path::raise;

/// The ordering family records.
pub const FAMILIES: &[BuiltinFamilyRecord] = &[
    SORT_FAMILY,
    SORT_BY_FAMILY,
    GROUP_BY_FAMILY,
    UNIQUE_FAMILY,
    UNIQUE_BY_FAMILY,
    MIN_FAMILY,
    MAX_FAMILY,
    MIN_BY_FAMILY,
    MAX_BY_FAMILY,
    REVERSE_FAMILY,
    BSEARCH_FAMILY,
];

/// The ordering overload records.
pub const OVERLOADS: &[BuiltinOverloadRecord] = &[
    SORT_OVERLOAD,
    SORT_BY_OVERLOAD,
    GROUP_BY_OVERLOAD,
    UNIQUE_OVERLOAD,
    UNIQUE_BY_OVERLOAD,
    MIN_OVERLOAD,
    MAX_OVERLOAD,
    MIN_BY_OVERLOAD,
    MAX_BY_OVERLOAD,
    REVERSE_OVERLOAD,
    BSEARCH_OVERLOAD,
];

/// The ordering execution payloads, aligned one-to-one with [`OVERLOADS`].
///
/// The family spans three shapes (whole-form, keyed, and the two standalone laws), so it names its own payload enum;
/// `registry::dispatch` wraps it at table build time. Every entry carries its overload id so the const coverage walk
/// there can prove pairwise alignment.
#[derive(Clone, Copy, Debug)]
pub enum OrderPayload {
    /// An arity-0 ordering form — the element IS the key.
    Whole(WholeForm),
    /// A `_by` ordering form — a keyed consumer frame.
    Keyed(KeyMode),
    /// `reverse/0` — the length-and-index reversal.
    Reverse,
    /// `bsearch/1` — the sorted-array binary search.
    BSearch,
}

pub const PAYLOADS: &[(u16, OrderPayload)] = &[
    (id::SORT, OrderPayload::Whole(WholeForm::Sort)),
    (id::SORT_BY, OrderPayload::Keyed(KeyMode::Sort)),
    (id::GROUP_BY, OrderPayload::Keyed(KeyMode::Group)),
    (id::UNIQUE, OrderPayload::Whole(WholeForm::Unique)),
    (id::UNIQUE_BY, OrderPayload::Keyed(KeyMode::Unique)),
    (id::MIN, OrderPayload::Whole(WholeForm::Min)),
    (id::MAX, OrderPayload::Whole(WholeForm::Max)),
    (id::MIN_BY, OrderPayload::Keyed(KeyMode::Min)),
    (id::MAX_BY, OrderPayload::Keyed(KeyMode::Max)),
    (id::REVERSE, OrderPayload::Reverse),
    (id::BSEARCH, OrderPayload::BSearch),
];

/// The four ordering forms that take NO key filter.
///
/// Each names the [`KeyMode`] whose order it publishes and the rejection class its own arity-0 spelling carries, which
/// is never the `_by` form's.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WholeForm {
    /// `sort/0`.
    Sort,
    /// `unique/0`.
    Unique,
    /// `min/0`.
    Min,
    /// `max/0`.
    Max,
}

impl WholeForm {
    /// The keyed mode this form publishes, with the element itself as its key.
    const fn mode(self) -> KeyMode {
        match self {
            Self::Sort => KeyMode::Sort,
            Self::Unique => KeyMode::Unique,
            Self::Min => KeyMode::Min,
            Self::Max => KeyMode::Max,
        }
    }
}

/// Evaluates one arity-0 ordering form over an owned input.
///
/// The keys ARE the elements: `sort` orders by whole values, and the reference minimum literally passes its input
/// twice. That is order-equivalent to the `_by` forms' one-element boxes (`[a]` against `[b]` is `a` against `b`) and
/// saves the box, which is the whole point of not routing these through the frame.
///
/// # Errors
///
/// Returns the form's own rejection for a non-array input, or an allocation failure.
pub fn whole(form: WholeForm, input: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let Value::Array(array) = input.untagged() else {
        return Err(reject_whole(form, input, resources));
    };
    let mut entries = try_entries(array)?;
    // The unkeyed forms collect their entries here, in memory: the spill's flush point is the KEYED frame (where the
    // derived keys are produced), and `sort`'s keys are shared element clones the spill cannot reduce.
    // The runs are always empty on this path.
    publish(form.mode(), &mut entries, array, &[], resources)
}

/// The rejection one arity-0 form raises for a non-array input.
fn reject_whole(form: WholeForm, input: &Value, resources: &ResourceContext<'_>) -> EngineRunError {
    let render = || -> Result<String, EngineRunError> {
        let operand = message::dump_trunc_owned(input)?;
        let kind = input.kind();
        match form {
            WholeForm::Sort | WholeForm::Unique => message::not_sortable_message(kind, &operand),
            // The reference minimum/maximum hand the SAME value to the two-operand implementation as both the values
            // and the keys, so the message names it twice.
            WholeForm::Min | WholeForm::Max => message::not_iterable_pair_message(kind, &operand, kind, &operand),
        }
    };
    match render() {
        Ok(text) => raise(&text, resources),
        Err(error) => error,
    }
}

/// Evaluates `reverse` over an owned input.
///
/// An ARRAY reverses. Everything else goes through the reference spelling: take the input's `length`, and — when it
/// is non-zero — index the input with `length - 1`, which is what raises. A zero length means `range(0; 0)` is empty
/// and the answer is `[]`, which is why `null`, `""`, `{}` and `0` all reverse to the empty array instead of erroring.
///
/// # Errors
///
/// Returns the no-length error for a boolean, the index error for any other non-array with a non-zero length, or an
/// allocation failure.
pub fn reverse(input: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    if let Value::Array(array) = input.untagged() {
        let mut out = Array::try_with_capacity(array.len()).map_err(|_| EngineRunError::allocation_failure())?;
        for position in (0..array.len()).rev() {
            let element = array
                .get(position)
                .ok_or_else(|| EngineRunError::internal_contract("array shorter than its length"))?
                .clone();
            out.try_push(element)
                .map_err(|_| EngineRunError::allocation_failure())?;
        }
        return Ok(Value::Array(out));
    }
    let length = length_of(input, resources)?;
    // The null-as-empty-container cell's reverse half: `null | reverse` answers `[]`. The other zero-length inputs
    // (`0`, `""`, `{}`, `[]`) are the reference's DEFINED reverse law and are deliberately not cells.
    if matches!(input.untagged(), Value::Null) {
        crate::error::mismatch::resolve_at(
            resources,
            crate::error::mismatch::MismatchCell::NullAsEmptyContainer,
            false,
            (),
        )?;
    }
    if is_zero(&length) {
        return Ok(Value::Array(
            Array::try_new().map_err(|_| EngineRunError::allocation_failure())?,
        ));
    }
    let first = crate::semantics::binary::apply(
        crate::semantics::binary::BinaryKind::Subtract,
        &length,
        &Value::Number(integer(1)),
        resources,
    )
    .map_err(|_| EngineRunError::internal_contract("a length that cannot be decremented"))?;
    let accessor = message::render_owned_key(&first)?;
    let text = message::index_message(input.kind(), &accessor)?;
    Err(raise(&text, resources))
}

/// Evaluates `bsearch`'s pure half: the position of `target` in a SORTED array, or `-1 - insertion_point` when it is
/// absent.
///
/// The search is inclusive-bounds over the total order, with the midpoint the UPPER-middle of the slice (`low + (high -
/// low + 1) / 2`), which is what makes a run of equal elements answer with the midpoint the reference walk lands on
/// rather than with the run's first member.
///
/// It reads the OBSERVABLE law because the reference `bsearch` is written in the language itself, over the `>` and `==`
/// OPERATORS: a NaN target is therefore never found and always bisects left, so `[nan] | bsearch(nan)` is `-1` and
/// `[nan] | bsearch(1)` is `-2`. Under the internal law the first would wrongly answer `0`.
///
/// # Errors
///
/// Returns the `cannot be searched from` error for a non-array subject, or an allocation failure.
pub fn bsearch(subject: &Value, target: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let Value::Array(array) = subject.untagged() else {
        let operand = message::dump_trunc_owned(subject)?;
        let text = message::not_searchable_message(subject.kind(), &operand)?;
        return Err(raise(&text, resources));
    };
    let mut low: i64 = 0;
    let mut high =
        i64::try_from(array.len()).map_err(|_| EngineRunError::internal_contract("an array longer than i64"))? - 1;
    while low <= high {
        let middle = low + (high - low + 1) / 2;
        let at =
            usize::try_from(middle).map_err(|_| EngineRunError::internal_contract("a negative search midpoint"))?;
        let element = array
            .get(at)
            .ok_or_else(|| EngineRunError::internal_contract("a midpoint outside the array"))?;
        // TARGET on the left, as every comparison in the reference definition spells it (`target > .[0]`). The
        // orientation is unobservable for any pair the order ranks symmetrically and decides the whole NaN family: a
        // NaN target is `Less` than every element INCLUDING a NaN one, so the walk only ever goes left and the
        // insertion point stays 0.
        match observable_cmp(target, element).map_err(|_| comparison_error(resources))? {
            core::cmp::Ordering::Equal => return Ok(Value::Number(integer(middle))),
            core::cmp::Ordering::Greater => low = middle + 1,
            core::cmp::Ordering::Less => high = middle - 1,
        }
    }
    Ok(Value::Number(integer(-1 - low)))
}

/// One value's `length`, as an owned number.
fn length_of(input: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let cloned = input.clone();
    let owned = EngineResult::owned(cloned);
    super::core::length(&owned, resources)
}

/// Whether a length is exactly zero, in any of the number representations.
///
/// Two numbers compare at depth 0, so the comparison row can never trip here and a non-`Ok` answer would be a broken
/// invariant rather than a program's depth; it reads as "not zero", which is the conservative direction.
fn is_zero(length: &Value) -> bool {
    matches!(length, Value::Number(number) if total_cmp(
        &Value::Number(number.clone()),
        &Value::Number(integer(0)),
    )
    .is_ok_and(core::cmp::Ordering::is_eq))
}

/// One machine integer as an owned number.
fn integer(value: i64) -> Number {
    Number::integer(Integer::from_i64(value))
}

const SORT_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::SORT),
    canonical_name: "sort",
    category: "order",
    summary: "The input array's elements in ascending total order.",
    detail: "The order is the total order over all six kinds \
             (`null < false < true < numbers < strings < arrays < objects`), so \
             a mixed array sorts without erroring. A non-array input is \
             `<kind> (<v>) cannot be sorted, as it is not an array` — the \
             SINGLE-operand class, which `sort_by` does not share.",
};

const SORT_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::SORT),
    family: BuiltinFamilyId::new(id::SORT),
    canonical_name: "sort",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "sort",
            input: "[3,1,2]",
            expected: "[1,2,3]\n",
        },
        BuiltinExample {
            program: "sort",
            input: "[{\"a\":1},[1],\"s\",1,true,false,null]",
            expected: "[null,false,true,1,\"s\",[1],{\"a\":1}]\n",
        },
    ],
};

const SORT_BY_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::SORT_BY),
    canonical_name: "sort_by",
    category: "order",
    summary: "The input array's elements in ascending order of a key filter.",
    detail: "The key is the ARRAY of ALL the filter's outputs for that element, \
             which is why `sort_by(.a, .b)` is a lexicographic two-level sort \
             and `sort_by(empty)` is a stable no-op. The sort is STABLE: equal \
             keys keep input order.",
};

const SORT_BY_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::SORT_BY),
    family: BuiltinFamilyId::new(id::SORT_BY),
    canonical_name: "sort_by",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "sort_by(.a)",
            input: "[{\"a\":2,\"i\":0},{\"a\":1,\"i\":1},{\"a\":2,\"i\":2}]",
            expected: "[{\"a\":1,\"i\":1},{\"a\":2,\"i\":0},{\"a\":2,\"i\":2}]\n",
        },
        BuiltinExample {
            program: "sort_by(.a,.b)",
            input: "[{\"a\":1,\"b\":2},{\"a\":1,\"b\":1}]",
            expected: "[{\"a\":1,\"b\":1},{\"a\":1,\"b\":2}]\n",
        },
    ],
};

const GROUP_BY_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::GROUP_BY),
    canonical_name: "group_by",
    category: "order",
    summary: "The input array partitioned into equal-key runs, in key order.",
    detail: "Groups are published in ascending KEY order and each group's \
             members stay in INPUT order, both of which follow from the sort \
             being stable. An empty input is `[]`, never `[[]]`.",
};

const GROUP_BY_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::GROUP_BY),
    family: BuiltinFamilyId::new(id::GROUP_BY),
    canonical_name: "group_by",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "group_by(.a)",
            input: "[{\"a\":2,\"i\":0},{\"a\":1,\"i\":1},{\"a\":2,\"i\":2}]",
            expected: "[[{\"a\":1,\"i\":1}],[{\"a\":2,\"i\":0},{\"a\":2,\"i\":2}]]\n",
        },
        BuiltinExample {
            program: "group_by(.)",
            input: "[]",
            expected: "[]\n",
        },
        // A MULTI-VALUED key filter groups by the whole key ARRAY.
        BuiltinExample {
            program: "group_by(.a, .b)",
            input: "[{\"a\":1,\"b\":1},{\"a\":1,\"b\":2},{\"a\":2,\"b\":1}]",
            expected: "[[{\"a\":1,\"b\":1}],[{\"a\":1,\"b\":2}],[{\"a\":2,\"b\":1}]]\n",
        },
    ],
};

const UNIQUE_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::UNIQUE),
    canonical_name: "unique",
    category: "order",
    summary: "The input array's distinct elements, sorted.",
    detail: "Distinctness is the reference's own equality, which compares numbers by \
             VALUE, so `[1, 1.0, \"1\"]` is `[1, \"1\"]`. The rejection class \
             is `sort`'s single-operand one, not `unique_by`'s doubled one.",
};

const UNIQUE_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::UNIQUE),
    family: BuiltinFamilyId::new(id::UNIQUE),
    canonical_name: "unique",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "unique",
            input: "[3,1,3,2,1]",
            expected: "[1,2,3]\n",
        },
        BuiltinExample {
            program: "unique",
            input: "[1,1.0,\"1\"]",
            expected: "[1,\"1\"]\n",
        },
    ],
};

const UNIQUE_BY_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::UNIQUE_BY),
    canonical_name: "unique_by",
    category: "order",
    summary: "The FIRST element of each equal-key run, in key order.",
    detail: "It keeps the first element of each run in the STABLE sorted order, \
             which means the surviving element is the one that appeared \
             earliest in the input among those sharing its key.",
};

const UNIQUE_BY_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::UNIQUE_BY),
    family: BuiltinFamilyId::new(id::UNIQUE_BY),
    canonical_name: "unique_by",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "unique_by(length)",
            input: "[\"a\",\"bb\",\"cc\",\"d\"]",
            expected: "[\"a\",\"bb\"]\n",
        },
        BuiltinExample {
            program: "unique_by(.a)",
            input: "[{\"a\":1,\"i\":0},{\"a\":1,\"i\":1}]",
            expected: "[{\"a\":1,\"i\":0}]\n",
        },
        // A MULTI-VALUED key filter compares the whole key ARRAY, so these two elements are distinct even though their
        // first key agrees.
        BuiltinExample {
            program: "unique_by(.a, .b)",
            input: "[{\"a\":1,\"b\":1},{\"a\":1,\"b\":2}]",
            expected: "[{\"a\":1,\"b\":1},{\"a\":1,\"b\":2}]\n",
        },
    ],
};

const MIN_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::MIN),
    canonical_name: "min",
    category: "order",
    summary: "The smallest element of the input array, or `null` when empty.",
    detail: "`null` for an empty array is the answer, not an error. A non-array \
             input is the DOUBLED iteration message with the same value named \
             twice, because the reference implementation passes its input as both the \
             values and the keys.",
};

const MIN_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::MIN),
    family: BuiltinFamilyId::new(id::MIN),
    canonical_name: "min",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "min",
            input: "[3,1,2]",
            expected: "1\n",
        },
        BuiltinExample {
            program: "min",
            input: "[]",
            expected: "null\n",
        },
    ],
};

const MAX_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::MAX),
    canonical_name: "max",
    category: "order",
    summary: "The largest element of the input array, or `null` when empty.",
    detail: "The mirror of `min`, with the same `null`-for-empty answer and the \
             same doubled rejection message.",
};

const MAX_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::MAX),
    family: BuiltinFamilyId::new(id::MAX),
    canonical_name: "max",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "max",
            input: "[3,1,2]",
            expected: "3\n",
        },
        BuiltinExample {
            program: "max",
            input: "[]",
            expected: "null\n",
        },
    ],
};

const MIN_BY_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::MIN_BY),
    canonical_name: "min_by",
    category: "order",
    summary: "The FIRST element whose key is smallest, or `null` when empty.",
    detail: "FIRST-wins on a tie, which is NOT the mirror of `max_by`'s \
             LAST-wins. One comparison predicate shared between the two gets \
             exactly one of them wrong.",
};

const MIN_BY_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::MIN_BY),
    family: BuiltinFamilyId::new(id::MIN_BY),
    canonical_name: "min_by",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        // The smallest key is NOT on the first element, so a key filter that is evaluated and discarded answers
        // `{"a":2,"i":0}` here; the tie between the last two elements still pins FIRST-wins.
        BuiltinExample {
            program: "min_by(.a)",
            input: "[{\"a\":2,\"i\":0},{\"a\":1,\"i\":1},{\"a\":1,\"i\":2}]",
            expected: "{\"a\":1,\"i\":1}\n",
        },
        BuiltinExample {
            program: "min_by(.a)",
            input: "[]",
            expected: "null\n",
        },
        // A MULTI-VALUED key filter compares the whole key ARRAY.
        BuiltinExample {
            program: "min_by(.a, .b)",
            input: "[{\"a\":1,\"b\":2},{\"a\":1,\"b\":1}]",
            expected: "{\"a\":1,\"b\":1}\n",
        },
    ],
};

const MAX_BY_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::MAX_BY),
    canonical_name: "max_by",
    category: "order",
    summary: "The LAST element whose key is largest, or `null` when empty.",
    detail: "LAST-wins on a tie, the asymmetric twin of `min_by`'s FIRST-wins.",
};

const MAX_BY_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::MAX_BY),
    family: BuiltinFamilyId::new(id::MAX_BY),
    canonical_name: "max_by",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "max_by(.a)",
            input: "[{\"a\":2,\"i\":0},{\"a\":2,\"i\":1},{\"a\":1,\"i\":2}]",
            expected: "{\"a\":2,\"i\":1}\n",
        },
        BuiltinExample {
            program: "max_by(.a)",
            input: "[]",
            expected: "null\n",
        },
        // A MULTI-VALUED key filter compares the whole key ARRAY.
        BuiltinExample {
            program: "max_by(.a, .b)",
            input: "[{\"a\":1,\"b\":2},{\"a\":1,\"b\":1}]",
            expected: "{\"a\":1,\"b\":2}\n",
        },
    ],
};

const REVERSE_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::REVERSE),
    canonical_name: "reverse",
    category: "order",
    summary: "The input array's elements in reverse position order.",
    detail: "The reference defines it as `[.[length - 1 - range(0; length)]]`, and the \
             definition shows through: a zero-length input (`null`, `\"\"`, \
             `{}`, `0`) reverses to `[]`, a boolean has no length, and every \
             other non-array is an INDEX error naming `length - 1`.",
};

const REVERSE_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::REVERSE),
    family: BuiltinFamilyId::new(id::REVERSE),
    canonical_name: "reverse",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "reverse",
            input: "[1,2,3]",
            expected: "[3,2,1]\n",
        },
        BuiltinExample {
            program: "reverse",
            input: "null",
            expected: "[]\n",
        },
    ],
};

const BSEARCH_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::BSEARCH),
    canonical_name: "bsearch",
    category: "order",
    summary: "The index of a value in a SORTED array, or `-1 - insertion_point`.",
    detail: "A hit answers with the index the binary search landed on; a miss \
             answers with `-1 - i` where `i` is where the value would be \
             inserted, so a miss is always negative and a hit never is. The \
             argument is an ordinary filter: every output it produces is \
             searched for separately.",
};

const BSEARCH_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::BSEARCH),
    family: BuiltinFamilyId::new(id::BSEARCH),
    canonical_name: "bsearch",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "bsearch(2)",
            input: "[1,2,3]",
            expected: "1\n",
        },
        BuiltinExample {
            program: "bsearch(4)",
            input: "[1,2,3]",
            expected: "-4\n",
        },
    ],
};

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;
    use jqf_resource::{ContinueControl, RequestAccount, ResourceLimits, WorkMeter};

    fn resources() -> ResourceContext<'static> {
        ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                .expect("account"),
            &ContinueControl,
            WorkMeter::try_new_v1(4_096).expect("work"),
        )
        .expect("resources")
    }

    fn number(value: i64) -> Value {
        Value::Number(integer(value))
    }

    fn array(items: Vec<i64>) -> Value {
        let _resources = resources();
        let items = items.into_iter().map(number).collect();
        Value::Array(Array::try_from_vec(items).expect("array"))
    }

    /// The reference walk pivots on the slice's UPPER-middle, so an even run of equal elements answers one index higher
    /// than a floored midpoint would.
    #[test]
    fn bsearch_even_run_of_equals_lands_on_the_upper_middle() {
        let cases: &[(&[i64], i64)] = &[
            (&[1, 1], 1),
            (&[1, 1, 1, 1], 2),
            (&[0, 1, 1, 1], 2),
            (&[1, 1, 1, 1, 1, 1], 3),
        ];
        for &(subject, expected) in cases {
            let resources = resources();
            let out = bsearch(&array(subject.to_vec()), &number(1), &resources).expect("bsearch");
            let got = match out.untagged() {
                Value::Number(n) => n.to_i64().expect("integer answer"),
                _ => panic!("bsearch returned a non-number"),
            };
            assert_eq!(got, expected, "bsearch over {subject:?}");
        }
    }
}
