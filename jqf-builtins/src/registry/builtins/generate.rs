//! The generator family: `range`, `while`, `until`, `repeat`, `recurse`, `combinations`, and the two consumers that
//! read a generator by position, `nth` and `skip`.
//!
//! One job: own the family and overload records for the vocabulary that PRODUCES values rather than reshaping one. The
//! stepping laws live in [`crate::semantics::generate`] and the frame that resumes them in [`crate::exec`]; nothing
//! here executes.
//!
//! **Three of these do not terminate, and that is the specification.** `while`, `until` and `repeat` run until a
//! consumer stops them — `limit`, `first`, or the ledger's cooperative control check. The reference trips an
//! assertion on some unbounded `while` shapes, so those shapes are NOT oracle-pinned anywhere in this repo; what IS
//! pinned is that a BOUNDED one (`[limit(3; repeat(1))]`) completes.
//!
//! **`repeat(f)` does not iterate.** It applies `f` to the ORIGINAL input, forever — `recurse` is the one that
//! iterates. The pinned rows:
//!
//! ```text
//! 0 | [limit(5; repeat(.+1))]   ->  [1,1,1,1,1]
//! 1 | [limit(4; recurse(.*2))]  ->  [1,2,4,8]
//! ```
//!
//! **`nth`/`skip` are consumers, not generators**, and are registered lowerings onto machinery that already exists:
//! `nth/1` is `.[$n]`, `skip/2` is the countdown `foreach` that `limit` is a sibling of, and `nth/2` is `first(skip($n;
//! g))`. All three reject a negative count with the reference's own message, which is a reference tightening — older
//! releases answered empty.
//!
//! Negative space: no evaluator payload and no stepping arithmetic live here.

use super::id;
use crate::registry::record::{
    BuiltinExample, BuiltinExecution, BuiltinFamilyId, BuiltinFamilyRecord, BuiltinOverloadId, BuiltinOverloadRecord,
    DemandTransfer, Effects, ParameterKind, SemanticRevision,
};
use crate::semantics::generate::{Generator, row};

/// One generator's canonical name, read out of the semantic table rather than respelled here — the registry's whole
/// premise is that one record feeds every consumer, and a second spelling of `range` would be a second record.
const fn name(generator: Generator) -> &'static str {
    row(generator).name
}

/// The generator family records.
pub const FAMILIES: &[BuiltinFamilyRecord] = &[
    RANGE_FAMILY,
    WHILE_FAMILY,
    UNTIL_FAMILY,
    REPEAT_FAMILY,
    RECURSE_FAMILY,
    COMBINATIONS_FAMILY,
    NTH_FAMILY,
    SKIP_FAMILY,
];

/// The generator overload records.
pub const OVERLOADS: &[BuiltinOverloadRecord] = &[
    RANGE_ONE_OVERLOAD,
    RANGE_TWO_OVERLOAD,
    RANGE_THREE_OVERLOAD,
    WHILE_OVERLOAD,
    UNTIL_OVERLOAD,
    REPEAT_OVERLOAD,
    RECURSE_ZERO_OVERLOAD,
    RECURSE_ONE_OVERLOAD,
    RECURSE_TWO_OVERLOAD,
    COMBINATIONS_ZERO_OVERLOAD,
    COMBINATIONS_ONE_OVERLOAD,
    NTH_ONE_OVERLOAD,
    NTH_TWO_OVERLOAD,
    SKIP_OVERLOAD,
];

/// The generator execution payloads, aligned one-to-one with [`OVERLOADS`].
///
/// The family spans value SOURCES (whose arities differ only in which argument they supply) and three lowerings (`nth`,
/// `skip`), so it names its own payload enum; `registry::dispatch` wraps it at table build time.
/// Every entry carries its overload id so the const coverage walk there can prove pairwise alignment.
#[derive(Clone, Copy, Debug)]
pub enum GeneratePayload {
    /// A value SOURCE; the arity is read off the call's argument list.
    Source(Generator),
    /// `nth/1` — expands to `.[$n]`.
    NthIndex,
    /// `nth/2` — expands to `first(skip($n; g))`.
    Nth,
    /// `skip/2` — expands to the reference's countdown `foreach`.
    Skip,
}

pub const PAYLOADS: &[(u16, GeneratePayload)] = &[
    (id::RANGE_ONE, GeneratePayload::Source(Generator::Range)),
    (id::RANGE_TWO, GeneratePayload::Source(Generator::Range)),
    (id::RANGE_THREE, GeneratePayload::Source(Generator::Range)),
    (id::WHILE, GeneratePayload::Source(Generator::While)),
    (id::UNTIL, GeneratePayload::Source(Generator::Until)),
    (id::REPEAT, GeneratePayload::Source(Generator::Repeat)),
    (id::RECURSE_ZERO, GeneratePayload::Source(Generator::Recurse)),
    (id::RECURSE_ONE, GeneratePayload::Source(Generator::Recurse)),
    (id::RECURSE_TWO, GeneratePayload::Source(Generator::Recurse)),
    (id::COMBINATIONS_ZERO, GeneratePayload::Source(Generator::Combinations)),
    (id::COMBINATIONS_ONE, GeneratePayload::Source(Generator::Combinations)),
    (id::NTH_ONE, GeneratePayload::NthIndex),
    (id::NTH_TWO, GeneratePayload::Nth),
    (id::SKIP, GeneratePayload::Skip),
];

const RANGE_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::RANGE),
    canonical_name: name(Generator::Range),
    category: "generators",
    summary: "The numbers from `from` (default 0) up to but excluding `upto`, stepping by `by`.",
    detail: "Every bound is a FILTER, and the nesting is LEFT-OUTER: `from` is \
             the outer loop, `upto` the middle one and `by` the innermost, so \
             `range(0,1; 3,4)` runs (0,3), (0,4), (1,3), (1,4) in that order. A \
             zero step emits nothing rather than hanging, and the bound is \
             ACCUMULATED (`current += by`) rather than recomputed, which is why \
             `[range(1;2;1e-1)] | .[2]` is `1.2000000000000002`. Arities 1 and 2 \
             REQUIRE numeric bounds (`Range bounds must be numeric`); arity 3 \
             does not, because the reference defines it in the language itself — a non-numeric triple walks \
             the TOTAL ORDER with binary `+`, so `range({};3;1)` is empty and \
             `range(\"a\";\"z\";\"b\")` never terminates.",
};

const RANGE_ONE_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::RANGE_ONE),
    family: BuiltinFamilyId::new(id::RANGE),
    canonical_name: name(Generator::Range),
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "[range(3)]",
            input: "null",
            expected: "[0,1,2]\n",
        },
        BuiltinExample {
            program: "[range(-1)]",
            input: "null",
            expected: "[]\n",
        },
    ],
};

const RANGE_TWO_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::RANGE_TWO),
    family: BuiltinFamilyId::new(id::RANGE),
    canonical_name: name(Generator::Range),
    arity: 2,
    parameters: &[ParameterKind::Filter, ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "[range(2;5)]",
            input: "null",
            expected: "[2,3,4]\n",
        },
        BuiltinExample {
            program: "[range(0,1;3,4)]",
            input: "null",
            expected: "[0,1,2,0,1,2,3,1,2,1,2,3]\n",
        },
    ],
};

const RANGE_THREE_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::RANGE_THREE),
    family: BuiltinFamilyId::new(id::RANGE),
    canonical_name: name(Generator::Range),
    arity: 3,
    parameters: &[ParameterKind::Filter, ParameterKind::Filter, ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "[range(0;10;3)]",
            input: "null",
            expected: "[0,3,6,9]\n",
        },
        BuiltinExample {
            program: "[range(0;3;0)]",
            input: "null",
            expected: "[]\n",
        },
        BuiltinExample {
            program: "[range({};3;1)]",
            input: "null",
            expected: "[]\n",
        },
        BuiltinExample {
            program: "[limit(3; range(\"a\";\"z\";\"b\"))]",
            input: "null",
            expected: "[\"a\",\"ab\",\"abb\"]\n",
        },
    ],
};

const WHILE_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::WHILE),
    canonical_name: name(Generator::While),
    category: "generators",
    summary: "Emit the value, then step it, for as long as the condition holds.",
    detail: "Exactly `def _while: if cond then ., (update | _while) else empty \
             end;`. The value is emitted BEFORE the step, so the first output \
             is the input itself whenever the condition already holds. A \
             condition that never goes falsy never terminates — bound it with \
             `limit` or `first`.",
};

const WHILE_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::WHILE),
    family: BuiltinFamilyId::new(id::WHILE),
    canonical_name: name(Generator::While),
    arity: 2,
    parameters: &[ParameterKind::Filter, ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "[while(.<100; .*2)]",
            input: "1",
            expected: "[1,2,4,8,16,32,64]\n",
        },
        BuiltinExample {
            program: "[limit(2; while(true; .+1))]",
            input: "0",
            expected: "[0,1]\n",
        },
    ],
};

const UNTIL_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::UNTIL),
    canonical_name: name(Generator::Until),
    category: "generators",
    summary: "Step the value until the condition holds, then emit it once.",
    detail: "Exactly `def _until: if cond then . else (update | _until) end;`. \
             Unlike `while` it emits ONE value — the first that satisfies the \
             condition — so it is a loop rather than a stream.",
};

const UNTIL_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::UNTIL),
    family: BuiltinFamilyId::new(id::UNTIL),
    canonical_name: name(Generator::Until),
    arity: 2,
    parameters: &[ParameterKind::Filter, ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "until(.>=100; .*2)",
            input: "1",
            expected: "128\n",
        },
        BuiltinExample {
            program: "until(true; .+1)",
            input: "5",
            expected: "5\n",
        },
    ],
};

const REPEAT_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::REPEAT),
    canonical_name: name(Generator::Repeat),
    category: "generators",
    summary: "Apply the filter to the input over and over, forever.",
    detail: "The filter is re-applied to the ORIGINAL input every round — it \
             does NOT iterate on its own output, which is what `recurse` does. \
             `0 | [limit(5; repeat(.+1))]` is `[1,1,1,1,1]`. A multi-output filter contributes \
             every output each round.",
};

const REPEAT_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::REPEAT),
    family: BuiltinFamilyId::new(id::REPEAT),
    canonical_name: name(Generator::Repeat),
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "[limit(3; repeat(.*2))]",
            input: "1",
            expected: "[2,2,2]\n",
        },
        BuiltinExample {
            program: "[limit(5; repeat(1,2))]",
            input: "null",
            expected: "[1,2,1,2,1]\n",
        },
    ],
};

const RECURSE_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::RECURSE),
    canonical_name: name(Generator::Recurse),
    category: "generators",
    summary: "The input, then the filter applied to it, then to that, and so on.",
    detail: "`recurse` is `def r: ., (f | r); r` — the value is emitted FIRST \
             and its children after it, depth first. `recurse/0` is \
             `recurse(.[]?)`, whose `?` SUPPRESSES the iterate error on a \
             scalar; `recurse/1` has no `?` and therefore PROPAGATES whatever \
             its argument raises. `recurse/2` gates the CHILDREN and never the \
             seed, so `20 | [recurse(.*2; . < 10)]` is `[20]`.",
};

const RECURSE_ZERO_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::RECURSE_ZERO),
    family: BuiltinFamilyId::new(id::RECURSE),
    canonical_name: name(Generator::Recurse),
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "[recurse]",
            input: "[[1]]",
            expected: "[[[1]],[1],1]\n",
        },
        BuiltinExample {
            program: "[recurse]",
            input: "{\"a\":1}",
            expected: "[{\"a\":1},1]\n",
        },
    ],
};

const RECURSE_ONE_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::RECURSE_ONE),
    family: BuiltinFamilyId::new(id::RECURSE),
    canonical_name: name(Generator::Recurse),
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "[recurse(if . < 8 then .*2 else empty end)]",
            input: "1",
            expected: "[1,2,4,8]\n",
        },
        BuiltinExample {
            program: "[limit(4; recurse(.*2))]",
            input: "1",
            expected: "[1,2,4,8]\n",
        },
    ],
};

const RECURSE_TWO_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::RECURSE_TWO),
    family: BuiltinFamilyId::new(id::RECURSE),
    canonical_name: name(Generator::Recurse),
    arity: 2,
    parameters: &[ParameterKind::Filter, ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "[recurse(.*2; . < 10)]",
            input: "1",
            expected: "[1,2,4,8]\n",
        },
        BuiltinExample {
            program: "[recurse(.*2; . < 10)]",
            input: "20",
            expected: "[20]\n",
        },
    ],
};

const COMBINATIONS_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::COMBINATIONS),
    canonical_name: name(Generator::Combinations),
    category: "generators",
    summary: "One array per Cartesian combination of the input's members.",
    detail: "The rightmost dimension varies fastest. ZERO dimensions produce \
             exactly ONE empty combination (`[] | [combinations]` is `[[]]`) \
             while an EMPTY dimension produces none at all. Dimensions are read \
             LAZILY, so `[[1,2],[]] | combinations` is empty with no error \
             while `[[1],1] | combinations` raises — the second dimension is \
             only examined once the first has a child to pair with it.",
};

const COMBINATIONS_ZERO_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::COMBINATIONS_ZERO),
    family: BuiltinFamilyId::new(id::COMBINATIONS),
    canonical_name: name(Generator::Combinations),
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "[combinations]",
            input: "[[1,2],[3,4]]",
            expected: "[[1,3],[1,4],[2,3],[2,4]]\n",
        },
        BuiltinExample {
            program: "[combinations]",
            input: "[]",
            expected: "[[]]\n",
        },
    ],
};

const COMBINATIONS_ONE_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::COMBINATIONS_ONE),
    family: BuiltinFamilyId::new(id::COMBINATIONS),
    canonical_name: name(Generator::Combinations),
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "[combinations(2)]",
            input: "[0,1]",
            expected: "[[0,0],[0,1],[1,0],[1,1]]\n",
        },
        BuiltinExample {
            program: "[combinations(0)]",
            input: "[1]",
            expected: "[[]]\n",
        },
    ],
};

const NTH_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::NTH),
    canonical_name: "nth",
    category: "generators",
    summary: "The nth element of an array, or the nth output of a generator.",
    detail: "`nth($n)` is exactly `.[$n]`, so a NEGATIVE index counts from the \
             end. `nth($n; g)` is `first(skip($n; g))`, which is a different \
             thing: it emits NOTHING when the generator is shorter, and a \
             negative index is an error rather than a count from the end \
             (a generator has no end to count from).",
};

const NTH_ONE_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::NTH_ONE),
    family: BuiltinFamilyId::new(id::NTH),
    canonical_name: "nth",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Lowering,
    demand_transfer: DemandTransfer::ViaLowering,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "nth(1)",
            input: "[10,20,30]",
            expected: "20\n",
        },
        BuiltinExample {
            program: "nth(-1)",
            input: "[10,20,30]",
            expected: "30\n",
        },
    ],
};

const NTH_TWO_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::NTH_TWO),
    family: BuiltinFamilyId::new(id::NTH),
    canonical_name: "nth",
    arity: 2,
    parameters: &[ParameterKind::Filter, ParameterKind::Filter],
    execution: BuiltinExecution::Lowering,
    demand_transfer: DemandTransfer::ViaLowering,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "nth(1; range(5))",
            input: "null",
            expected: "1\n",
        },
        BuiltinExample {
            program: "[nth(5; 1,2,3)]",
            input: "null",
            expected: "[]\n",
        },
    ],
};

const SKIP_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::SKIP),
    canonical_name: "skip",
    category: "generators",
    summary: "Every output of a generator except the first `$n`.",
    detail: "A countdown `foreach` — `foreach f as $item ($n; . - 1; if . < 0 \
             then $item else empty end)` — so a fractional count behaves like a \
             countdown rather than an index, and a NON-NUMERIC count raises the \
             ordinary subtraction type error lazily, only once the generator \
             yields. A negative count is rejected up front, which is a reference \
             tightening: older releases answered empty.",
};

const SKIP_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::SKIP),
    family: BuiltinFamilyId::new(id::SKIP),
    canonical_name: "skip",
    arity: 2,
    parameters: &[ParameterKind::Filter, ParameterKind::Filter],
    execution: BuiltinExecution::Lowering,
    demand_transfer: DemandTransfer::ViaLowering,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "[skip(2; range(5))]",
            input: "null",
            expected: "[2,3,4]\n",
        },
        BuiltinExample {
            program: "[skip(0; 1,2)]",
            input: "null",
            expected: "[1,2]\n",
        },
    ],
};
