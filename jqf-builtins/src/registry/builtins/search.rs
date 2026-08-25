//! The argument-taking text builtins: what a string starts and ends with, what to take off its ends, where a needle
//! sits inside it, where to cut it, and what is inside what.
//!
//! One job: own the family and overload records for the text builtins that read an ARGUMENT as well as an input, plus
//! the one law table the executor dispatches through. Each answers once per argument output, so they share
//! [`crate::exec`]'s argument-answer drive rather than its owned-law drive — which is the line between this module
//! and [`crate::registry::builtins::strings`], whose ten laws are functions of the input alone.
//!
//! The search half is the `indices` family, and the reference builds it out of pieces it already has:
//!
//! ```text
//! def indices($i): if type == "array" and ($i|type) == "array" then .[$i] elif type == "array" then .[[$i]] elif type
//! == "string" and ($i|type) == "string" then _strindices($i) else .[$i] end;
//! def index($i):  indices($i) | .[0];
//! def rindex($i): indices($i) | .[-1:][0];
//! ```
//!
//! Three of those four branches are searches and land here as one evaluator; the FOURTH is the ordinary index step, and
//! it reaches it through [`crate::exec::index_owned`] rather than a second spelling. That is what makes `{"a":7} |
//! indices("a")` answer `7` and `1 | indices("a")` say `Cannot index number with string ("a")` — the reference's
//! `indices` is not a search over those inputs at all, it is an index, and every message follows from saying so once.
//!
//! `index/1` and `rindex/1` stay LOWERINGS for the same reason: `.[0]` and `.[-1:][0]` are real steps, so the
//! pass-through cases inherit the real answers, down to `{"a":1} | rindex("a")` reporting the slice mismatch's
//! synthesized accessor (`Cannot index number with object ({"start":-1,"end":null})`). Hand-rolling "the last element"
//! would have to reproduce the slice law to say that.
//!
//! `split/1` is the `./$s` operator under another name, so its cut is [`crate::semantics::text::split`] — the
//! operator's own law — and only its refusal is its own.
//!
//! `contains/1` is the last member and the odd one: its relation is a VALUE law rather than a text one — strings are
//! one of its four cases — so the recursion lives in [`crate::semantics::contain`] and only the refusal is here.
//! `inside/1` is a LOWERING of the reference definition, `. as $x | xs | contains($x)`, which is why `"a" | inside(1)`
//! reports the operands in the SWAPPED order.
//!
//! `ltrimstr`, `rtrimstr` and `trimstr` are not separate laws from the two predicates; they are those predicates plus a
//! slice. The reference makes that literal — the trio are language-level definitions over `startswith`/`endswith`,
//! not the pass-through C builtins of 1.6 and 1.7 — and it leaks through the messages, which is the whole reason all
//! five live together:
//!
//! ```text
//! '1'    | ltrimstr("x")  → startswith() requires string inputs
//! '1'    | rtrimstr("x")  → endswith() requires string inputs
//! '"hi"' | trimstr(1)     → startswith() requires string inputs
//! ```
//!
//! `trimstr` reports `startswith`'s message because it is `ltrimstr` THEN `rtrimstr` and the first half raises first.
//! One message per predicate, named with empty parens, and raised no matter which SIDE is the non-string.
//!
//! Each trim removes at most ONE occurrence, which is what makes `trimstr` a composition rather than a loop: `"aa" |
//! trimstr("a")` is `""` (one off each end) while `"aaa" | trimstr("a")` is `"a"` and not `""`.
//!
//! Negative space: it owns no message text, which is [`message`]'s; it owns no arity-0 law, which is `strings`'; it
//! owns no search or cut algorithm, which is [`crate::semantics::text`]'s, shared with the `/` operator; and it owns no
//! codepoint arithmetic for the AFFIX laws — a prefix test is a byte-slice test, and UTF-8 makes that the same
//! answer.

use jqf_data::{Array, Object, Value};
use jqf_resource::ResourceContext;

use super::id;
use crate::error::{EngineRunError, message};
use crate::registry::record::{
    BuiltinExample, BuiltinExecution, BuiltinFamilyId, BuiltinFamilyRecord, BuiltinOverloadId, BuiltinOverloadRecord,
    DemandTransfer, Effects, ParameterKind, SemanticRevision,
};
use crate::semantics::depth::{self, Guarded};
use crate::semantics::index_owned;
use crate::semantics::owned_kind;
use crate::semantics::path::{raise, try_clone};
use crate::semantics::{contain, text};

/// The argument-taking text family records.
pub const FAMILIES: &[BuiltinFamilyRecord] = &[
    STARTSWITH_FAMILY,
    ENDSWITH_FAMILY,
    LTRIMSTR_FAMILY,
    RTRIMSTR_FAMILY,
    TRIMSTR_FAMILY,
    INDICES_FAMILY,
    INDEX_OF_FAMILY,
    RINDEX_OF_FAMILY,
    STRINDICES_FAMILY,
    SPLIT_FAMILY,
    CONTAINS_FAMILY,
    INSIDE_FAMILY,
];

/// The argument-taking text overload records.
pub const OVERLOADS: &[BuiltinOverloadRecord] = &[
    STARTSWITH_OVERLOAD,
    ENDSWITH_OVERLOAD,
    LTRIMSTR_OVERLOAD,
    RTRIMSTR_OVERLOAD,
    TRIMSTR_OVERLOAD,
    INDICES_OVERLOAD,
    INDEX_OF_OVERLOAD,
    RINDEX_OF_OVERLOAD,
    STRINDICES_OVERLOAD,
    SPLIT_OVERLOAD,
    CONTAINS_OVERLOAD,
    INSIDE_OVERLOAD,
];

/// The search execution payloads, aligned one-to-one with [`OVERLOADS`].
///
/// The family spans argument-taking text laws and one lowering (`inside`), so it names its own payload enum;
/// `registry::dispatch` wraps it at table build time. Every entry carries its overload id so the const coverage walk
/// there can prove pairwise alignment.
#[derive(Clone, Copy, Debug)]
pub enum SearchPayload {
    /// One of the argument-taking text laws.
    Text(TextLaw),
    /// `inside/1` — expands to `. as $x | xs | contains($x)`.
    Inside,
}

pub const PAYLOADS: &[(u16, SearchPayload)] = &[
    (id::STARTSWITH, SearchPayload::Text(TextLaw::StartsWith)),
    (id::ENDSWITH, SearchPayload::Text(TextLaw::EndsWith)),
    (id::LTRIMSTR, SearchPayload::Text(TextLaw::LTrimStr)),
    (id::RTRIMSTR, SearchPayload::Text(TextLaw::RTrimStr)),
    (id::TRIMSTR, SearchPayload::Text(TextLaw::TrimStr)),
    (id::INDICES, SearchPayload::Text(TextLaw::Indices)),
    (id::INDEX_OF, SearchPayload::Text(TextLaw::FirstIndexOf)),
    (id::RINDEX_OF, SearchPayload::Text(TextLaw::LastIndexOf)),
    (id::STRINDICES, SearchPayload::Text(TextLaw::StrIndices)),
    (id::SPLIT, SearchPayload::Text(TextLaw::Split)),
    (id::CONTAINS, SearchPayload::Text(TextLaw::Contains)),
    (id::INSIDE, SearchPayload::Inside),
];

/// Which argument-taking text law one `Call` runs.
#[derive(Clone, Copy, Debug)]
pub enum TextLaw {
    /// `startswith/1` — the prefix predicate.
    StartsWith,
    /// `endswith/1` — the suffix predicate.
    EndsWith,
    /// `ltrimstr/1` — one leading occurrence removed.
    LTrimStr,
    /// `rtrimstr/1` — one trailing occurrence removed.
    RTrimStr,
    /// `trimstr/1` — `ltrimstr` then `rtrimstr`.
    TrimStr,
    /// `indices/1` — every position of a needle, or the index step.
    Indices,
    /// `index/1` — the FIRST position of a needle (or the fallback's `.[0]`), scanned with an early stop and no
    /// position array.
    FirstIndexOf,
    /// `rindex/1` — the LAST position of a needle (or the fallback's `.[-1:][0]`), scanned end-first and with no
    /// position array.
    LastIndexOf,
    /// `_strindices/1` — the string route on its own, with its own refusals.
    StrIndices,
    /// `split/1` — the `/` operator's cut with its own refusal.
    Split,
    /// `contains/1` — the containment relation, with the top-level kind rule.
    Contains,
}

/// Evaluates one argument-taking text law.
///
/// # Errors
///
/// Returns the law's own refusal for an input or argument it does not accept, or an allocation failure.
pub fn apply(
    law: TextLaw,
    input: &Value,
    argument: &Value,
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    match law {
        // The reported name is written out per law rather than derived, because it is not always the law's own: the
        // reference DEFINES the trims over the two predicates, so `ltrimstr` and `trimstr` report `startswith` (the
        // first half raises first) and `rtrimstr` reports `endswith`.
        TextLaw::StartsWith => {
            let (text, affix) = affixes("startswith", input, argument, resources)?;
            Ok(Value::Bool(text.starts_with(affix)))
        }
        TextLaw::EndsWith => {
            let (text, affix) = affixes("endswith", input, argument, resources)?;
            Ok(Value::Bool(text.ends_with(affix)))
        }
        TextLaw::LTrimStr => {
            let (text, affix) = affixes("startswith", input, argument, resources)?;
            published(input, strip_prefix(text, affix), resources)
        }
        TextLaw::RTrimStr => {
            let (text, affix) = affixes("endswith", input, argument, resources)?;
            published(input, strip_suffix(text, affix), resources)
        }
        // The second strip reads the FIRST one's answer, which is what makes the two ends independent: `"aa" |
        // trimstr("a")` takes one occurrence off each end and lands on `""`.
        TextLaw::TrimStr => {
            let (text, affix) = affixes("startswith", input, argument, resources)?;
            published(input, strip_suffix(strip_prefix(text, affix), affix), resources)
        }
        TextLaw::Contains => contains(input, argument, resources),
        TextLaw::Indices => indices(input, argument, resources),
        TextLaw::FirstIndexOf => first_index(input, argument, resources),
        TextLaw::LastIndexOf => last_index(input, argument, resources),
        TextLaw::StrIndices => string_indices(input, argument, resources),
        TextLaw::Split => cut(input, argument, resources),
    }
}

/// Both sides as text, or the refusal `name` reports.
///
/// One gate for all five affix laws, because the reference has one: the message depends on which BUILTIN was called and
/// never on which side was wrong, so `startswith(1)` and `1 | startswith("a")` report identically.
fn affixes<'value>(
    name: &str,
    input: &'value Value,
    argument: &'value Value,
    resources: &ResourceContext<'_>,
) -> Result<(&'value str, &'value str), EngineRunError> {
    if let (Value::String(text), Value::String(affix)) = (input.untagged(), argument.untagged()) {
        return Ok((text.as_str(), affix.as_str()));
    }
    let text = message::requires_string_inputs_message(name)?;
    Err(raise(&text, resources))
}

/// `contains/1`: the top-level kind rule, then the relation.
///
/// The two halves are separate because the reference's are: a mismatch HERE raises and the same mismatch one level down
/// answers `false`, so the rule cannot be folded into the recursion.
fn contains(input: &Value, argument: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    if !contain::checkable(input, argument) {
        let left = message::dump_trunc_owned(input)?;
        let right = message::dump_trunc_owned(argument)?;
        let text = message::containment_message(input.kind(), &left, argument.kind(), &right)?;
        return Err(raise(&text, resources));
    }
    let held =
        contain::contains(input, argument).map_err(|_| raise(depth::message(Guarded::Containment), resources))?;
    Ok(Value::Bool(held))
}

/// `indices/1`: the reference's four branches, three searches and one index step.
fn indices(input: &Value, argument: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    match (input.untagged(), argument.untagged()) {
        (Value::Array(haystack), Value::Array(needle)) => text::subsequence_indices(haystack, needle, resources),
        // A non-array needle over an array is the `.[[$i]]` step: the needle becomes a ONE-element subsequence, which
        // is why `[0,1,2,1] | indices(1)` and `indices([1])` agree.
        (Value::Array(haystack), element) => {
            let needle = one_element(element, resources)?;
            text::subsequence_indices(haystack, &needle, resources)
        }
        (Value::String(haystack), Value::String(needle)) => {
            text::codepoint_indices(haystack.as_str(), needle.as_str(), resources)
        }
        // Not a search at all — see the module note.
        _ => index_owned(input, argument, resources),
    }
}

/// `index/1`: the FIRST position, one scan with an early stop.
///
/// The reference defines `index($s)` as `indices($s) | .[0]`, and the definition is the contract: the three search
/// branches answer the first position (or `null` when there is none), and the fallback branch applies `.[0]` to the
/// index step's answer, exactly as the piped graph would. The scan stops at the first match, so a haystack probed
/// repeatedly (`$b | index($v)` inside a map — the set-membership spelling) pays for the hit it finds, not for the
/// whole haystack and a position array it never reads.
fn first_index(input: &Value, argument: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    match (input.untagged(), argument.untagged()) {
        (Value::Array(haystack), Value::Array(needle)) => text::first_subsequence_position(haystack, needle, resources),
        // The same one-element wrapping `indices` gives a non-array needle over an array: `[0,1,2,1] | index(1)` and
        // `index([1])` agree.
        (Value::Array(haystack), element) => {
            let needle = one_element(element, resources)?;
            text::first_subsequence_position(haystack, &needle, resources)
        }
        (Value::String(haystack), Value::String(needle)) => {
            Ok(text::first_codepoint_position(haystack.as_str(), needle.as_str()))
        }
        // The `.[argument]` fallback piped into `.[0]`: two owned index steps, the second with the literal `0` the
        // graph uses.
        _ => {
            let fallback = index_owned(input, argument, resources)?;
            index_owned(
                &fallback,
                &Value::Number(jqf_data::Number::integer(jqf_data::Integer::from_i64(0))),
                resources,
            )
        }
    }
}

/// `rindex/1`: the LAST position, one end-first scan.
///
/// The reference defines `rindex($s)` as `indices($s) | .[-1:][0]`, and the definition is the contract: the search
/// branches answer the last position (or `null`), and the fallback branch applies the `.[-1:]` slice then `.[0]` to the
/// index step's answer — the same three steps the piped graph runs.
fn last_index(input: &Value, argument: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    match (input.untagged(), argument.untagged()) {
        (Value::Array(haystack), Value::Array(needle)) => text::last_subsequence_position(haystack, needle, resources),
        (Value::Array(haystack), element) => {
            let needle = one_element(element, resources)?;
            text::last_subsequence_position(haystack, &needle, resources)
        }
        (Value::String(haystack), Value::String(needle)) => {
            Ok(text::last_codepoint_position(haystack.as_str(), needle.as_str()))
        }
        _ => {
            let fallback = index_owned(input, argument, resources)?;
            let sliced = last_slice_fallback(&fallback, resources)?;
            index_owned(
                &sliced,
                &Value::Number(jqf_data::Number::integer(jqf_data::Integer::from_i64(0))),
                resources,
            )
        }
    }
}

/// The `.[-1:]` slice step over an OWNED value, for `rindex`'s fallback branch: `null` stays `null`, an array keeps its
/// last element in a fresh one-element array, a string keeps its last codepoint, and any other kind raises the slice
/// step's own mismatch message with the authored bounds `{"start":-1,"end":null}` — the exact operand the piped graph
/// renders.
fn last_slice_fallback(value: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    match value.untagged() {
        Value::Null => Ok(Value::Null),
        Value::Array(array) => {
            let mut out = Array::try_new().map_err(|_| EngineRunError::allocation_failure())?;
            if let Some(last) = array.get(array.len().wrapping_sub(1)) {
                out.try_push(try_clone(last))
                    .map_err(|_| EngineRunError::allocation_failure())?;
            }
            Ok(Value::Array(out))
        }
        Value::String(text) => {
            let last = text.chars().next_back().map_or("", |ch| {
                // The final codepoint's byte slice, owned by the source string.
                let end = text.len();
                let start = end - ch.len_utf8();
                &text[start..end]
            });
            Ok(Value::String(
                jqf_data::Shared::try_from_str(last).map_err(|_| EngineRunError::allocation_failure())?,
            ))
        }
        other => {
            let mut operand = Object::try_new().map_err(|_| EngineRunError::allocation_failure())?;
            let start_key =
                jqf_data::ObjectKey::try_from_str("start").map_err(|_| EngineRunError::allocation_failure())?;
            operand
                .try_insert_unique(
                    start_key,
                    Value::Number(jqf_data::Number::integer(jqf_data::Integer::from_i64(-1))),
                )
                .map_err(|_| EngineRunError::allocation_failure())?;
            let end_key = jqf_data::ObjectKey::try_from_str("end").map_err(|_| EngineRunError::allocation_failure())?;
            operand
                .try_insert_unique(end_key, Value::Null)
                .map_err(|_| EngineRunError::allocation_failure())?;
            let rendered = message::render_owned_key(&Value::Object(operand))?;
            let text = message::index_message(owned_kind(other), &rendered)?;
            Err(raise(&text, resources))
        }
    }
}

/// `_strindices/1`: the string route, with the two refusals the reference gives it alone.
///
/// The INPUT is checked first, so `123 | _strindices(123)` reports the input's message and never the argument's.
fn string_indices(input: &Value, argument: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let Value::String(haystack) = input.untagged() else {
        let operand = message::dump_trunc_owned(input)?;
        let text = message::not_searchable_string_message(input.kind(), &operand)?;
        return Err(raise(&text, resources));
    };
    let Value::String(needle) = argument.untagged() else {
        let operand = message::dump_trunc_owned(argument)?;
        let text = message::not_a_string_message(argument.kind(), &operand)?;
        return Err(raise(&text, resources));
    };
    text::codepoint_indices(haystack.as_str(), needle.as_str(), resources)
}

/// `split/1`: the `/` operator's cut, refusing with `split`'s own message.
fn cut(input: &Value, argument: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let (Value::String(text), Value::String(separator)) = (input.untagged(), argument.untagged()) else {
        let message = message::split_inputs_message()?;
        return Err(raise(&message, resources));
    };
    text::split(text.as_str(), separator.as_str(), resources).map_err(|_| EngineRunError::allocation_failure())
}

/// `element` as a one-element array, which is the needle the `.[[$i]]` step builds.
fn one_element(element: &Value, _resources: &ResourceContext<'_>) -> Result<Array, EngineRunError> {
    let mut needle = Array::try_new().map_err(|_| EngineRunError::allocation_failure())?;
    needle
        .try_push(try_clone(element))
        .map_err(|_| EngineRunError::allocation_failure())?;
    Ok(needle)
}

/// `text` without one leading `affix`, or `text` when it does not start with one.
fn strip_prefix<'text>(text: &'text str, affix: &str) -> &'text str {
    text.strip_prefix(affix).unwrap_or(text)
}

/// `text` without one trailing `affix`, or `text` when it does not end with one.
fn strip_suffix<'text>(text: &'text str, affix: &str) -> &'text str {
    text.strip_suffix(affix).unwrap_or(text)
}

/// The trimmed text as a value, retaining the input's payload when nothing moved.
///
/// A trim that matched nothing is the identity, and an identity has no copy to make: the empty affix takes this path
/// for every input, and so does every no-match, which is the common case in a `map(ltrimstr(…))` over mixed data.
fn published(input: &Value, trimmed: &str, _resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    if let Value::String(payload) = input.untagged()
        && trimmed.len() == payload.as_str().len()
    {
        return Ok(Value::String(payload.clone_shared()));
    }
    Value::try_string(trimmed).map_err(|_| EngineRunError::allocation_failure())
}

const STARTSWITH_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::STARTSWITH),
    canonical_name: "startswith",
    category: "text",
    summary: "Whether the input string begins with the argument string.",
    detail: "The empty prefix is always present, so `startswith(\"\")` is `true` \
             for every string including the empty one. A non-string on EITHER \
             side raises `startswith() requires string inputs`.",
};

const STARTSWITH_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::STARTSWITH),
    family: BuiltinFamilyId::new(id::STARTSWITH),
    canonical_name: "startswith",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "[.[] | startswith(\"foo\")]",
            input: "[\"fo\",\"foo\",\"barfoo\",\"foobar\"]",
            expected: "[false,true,false,true]\n",
        },
        BuiltinExample {
            program: "[startswith(\"a\",\"b\",\"\")]",
            input: "\"abc\"",
            expected: "[true,false,true]\n",
        },
    ],
};

const ENDSWITH_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::ENDSWITH),
    canonical_name: "endswith",
    category: "text",
    summary: "Whether the input string ends with the argument string.",
    detail: "`startswith`'s law on the other end, with its own message: a \
             non-string on either side raises `endswith() requires string \
             inputs`.",
};

const ENDSWITH_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::ENDSWITH),
    family: BuiltinFamilyId::new(id::ENDSWITH),
    canonical_name: "endswith",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "[.[] | endswith(\"foo\")]",
            input: "[\"fo\",\"foo\",\"barfoo\",\"foobar\"]",
            expected: "[false,true,true,false]\n",
        },
        BuiltinExample {
            program: "endswith(\"\")",
            input: "\"\"",
            expected: "true\n",
        },
    ],
};

const LTRIMSTR_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::LTRIMSTR),
    canonical_name: "ltrimstr",
    category: "text",
    summary: "The input string with ONE leading occurrence of the argument \
              removed.",
    detail: "Unchanged when the string does not start with the argument, and \
             unchanged for the empty argument. Only one occurrence goes: \
             `\"aa\" | ltrimstr(\"a\")` is `\"a\"`. In the current reference a non-string \
             on either side RAISES rather than passing through, and the message \
             is `startswith`'s, because that is what the reference defines it over.",
};

const LTRIMSTR_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::LTRIMSTR),
    family: BuiltinFamilyId::new(id::LTRIMSTR),
    canonical_name: "ltrimstr",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "[.[] | ltrimstr(\"foo\")]",
            input: "[\"fo\",\"foo\",\"barfoo\",\"foobar\",\"afoo\"]",
            expected: "[\"fo\",\"\",\"barfoo\",\"bar\",\"afoo\"]\n",
        },
        BuiltinExample {
            program: "ltrimstr(\"a\")",
            input: "\"aa\"",
            expected: "\"a\"\n",
        },
    ],
};

const RTRIMSTR_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::RTRIMSTR),
    canonical_name: "rtrimstr",
    category: "text",
    summary: "The input string with ONE trailing occurrence of the argument \
              removed.",
    detail: "`ltrimstr`'s law on the other end, and it reports `endswith`'s \
             message for the same reason.",
};

const RTRIMSTR_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::RTRIMSTR),
    family: BuiltinFamilyId::new(id::RTRIMSTR),
    canonical_name: "rtrimstr",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "[.[] | rtrimstr(\"foo\")]",
            input: "[\"fo\",\"foo\",\"barfoo\",\"foobar\",\"foob\"]",
            expected: "[\"fo\",\"\",\"bar\",\"foobar\",\"foob\"]\n",
        },
        BuiltinExample {
            program: "rtrimstr(\"\")",
            input: "\"xx\"",
            expected: "\"xx\"\n",
        },
    ],
};

const TRIMSTR_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::TRIMSTR),
    canonical_name: "trimstr",
    category: "text",
    summary: "The input string with one leading AND one trailing occurrence of \
              the argument removed.",
    detail: "`ltrimstr` then `rtrimstr`, so the ends are independent and each \
             gives up at most one occurrence: `\"aa\" | trimstr(\"a\")` is `\"\"` \
             while `\"aaa\" | trimstr(\"a\")` is `\"a\"`. It reports \
             `startswith`'s message, because the first half raises first.",
};

const TRIMSTR_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::TRIMSTR),
    family: BuiltinFamilyId::new(id::TRIMSTR),
    canonical_name: "trimstr",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "[.[] | trimstr(\"foo\")]",
            input: "[\"fo\",\"foo\",\"barfoo\",\"foobarfoo\",\"foob\"]",
            expected: "[\"fo\",\"\",\"bar\",\"bar\",\"b\"]\n",
        },
        BuiltinExample {
            program: "trimstr(\"a\")",
            input: "\"aaa\"",
            expected: "\"a\"\n",
        },
    ],
};

const INDICES_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::INDICES),
    canonical_name: "indices",
    category: "text",
    summary: "Every position at which the argument occurs in the input.",
    detail: "Over a string it is the CODEPOINT positions of a substring; over an \
             array it is the positions of a subsequence, with a non-array \
             argument read as a one-element one. Both searches OVERLAP, so \
             `\"aaaa\" | indices(\"aa\")` is `[0,1,2]`. Any other input is not a \
             search: the law falls through to `.[argument]`, so an object answers the \
             value at that key and a number raises the index step's own \
             mismatch.",
};

const INDICES_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::INDICES),
    family: BuiltinFamilyId::new(id::INDICES),
    canonical_name: "indices",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "indices(\", \")",
            input: "\"a,b, cd,e, fgh, ijkl\"",
            expected: "[3,9,14]\n",
        },
        BuiltinExample {
            program: "indices([1,2])",
            input: "[0,1,2,3,1,4,2,5,1,2,6,7]",
            expected: "[1,8]\n",
        },
    ],
};

const INDEX_OF_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::INDEX_OF),
    canonical_name: "index",
    category: "text",
    summary: "The FIRST position at which the argument occurs, or `null`.",
    detail: "`indices(needle) | .[0]`, which is where the `null` comes from: no \
             match is the empty position array and `[] | .[0]` is `null`. The \
             empty needle never matches, so `\"\" | index(\"\")` is `null` and \
             not `0`.",
};

const INDEX_OF_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::INDEX_OF),
    family: BuiltinFamilyId::new(id::INDEX_OF),
    canonical_name: "index",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "index(\",\")",
            input: "\"a,bc,def\"",
            expected: "1\n",
        },
        BuiltinExample {
            program: "index(\"z\")",
            input: "\"abc\"",
            expected: "null\n",
        },
    ],
};

const RINDEX_OF_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::RINDEX_OF),
    canonical_name: "rindex",
    category: "text",
    summary: "The LAST position at which the argument occurs, or `null`.",
    detail: "`indices(needle) | .[-1:][0]` — the last position through a slice \
             rather than a length, which is what makes the empty case `null` \
             without a length test.",
};

const RINDEX_OF_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::RINDEX_OF),
    family: BuiltinFamilyId::new(id::RINDEX_OF),
    canonical_name: "rindex",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "rindex(\",\")",
            input: "\"a,bc,def\"",
            expected: "4\n",
        },
        BuiltinExample {
            program: "rindex(\"aba\")",
            input: "\"abababa\"",
            expected: "4\n",
        },
    ],
};

const STRINDICES_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::STRINDICES),
    canonical_name: "_strindices",
    category: "text",
    summary: "The string route of `indices` on its own, for string inputs only.",
    detail: "The reference's internal spelling, reachable by name and with refusals of its \
             own: a non-string input says `<kind> (<value>) cannot be searched, \
             as it is not a string` and a non-string argument says `<kind> \
             (<value>) is not a string`. The input is checked first.",
};

const STRINDICES_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::STRINDICES),
    family: BuiltinFamilyId::new(id::STRINDICES),
    canonical_name: "_strindices",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "_strindices(\"é\")",
            input: "\"xéyé\"",
            expected: "[1,3]\n",
        },
        BuiltinExample {
            program: "try _strindices(123) catch .",
            input: "\"abc\"",
            expected: "\"number (123) is not a string\"\n",
        },
    ],
};

const SPLIT_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::SPLIT),
    canonical_name: "split",
    category: "text",
    summary: "The input string cut on the argument separator.",
    detail: "The `/` operator under another name, so the edges are its edges: an \
             empty separator cuts into codepoints, an empty input is `[]`, and a \
             separator at either end contributes an empty piece. A non-string on \
             either side raises `split input and separator must be strings`, \
             which is NOT the operator's `cannot be divided` message.",
};

const SPLIT_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::SPLIT),
    family: BuiltinFamilyId::new(id::SPLIT),
    canonical_name: "split",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "split(\", \")",
            input: "\"a,b, c, d, e,f\"",
            expected: "[\"a,b\",\"c\",\"d\",\"e,f\"]\n",
        },
        BuiltinExample {
            program: "split(\"\")",
            input: "\"abc\"",
            expected: "[\"a\",\"b\",\"c\"]\n",
        },
    ],
};

const CONTAINS_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::CONTAINS),
    canonical_name: "contains",
    category: "text",
    summary: "Whether the argument is contained in the input.",
    detail: "Four cases, applied recursively: two objects agree when every key \
             of the argument is present and its values are in the relation; two \
             arrays when every argument element is in the relation with SOME \
             input element (so `[1,2] | contains([2,1])` is `true`); two strings \
             by substring; anything else by value equality. A TOP-LEVEL kind \
             mismatch raises `<k> (<l>) and <k> (<r>) cannot have their \
             containment checked` — and the reference counts `true` and `false` as two \
             kinds, so `true | contains(false)` raises while `1 | contains(2)` \
             is `false`. One level down the same mismatch is just `false`.",
};

const CONTAINS_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::CONTAINS),
    family: BuiltinFamilyId::new(id::CONTAINS),
    canonical_name: "contains",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "contains([\"baz\",\"bar\"])",
            input: "[\"foobar\",\"foobaz\",\"blarp\"]",
            expected: "true\n",
        },
        BuiltinExample {
            program: "contains({foo: 12, bar: [1,2]})",
            input: "{\"foo\":12,\"bar\":[1,2,3]}",
            expected: "true\n",
        },
    ],
};

const INSIDE_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::INSIDE),
    canonical_name: "inside",
    category: "text",
    summary: "`contains` with the operands swapped.",
    detail: "The reference's `. as $x | xs | contains($x)`, so the INPUT is the inner value \
             and the argument is the container. The swap is observable in the \
             refusal, which names the argument first: `\"a\" | inside(1)` reports \
             `number (1) and string (\"a\")`.",
};

const INSIDE_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::INSIDE),
    family: BuiltinFamilyId::new(id::INSIDE),
    canonical_name: "inside",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Lowering,
    demand_transfer: DemandTransfer::ViaLowering,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "inside(\"foobar\")",
            input: "\"bar\"",
            expected: "true\n",
        },
        BuiltinExample {
            program: "inside([\"foobar\",\"foobaz\",\"blarp\"])",
            input: "[\"baz\",\"bar\"]",
            expected: "true\n",
        },
    ],
};
