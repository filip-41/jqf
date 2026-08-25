//! The kind-filter family: `booleans`, `numbers`, `strings`, `arrays`, `objects`, `iterables`, `scalars`.
//!
//! One job: own the seven records and the one predicate that decides them. Each is an `Evaluator`-kind, arity-0,
//! zero-or-one-output builtin that emits its input UNCHANGED when the input's type is in the overload's set, and emits
//! nothing otherwise.
//!
//! These are the reference definitions — `def objects: select(type == "object");` and its siblings — and jqf
//! carried them that way until the deep-tree descent showed what the spelling costs. Every visited value paid a heap
//! `String` for `type`'s answer, a second one for the literal it was compared against, two ledger charges, a string
//! comparison, and a `select` predicate frame, to decide a question the value's own kind tag already answers. As a
//! native evaluator the same decision reads the kind and routes the input handle: no allocation, no frame, no copy of
//! the located value.
//!
//! The decision is DELEGATED, not reimplemented: [`super::core::type_of`] is the same law `type/0` publishes, total
//! over every kind a codec can produce (bytes and the temporal kinds get their own names), so a filter admits exactly
//! the values the `select(type == …)` spelling admitted.
//!
//! Negative space: `values`/`nulls` stay standard-library definitions. They are spelled `select(. != null)` / `select(.
//! == null)`, so their law is the engine's EQUALITY law rather than this kind law, and folding them in here would
//! create a second answer to `== null` for tagged values.

use super::core::{TypeName, type_of};
use super::id;
use crate::codec_result::EngineResult;
use crate::error::EngineRunError;
use crate::registry::record::{
    BuiltinExample, BuiltinExecution, BuiltinFamilyId, BuiltinFamilyRecord, BuiltinOverloadId, BuiltinOverloadRecord,
    DemandTransfer, Effects, SemanticRevision,
};
use jqf_resource::ResourceContext;

/// The seven kind-filter family records.
pub const FAMILIES: &[BuiltinFamilyRecord] = &[
    family(
        id::BOOLEANS,
        "booleans",
        "The input when it is a boolean, and nothing otherwise.",
        "Emits the input unchanged when `type` calls it `\"boolean\"`, and emits \
         nothing otherwise.",
    ),
    family(
        id::NUMBERS,
        "numbers",
        "The input when it is a number, and nothing otherwise.",
        "Emits the input unchanged when `type` calls it `\"number\"`, and emits \
         nothing otherwise.",
    ),
    family(
        id::STRINGS,
        "strings",
        "The input when it is a string, and nothing otherwise.",
        "Emits the input unchanged when `type` calls it `\"string\"`, and emits \
         nothing otherwise.",
    ),
    family(
        id::ARRAYS,
        "arrays",
        "The input when it is an array, and nothing otherwise.",
        "Emits the input unchanged when `type` calls it `\"array\"`, and emits \
         nothing otherwise.",
    ),
    family(
        id::OBJECTS,
        "objects",
        "The input when it is an object, and nothing otherwise.",
        "Emits the input unchanged when `type` calls it `\"object\"`, and emits \
         nothing otherwise.",
    ),
    family(
        id::ITERABLES,
        "iterables",
        "The input when it is an array or an object, and nothing otherwise.",
        "Emits the input unchanged when it is one of the two container types \
         `.[]` iterates, and emits nothing otherwise.",
    ),
    family(
        id::SCALARS,
        "scalars",
        "The input when it is neither an array nor an object, and nothing \
         otherwise.",
        "The complement of `iterables`: emits null, booleans, numbers and \
         strings unchanged, and emits nothing for a container.",
    ),
];

/// The seven kind-filter overload records.
pub const OVERLOADS: &[BuiltinOverloadRecord] = &[
    overload(id::BOOLEANS, "booleans", BOOLEANS_EXAMPLES),
    overload(id::NUMBERS, "numbers", NUMBERS_EXAMPLES),
    overload(id::STRINGS, "strings", STRINGS_EXAMPLES),
    overload(id::ARRAYS, "arrays", ARRAYS_EXAMPLES),
    overload(id::OBJECTS, "objects", OBJECTS_EXAMPLES),
    overload(id::ITERABLES, "iterables", ITERABLES_EXAMPLES),
    overload(id::SCALARS, "scalars", SCALARS_EXAMPLES),
];

/// The kind-filter execution payloads, aligned one-to-one with [`OVERLOADS`].
///
/// Every entry carries its overload id so the const coverage walk in `registry::dispatch` can prove pairwise alignment.
pub const PAYLOADS: &[(u16, KindFilter)] = &[
    (id::BOOLEANS, KindFilter::Boolean),
    (id::NUMBERS, KindFilter::Number),
    (id::STRINGS, KindFilter::String),
    (id::ARRAYS, KindFilter::Array),
    (id::OBJECTS, KindFilter::Object),
    (id::ITERABLES, KindFilter::Iterable),
    (id::SCALARS, KindFilter::Scalar),
];

/// Which types one kind filter admits.
///
/// This is the tag the dispatch table carries, so the executor decides every one of the seven with one match and no
/// builtin-specific branch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KindFilter {
    /// `booleans`.
    Boolean,
    /// `numbers`.
    Number,
    /// `strings`.
    String,
    /// `arrays`.
    Array,
    /// `objects`.
    Object,
    /// `iterables` — an array or an object.
    Iterable,
    /// `scalars` — anything that is neither.
    Scalar,
}

/// Whether `filter` admits `input`.
///
/// Raises exactly where `type/0` raises, because it asks `type/0`'s question.
pub fn admits(
    filter: KindFilter,
    input: &EngineResult<'_>,
    resources: &ResourceContext<'_>,
) -> Result<bool, EngineRunError> {
    let actual = type_of(input, resources)?;
    Ok(match filter {
        KindFilter::Boolean => actual == TypeName::Boolean,
        KindFilter::Number => actual == TypeName::Number,
        KindFilter::String => actual == TypeName::String,
        KindFilter::Array => actual == TypeName::Array,
        KindFilter::Object => actual == TypeName::Object,
        KindFilter::Iterable => matches!(actual, TypeName::Array | TypeName::Object),
        KindFilter::Scalar => !matches!(actual, TypeName::Array | TypeName::Object),
    })
}

/// One kind filter's family record.
const fn family(id: u16, name: &'static str, summary: &'static str, detail: &'static str) -> BuiltinFamilyRecord {
    BuiltinFamilyRecord {
        id: BuiltinFamilyId::new(id),
        canonical_name: name,
        category: "core",
        summary,
        detail,
    }
}

/// One kind filter's overload record. Every field but the identity and the examples is shared, because every one of the
/// seven runs the same way.
const fn overload(id: u16, name: &'static str, examples: &'static [BuiltinExample]) -> BuiltinOverloadRecord {
    BuiltinOverloadRecord {
        id: BuiltinOverloadId::new(id),
        family: BuiltinFamilyId::new(id),
        canonical_name: name,
        arity: 0,
        parameters: &[],
        execution: BuiltinExecution::Evaluator,
        // The filter reads the input's KIND, which on a document value is only knowable by reading it — the same
        // reason `type/0`'s kind read is shallow-safe — but an admitted input passes through WHOLE, so the demand is
        // the subtree, never the shallow fact. That is exactly what the `select(type == …)` spelling already
        // classified as, so the native form moves no route.
        demand_transfer: DemandTransfer::Subtree,
        semantic_revision: SemanticRevision::new(1),
        effects: Effects::Pure,
        examples,
    }
}

const BOOLEANS_EXAMPLES: &[BuiltinExample] = &[
    BuiltinExample {
        program: "booleans",
        input: "false",
        expected: "false\n",
    },
    BuiltinExample {
        program: "[.[] | booleans]",
        input: "[1, true, \"a\", null]",
        expected: "[true]\n",
    },
];

const NUMBERS_EXAMPLES: &[BuiltinExample] = &[
    BuiltinExample {
        program: "numbers",
        input: "1",
        expected: "1\n",
    },
    BuiltinExample {
        program: "[.[] | numbers]",
        input: "[1, true, \"a\", null]",
        expected: "[1]\n",
    },
];

const STRINGS_EXAMPLES: &[BuiltinExample] = &[
    BuiltinExample {
        program: "strings",
        input: "\"a\"",
        expected: "\"a\"\n",
    },
    BuiltinExample {
        program: "[.[] | strings]",
        input: "[1, true, \"a\", null]",
        expected: "[\"a\"]\n",
    },
];

const ARRAYS_EXAMPLES: &[BuiltinExample] = &[
    BuiltinExample {
        program: "arrays",
        input: "[1]",
        expected: "[1]\n",
    },
    BuiltinExample {
        program: "[.[] | arrays]",
        input: "[[1], {\"a\":1}, 2]",
        expected: "[[1]]\n",
    },
];

const OBJECTS_EXAMPLES: &[BuiltinExample] = &[
    BuiltinExample {
        program: "objects",
        input: "{\"a\":1}",
        expected: "{\"a\":1}\n",
    },
    BuiltinExample {
        program: "[.. | objects]",
        input: "{\"a\":{\"b\":1}}",
        expected: "[{\"a\":{\"b\":1}},{\"b\":1}]\n",
    },
];

const ITERABLES_EXAMPLES: &[BuiltinExample] = &[
    BuiltinExample {
        program: "iterables",
        input: "[1]",
        expected: "[1]\n",
    },
    BuiltinExample {
        program: "[.[] | iterables]",
        input: "[[1], {\"a\":1}, 2]",
        expected: "[[1],{\"a\":1}]\n",
    },
];

const SCALARS_EXAMPLES: &[BuiltinExample] = &[
    BuiltinExample {
        program: "scalars",
        input: "1",
        expected: "1\n",
    },
    BuiltinExample {
        program: "[.[] | scalars]",
        input: "[[1], {\"a\":1}, 2, null]",
        expected: "[2,null]\n",
    },
];
