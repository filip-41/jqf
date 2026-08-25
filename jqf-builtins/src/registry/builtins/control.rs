//! Control-flow builtins: `select/1` and `not/0`.
//!
//! One job: own the `select` and `not` family and overload records. `select` is an `Evaluator`-kind builtin with one
//! filter argument; its predicate runs over the call's original input and each non-`false`/`null` output emits that
//! original input once, in order. Its execution is a consumer frame the executor drives (a subgraph runner, not a pure
//! function), so this module owns only the record — the predicate frame lives in [`crate::exec`] and the truth law in
//! [`crate::semantics::truth`]. `not/0` is an `Evaluator`-kind, arity-0 builtin producing exactly one boolean: the
//! falsiness of its input (`false`/`null` → `true`, every other value → `false`), read through the same
//! [`crate::semantics::truth`] law; its evaluation is one route in [`crate::exec`].
//!
//! Negative space: no evaluator function here; `select` cannot be a pure value transform because it drives its argument
//! graph, and `not`'s single-boolean output is produced at the executor's dispatch site beside `length`/`keys`.

use super::id;
use crate::registry::record::{
    BuiltinExample, BuiltinExecution, BuiltinFamilyId, BuiltinFamilyRecord, BuiltinOverloadId, BuiltinOverloadRecord,
    DemandTransfer, Effects, ParameterKind, SemanticRevision,
};

/// The `select`, `not`, `error`, `first`, and `limit` family records.
pub const FAMILIES: &[BuiltinFamilyRecord] = &[SELECT_FAMILY, NOT_FAMILY, ERROR_FAMILY, FIRST_FAMILY, LIMIT_FAMILY];

/// The `select/1`, `not/0`, `error/0`, `error/1`, `first/1`, and `limit/2` overload records.
pub const OVERLOADS: &[BuiltinOverloadRecord] = &[
    SELECT_OVERLOAD,
    NOT_OVERLOAD,
    ERROR_ZERO_OVERLOAD,
    ERROR_ONE_OVERLOAD,
    FIRST_OVERLOAD,
    LIMIT_OVERLOAD,
];

/// The control execution payloads, aligned one-to-one with [`OVERLOADS`].
///
/// The family spans evaluators and lowerings, so it names its own payload enum; `registry::dispatch` wraps it at table
/// build time. Every entry carries its overload id so the const coverage walk there can prove pairwise alignment.
#[derive(Clone, Copy, Debug)]
pub enum ControlPayload {
    /// `select/1` — the predicate filter.
    Select,
    /// `not/0` — the input-falsiness boolean.
    Not,
    /// `error/0` — raises the current input.
    ErrorZero,
    /// `error/1` — raises the argument's first output.
    ErrorOne,
    /// `first/1` — expands to `label $out | g | ., break $out`.
    First,
    /// `limit/2` — expands to the bounded `foreach`.
    Limit,
}

pub const PAYLOADS: &[(u16, ControlPayload)] = &[
    (id::SELECT, ControlPayload::Select),
    (id::NOT, ControlPayload::Not),
    (id::ERROR_ZERO, ControlPayload::ErrorZero),
    (id::ERROR_ONE, ControlPayload::ErrorOne),
    (id::FIRST, ControlPayload::First),
    (id::LIMIT, ControlPayload::Limit),
];

const FIRST_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::FIRST),
    canonical_name: "first",
    category: "control",
    summary: "The first output of a generator, or nothing when it produces none.",
    detail: "`first(g)` drives `g` only until its first output, emits it, and \
             abandons the rest. A generator with no outputs emits nothing rather \
             than `null`, and an error raised before the first output \
             propagates. This is the reference's own definition — \
             `label $out | g | ., break $out` — so the short-circuit IS the \
             non-local exit rather than a second mechanism beside it.",
};

const FIRST_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::FIRST),
    family: BuiltinFamilyId::new(id::FIRST),
    canonical_name: "first",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Lowering,
    demand_transfer: DemandTransfer::ViaLowering,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "first(.[])",
            input: "[3,1,2]",
            expected: "3\n",
        },
        BuiltinExample {
            program: "[first(empty)]",
            input: "null",
            expected: "[]\n",
        },
    ],
};

const LIMIT_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::LIMIT),
    canonical_name: "limit",
    category: "control",
    summary: "At most `n` outputs of a generator, abandoning it once the count \
              is reached.",
    detail: "`limit($n; f)` emits `f`'s first `n` outputs and then abandons it. \
             A count of zero emits nothing WITHOUT evaluating `f`; a negative \
             count raises `limit doesn't support negative count`. The count is a \
             VALUE parameter, so it may itself be a generator, in which case the \
             whole bounded drive runs once per count value. This is the reference's own \
             definition — a `foreach` counting down inside a `label` — so a \
             fractional count yields `ceil(n)` outputs, and a non-numeric count \
             raises the ordinary subtraction error LAZILY, only once `f` yields.",
};

const LIMIT_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::LIMIT),
    family: BuiltinFamilyId::new(id::LIMIT),
    canonical_name: "limit",
    arity: 2,
    parameters: &[ParameterKind::Value, ParameterKind::Filter],
    execution: BuiltinExecution::Lowering,
    demand_transfer: DemandTransfer::ViaLowering,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "[limit(2; .[])]",
            input: "[1,2,3]",
            expected: "[1,2]\n",
        },
        BuiltinExample {
            program: "[limit(0; error(\"never\"))]",
            input: "null",
            expected: "[]\n",
        },
    ],
};

const SELECT_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::SELECT),
    canonical_name: "select",
    category: "control",
    summary: "Emit the input unchanged for each truthy output of a predicate, \
              else nothing.",
    detail: "The predicate filter runs over the original input. Every predicate \
             output other than `false` or `null` emits that original input once, \
             in order; zero outputs emit nothing. Any prior emissions stand if the \
             predicate later errors.",
};

const SELECT_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::SELECT),
    family: BuiltinFamilyId::new(id::SELECT),
    canonical_name: "select",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::ConditionUnionPassThrough,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "select(.ok)",
            input: "{\"ok\":true,\"v\":1}",
            expected: "{\"ok\":true,\"v\":1}\n",
        },
        BuiltinExample {
            program: "select(true)",
            input: "42",
            expected: "42\n",
        },
    ],
};

const NOT_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::NOT),
    canonical_name: "not",
    category: "control",
    summary: "The boolean negation of the input's truthiness: `false`/`null` \
              become `true`, every other value becomes `false`.",
    detail: "`not` reads its input under the one truth law — only `false` and \
             `null` are falsy — and emits the opposite boolean. `0`, `\"\"`, `[]`, \
             and `{}` are all truthy, so each maps to `false`. It takes no \
             argument and always emits exactly one boolean.",
};

const NOT_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::NOT),
    family: BuiltinFamilyId::new(id::NOT),
    canonical_name: "not",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::InputPassThrough,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "not",
            input: "true",
            expected: "false\n",
        },
        BuiltinExample {
            program: "not",
            input: "null",
            expected: "true\n",
        },
        // `0` is truthy under the one truth law, so `0 | not` is `false`.
        BuiltinExample {
            program: "not",
            input: "0",
            expected: "false\n",
        },
    ],
};

const ERROR_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::ERROR),
    canonical_name: "error",
    category: "control",
    summary: "Raise an error: the current input (`error`) or an argument \
              (`error(f)`) becomes the raised value.",
    detail: "`error` raises the current input as the error value; `error(f)` \
             raises `f`'s FIRST output. The raised value is arbitrary (any JSON \
             value, not only a string). A `try … catch h` seeds `h` with it; an \
             uncaught raise ends the value's run. It emits zero normal outputs.",
};

const ERROR_ZERO_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::ERROR_ZERO),
    family: BuiltinFamilyId::new(id::ERROR),
    canonical_name: "error",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        // Written in caught form so the examples-as-tests harness asserts bytes.
        BuiltinExample {
            program: "try error catch .",
            input: "\"boom\"",
            expected: "\"boom\"\n",
        },
        BuiltinExample {
            program: "try error catch .",
            input: "5",
            expected: "5\n",
        },
    ],
};

const ERROR_ONE_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::ERROR_ONE),
    family: BuiltinFamilyId::new(id::ERROR),
    canonical_name: "error",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "try error(\"boom\") catch .",
            input: "null",
            expected: "\"boom\"\n",
        },
        BuiltinExample {
            program: "try error({\"code\":42}) catch .",
            input: "null",
            expected: "{\"code\":42}\n",
        },
        // A MULTI-VALUED message argument raises on its FIRST output; the rest are never reached.
        BuiltinExample {
            program: "try error(\"a\",\"b\") catch .",
            input: "null",
            expected: "\"a\"\n",
        },
    ],
};
