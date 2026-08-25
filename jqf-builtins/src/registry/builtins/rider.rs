//! The scalar-tails misc riders: `builtins/0`, `have_decnum/0`, and `debug/0`/`debug/1`.
//!
//! One job: own the family/overload records AND the per-law evaluators the executor dispatches through.
//!
//! - `builtins/0` enumerates the registry truthfully — every registered
//!   `name/arity` string, sorted — an overload the registry owns but `builtins` omits would silently lie about parity,
//!   and an overload it lists but cannot resolve would lie the other way. The enumeration is the registry's own, so the
//!   two cannot drift.
//! - `have_decnum/0` answers `true`: jqf's exact number model is the
//!   exact-decimal branch of the reference's `if have_decnum then …` spellings (the suite's decnum expectations are
//!   exactly the digits jqf renders).
//! - `debug/0` and `debug/1` pass the piped value through unchanged (the stdout law) AND write
//!   `["DEBUG:",<compact value>]` plus a newline to the host's stderr channel per message, which is the builtin's whole
//!   point: the stdout law is a no-op, so a stdout-diffing lane cannot see the builtin work at all. `debug/1` is the
//!   reference's `(msgs | debug | empty), .`, so its argument iterates — one line per output — and the compat corpus
//!   pins every line as a `stderrparity` row against the live reference.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use jqf_data::{Array, ScalarView, Value};
use jqf_resource::ResourceContext;

use super::id;
use crate::codec_result::EngineResult;
use crate::error::EngineRunError;
use crate::registry::record::{
    BuiltinExample, BuiltinExecution, BuiltinFamilyId, BuiltinFamilyRecord, BuiltinOverloadId, BuiltinOverloadRecord,
    DemandTransfer, Effects, ParameterKind, SemanticRevision,
};
use crate::semantics::order;

/// The rider law discriminants.
#[derive(Clone, Copy, Debug)]
pub enum RiderLaw {
    Builtins,
    HaveDecnum,
    /// `finites/0` — `def finites: select(isfinite);`.
    Finites,
    /// `normals/0` — `def normals: select(isnormal);`.
    Normals,
    /// `have_literal_numbers/0` — always `true`.
    HaveLiteralNumbers,
    Debug,
}

/// The evaluator payload the registry dispatches for every rider overload.
#[derive(Clone, Copy, Debug)]
pub enum RiderEvaluator {
    Unary(RiderLaw),
    /// `debug/1`: like the /0 form, but the MESSAGE is each output of the argument filter rather than the piped value.
    DebugOne,
}

/// `builtins/0`: every registered `name/arity` string, sorted.
pub fn builtins(_resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let mut names: Vec<String> = crate::registry::builtin_overloads()
        .iter()
        // The reference's own `builtins` hides its underscore-prefixed internals (`_negate`, `_strindices` — the
        // reference answers 226 entries with none starting `_`), and jqf's truthful enumeration follows.
        .filter(|overload| !overload.canonical_name.starts_with('_'))
        .map(|overload| format!("{}/{}", overload.canonical_name, overload.arity))
        .collect();
    // The stdlib-prelude-backed names (`all`, `any`, `isempty`, `first/0`, `last`, `values`, `nulls`) and the `empty/0`
    // syntax form join the registry's own enumeration, so `builtins` answers the reference's full surface.
    for (name, arity) in crate::registry::PRELUDE_ENUMERATED {
        names.push(format!("{name}/{arity}"));
    }
    names.sort();
    let mut array = Array::try_new().map_err(|_| EngineRunError::allocation_failure())?;
    for name in names {
        let value = Value::try_string(&name).map_err(|_| EngineRunError::allocation_failure())?;
        array
            .try_push(value)
            .map_err(|_| EngineRunError::allocation_failure())?;
    }
    Ok(Value::Array(array))
}

/// `have_decnum/0`: jqf's number model is the exact decimal one the reference's `have_decnum` branch spells.
pub fn have_decnum() -> Value {
    Value::Bool(true)
}

/// Whether `input` passes the `finites`/`normals` f64 predicate WITHOUT materializing a located value: an owned number
/// reads directly, a located number reads its borrowed `NumberView`, and every other kind answers `false` (`"x" |
/// finites` is empty, not an error).
pub fn result_number_passes(input: &EngineResult<'_>, law: RiderLaw) -> Result<bool, EngineRunError> {
    match input {
        EngineResult::Owned(value) => {
            let Value::Number(number) = value.untagged() else {
                return Ok(false);
            };
            let value = order::to_f64(number);
            Ok(match law {
                // The reference's `isfinite` is `isinfinite | not`, so NaN is "finite": `nan | finites` prints `null`.
                // Only the two infinities drop.
                RiderLaw::Finites => !value.is_infinite(),
                RiderLaw::Normals => value.is_normal(),
                _ => false,
            })
        }
        EngineResult::Located(located) => {
            // The payload-transparent view: a tag LAYER is descended before the node is asked for its scalar, so a
            // tagged number passes the same f64 law its payload does.
            let view = located
                .product()
                .document()
                .payload_view(located.node())
                .map_err(|_| EngineRunError::internal_contract("number filter view failed"))?;
            let Some(scalar) = view
                .scalar()
                .map_err(|_| EngineRunError::internal_contract("number filter scalar failed"))?
            else {
                return Ok(false);
            };
            match scalar {
                ScalarView::Number(number) => Ok(match law {
                    RiderLaw::Finites => !crate::semantics::order::view_to_f64(number).is_infinite(),
                    RiderLaw::Normals => crate::semantics::order::view_to_f64(number).is_normal(),
                    _ => false,
                }),
                _ => Ok(false),
            }
        }
    }
}

// ------------------------------------------------------------------------
// Registry records.

macro_rules! family {
    ($id:expr, $canonical_name:literal) => {
        BuiltinFamilyRecord {
            id: BuiltinFamilyId::new($id),
            canonical_name: $canonical_name,
            category: "rider",
            summary: concat!("The ", $canonical_name, " family."),
            detail: "",
        }
    };
}

const BUILTINS_FAMILY: BuiltinFamilyRecord = family!(id::BUILTINS_FAMILY_ID, "builtins");
const HAVE_DECNUM_FAMILY: BuiltinFamilyRecord = family!(id::HAVE_DECNUM_FAMILY_ID, "have_decnum");
const FINITES_FAMILY: BuiltinFamilyRecord = family!(id::FINITES_FAMILY_ID, "finites");
const NORMALS_FAMILY: BuiltinFamilyRecord = family!(id::NORMALS_FAMILY_ID, "normals");
const HAVE_LITERAL_NUMBERS_FAMILY: BuiltinFamilyRecord =
    family!(id::HAVE_LITERAL_NUMBERS_FAMILY_ID, "have_literal_numbers");
const DEBUG_FAMILY: BuiltinFamilyRecord = family!(id::DEBUG_FAMILY_ID, "debug");

pub const FAMILIES: &[BuiltinFamilyRecord] = &[
    BUILTINS_FAMILY,
    HAVE_DECNUM_FAMILY,
    FINITES_FAMILY,
    NORMALS_FAMILY,
    HAVE_LITERAL_NUMBERS_FAMILY,
    DEBUG_FAMILY,
];

const ONE_FILTER: &[ParameterKind] = &[ParameterKind::Filter];

const BUILTINS_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::BUILTINS),
    family: BuiltinFamilyId::new(id::BUILTINS_FAMILY_ID),
    canonical_name: "builtins",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[BuiltinExample {
        program: "[builtins | length > 10]",
        input: "null",
        expected: "[true]\n",
    }],
};

const HAVE_DECNUM_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::HAVE_DECNUM),
    family: BuiltinFamilyId::new(id::HAVE_DECNUM_FAMILY_ID),
    canonical_name: "have_decnum",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[BuiltinExample {
        program: "have_decnum",
        input: "null",
        expected: "true\n",
    }],
};

/// One of the two number-filter overloads (`finites`, `normals`): a zero-or-one filter that routes the input through
/// unchanged when the f64 predicate admits it, exactly like the kind filters.
const fn number_filter_overload(
    overload_id: u16,
    family_id: u16,
    name: &'static str,
    examples: &'static [BuiltinExample],
) -> BuiltinOverloadRecord {
    BuiltinOverloadRecord {
        id: BuiltinOverloadId::new(overload_id),
        family: BuiltinFamilyId::new(family_id),
        canonical_name: name,
        arity: 0,
        parameters: &[],
        execution: BuiltinExecution::Evaluator,
        demand_transfer: DemandTransfer::Subtree,
        semantic_revision: SemanticRevision::new(1),
        effects: Effects::Pure,
        examples,
    }
}

const FINITES_EXAMPLES: &[BuiltinExample] = &[BuiltinExample {
    program: "finites",
    input: "1.5",
    expected: "1.5\n",
}];
const NORMALS_EXAMPLES: &[BuiltinExample] = &[BuiltinExample {
    program: "normals",
    input: "1.5",
    expected: "1.5\n",
}];

const FINITES_OVERLOAD: BuiltinOverloadRecord =
    number_filter_overload(id::FINITES, id::FINITES_FAMILY_ID, "finites", FINITES_EXAMPLES);
const NORMALS_OVERLOAD: BuiltinOverloadRecord =
    number_filter_overload(id::NORMALS, id::NORMALS_FAMILY_ID, "normals", NORMALS_EXAMPLES);

const HAVE_LITERAL_NUMBERS_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::HAVE_LITERAL_NUMBERS),
    family: BuiltinFamilyId::new(id::HAVE_LITERAL_NUMBERS_FAMILY_ID),
    canonical_name: "have_literal_numbers",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[BuiltinExample {
        program: "have_literal_numbers",
        input: "null",
        expected: "true\n",
    }],
};

/// Both `debug` arities declare `Impure`, for the stderr effect the reference's own definition has and jqf now performs
/// through the host's stderr channel.
const DEBUG_0_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::DEBUG_0),
    family: BuiltinFamilyId::new(id::DEBUG_FAMILY_ID),
    canonical_name: "debug",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Impure,
    examples: &[BuiltinExample {
        program: "debug",
        input: "1",
        expected: "1\n",
    }],
};

const DEBUG_1_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::DEBUG_1),
    family: BuiltinFamilyId::new(id::DEBUG_FAMILY_ID),
    canonical_name: "debug",
    arity: 1,
    parameters: ONE_FILTER,
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Impure,
    examples: &[BuiltinExample {
        program: "debug(\"x\")",
        input: "1",
        expected: "1\n",
    }],
};

pub const OVERLOADS: &[BuiltinOverloadRecord] = &[
    BUILTINS_OVERLOAD,
    HAVE_DECNUM_OVERLOAD,
    FINITES_OVERLOAD,
    NORMALS_OVERLOAD,
    HAVE_LITERAL_NUMBERS_OVERLOAD,
    DEBUG_0_OVERLOAD,
    DEBUG_1_OVERLOAD,
];

/// The misc-rider execution payloads, aligned one-to-one with [`OVERLOADS`].
///
/// Every entry carries its overload id so the const coverage walk in `registry::dispatch` can prove pairwise alignment.
pub const PAYLOADS: &[(u16, RiderEvaluator)] = &[
    (id::BUILTINS, RiderEvaluator::Unary(RiderLaw::Builtins)),
    (id::HAVE_DECNUM, RiderEvaluator::Unary(RiderLaw::HaveDecnum)),
    (id::FINITES, RiderEvaluator::Unary(RiderLaw::Finites)),
    (id::NORMALS, RiderEvaluator::Unary(RiderLaw::Normals)),
    (
        id::HAVE_LITERAL_NUMBERS,
        RiderEvaluator::Unary(RiderLaw::HaveLiteralNumbers),
    ),
    (id::DEBUG_0, RiderEvaluator::Unary(RiderLaw::Debug)),
    (id::DEBUG_1, RiderEvaluator::DebugOne),
];
