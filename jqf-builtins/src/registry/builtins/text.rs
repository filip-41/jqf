//! Stringifying builtins: `tostring/0`, `tojson/0`, `join/1`.
//!
//! One job: own the family and overload records for the three builtins whose answer is TEXT, plus the three evaluator
//! payloads that turn a value into text. All three are defined against JSON's own spelling — not against the
//! session's output codec, which is why they consume [`crate::semantics::render`] rather than a codec handle.
//!
//! `tostring` and `tojson` differ on STRING, TEMPORAL, and BYTES: those three pass through `tostring` unquoted and are
//! re-quoted by `tojson`. Every other kind renders identically, which is why they share one renderer and not one
//! evaluator.
//!
//! `join/1` is the reference definition read as a SINGLE BUFFER:
//!
//! ```text
//! def join($x): reduce .[] as $i (null; (if . == null then "" else . + $x end)
//!   + ($i | if . == null then "" elif type == "number" then tojson
//!           elif type == "boolean" then tojson else . end)) // "";
//! ```
//!
//! Every edge case that definition owns is TRANSCRIBED rather than re-derived, because they are not obvious ones: an
//! empty input is `""` (the `//`), a `null` ITEM is the empty string while a `null` SEPARATOR is `+`'s identity, an
//! array or object item is NOT converted and so fails `+` with `string ("") and array ([1]) cannot be added`, and a
//! non-string separator survives exactly one element (`["a"] | join(1)` is `"a"`, `["a","b"] | join(1)` raises). The
//! two refusals a buffer cannot answer on its own are DELEGATED to the code the expansion itself reached —
//! [`message`]'s iterate spelling and [`apply_binary`] — so no message here can drift from `+`'s.
//!
//! Folding the expansion instead cost O(n²): `+` on two strings builds a THIRD string, so an n-element join copied the
//! whole accumulated prefix once per element. `jqf-data`'s text payload is an immutable shared slice with no spare
//! capacity, so there is no in-place `+` to reach for the way there is for an array or an object — the buffer has to
//! be the builtin's own.
//!
//! Negative space: it owns no escaping and no number spelling — both are [`crate::semantics::render`]'s — and it
//! owns no error message.

use alloc::string::String;

use jqf_data::{Array, Object, Value};
use jqf_resource::ResourceContext;

use super::id;
use crate::error::{EngineRunError, apply_binary, message};
use crate::registry::record::{
    BuiltinExample, BuiltinExecution, BuiltinFamilyId, BuiltinFamilyRecord, BuiltinOverloadId, BuiltinOverloadRecord,
    DemandTransfer, Effects, ParameterKind, SemanticRevision,
};
use crate::semantics::binary::BinaryKind;
use crate::semantics::path::raise;
use crate::semantics::render;

/// The stringifying family records.
pub const FAMILIES: &[BuiltinFamilyRecord] = &[TOSTRING_FAMILY, TOJSON_FAMILY, JOIN_FAMILY];

/// The stringifying overload records.
pub const OVERLOADS: &[BuiltinOverloadRecord] = &[TOSTRING_OVERLOAD, TOJSON_OVERLOAD, JOIN_OVERLOAD];

/// The stringifier execution payloads, aligned one-to-one with [`OVERLOADS`].
///
/// Every entry carries its overload id so the const coverage walk in `registry::dispatch` can prove pairwise alignment.
#[derive(Clone, Copy, Debug)]
pub enum TextPayload {
    /// `tostring/0` — a string unchanged, anything else as compact JSON.
    ToString,
    /// `tojson/0` — compact JSON, always.
    ToJson,
    /// `join/1` — the separator-interleaved concatenation.
    Join,
}

pub const PAYLOADS: &[(u16, TextPayload)] = &[
    (id::TOSTRING, TextPayload::ToString),
    (id::TOJSON, TextPayload::ToJson),
    (id::JOIN, TextPayload::Join),
];

/// Evaluates `tojson` over an owned input: its compact JSON, always quoted for a string.
///
/// # Errors
///
/// Returns an allocation failure, or an internal-contract error for a number carrying none of `jqf-data`'s three
/// representations.
pub fn tojson(input: &Value, _resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let text = render::to_json(input)?;
    Value::try_string(&text).map_err(|_| EngineRunError::allocation_failure())
}

/// Evaluates `tostring` over an owned input: a string unchanged, a temporal as its canonical text, bytes as unpadded
/// base64url, anything else as its compact JSON.
///
/// A temporal joins the string arm rather than the JSON one for the same reason a string is there: it already IS text,
/// so `tostring` must hand back the text and not the quoted JSON literal `tojson` builds around it. That keeps
/// `tostring`/`@text`/`"\(…)"` agreeing with each other and with `tojson` differing by exactly the quotes — the one
/// difference this pair is defined to have.
///
/// # Errors
///
/// As [`tojson`].
pub fn tostring(input: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let input = input.untagged();
    if let Value::String(text) = input {
        return Ok(Value::String(text.clone_shared()));
    }
    if let Some(text) = render::temporal_text(input)? {
        return Value::try_string(&text).map_err(|_| EngineRunError::allocation_failure());
    }
    if let Some(text) = render::bytes_text(input)? {
        return Value::try_string(&text).map_err(|_| EngineRunError::allocation_failure());
    }
    tojson(input, resources)
}

/// Evaluates `join($separator)` over an owned input, into ONE output buffer.
///
/// The loop IS the expansion's fold, with the accumulator spelled as a `String` the elements are written into:
/// `position > 0` is the expansion's `. == null` test (the accumulator is `null` before the first element and a string
/// ever after), and an empty input never enters the loop, so the empty buffer it publishes is the expansion's `//`
/// answer.
///
/// # Errors
///
/// Returns the iterate refusal for a non-container input, `+`'s own type mismatch for a separator or an element the
/// expansion would have handed it unconverted, or an allocation failure.
pub fn join(input: &Value, separator: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let children = Children::of(input, resources)?;
    let mut text = String::new();
    text.try_reserve(children.estimate(separator))
        .map_err(|_| EngineRunError::allocation_failure())?;
    for position in 0..children.len() {
        let item = children
            .get(position)
            .ok_or_else(|| EngineRunError::internal_contract("a join child vanished mid-walk"))?;
        if position > 0 {
            push_separator(&mut text, separator, resources)?;
        }
        push_item(&mut text, item, resources)?;
    }
    Value::try_string(&text).map_err(|_| EngineRunError::allocation_failure())
}

/// The input's children, in the order `.[]` yields them.
///
/// Indexed rather than iterated so that both containers drive ONE loop: their element types differ (a value against an
/// entry) and the join law does not.
enum Children<'a> {
    /// An array input: its elements.
    Array(&'a Array),
    /// An object input: its VALUES, in insertion order.
    Object(&'a Object),
}

impl<'a> Children<'a> {
    /// The children of a container input, or the iterate refusal `.[]` raises.
    fn of(input: &'a Value, resources: &ResourceContext<'_>) -> Result<Self, EngineRunError> {
        match input.untagged() {
            Value::Array(array) => Ok(Self::Array(array)),
            Value::Object(object) => Ok(Self::Object(object)),
            other => {
                let operand = message::dump_trunc_owned(other)?;
                let text = message::iterate_message(other.kind(), &operand)?;
                Err(raise(&text, resources))
            }
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Array(array) => array.len(),
            Self::Object(object) => object.len(),
        }
    }

    fn get(&self, position: usize) -> Option<&'a Value> {
        match self {
            Self::Array(array) => array.get(position),
            Self::Object(object) => object.get_index(position).map(jqf_data::ObjectEntry::value),
        }
    }

    /// A capacity for the output buffer: every STRING child's bytes plus one separator between each pair.
    ///
    /// A reservation, not a bound. A number or a boolean child renders to a length this cannot know without rendering
    /// it, so the buffer still grows for those — which is why every write goes through [`push_text`] and its fallible
    /// growth rather than trusting this figure.
    fn estimate(&self, separator: &Value) -> usize {
        let between = match separator.untagged() {
            Value::String(text) => text.len().saturating_mul(self.len().saturating_sub(1)),
            _ => 0,
        };
        (0..self.len())
            .filter_map(|position| match self.get(position)?.untagged() {
                Value::String(text) => Some(text.len()),
                _ => None,
            })
            .fold(between, usize::saturating_add)
    }
}

/// Writes the separator between two elements: the expansion's `. + $x` over an accumulator that is already a string.
///
/// `null` is `+`'s identity and writes nothing; a string writes its text; every other separator is the mismatch `+`
/// raises, with the buffer SO FAR as the left operand — which is why `["a","b"] | join(1)` names `"a"` and `["a"] |
/// join(1)` never asks at all.
fn push_separator(text: &mut String, separator: &Value, resources: &ResourceContext<'_>) -> Result<(), EngineRunError> {
    match separator.untagged() {
        Value::Null => Ok(()),
        Value::String(piece) => push_text(text, piece),
        _ => Err(refuse(text, separator, resources)),
    }
}

/// Writes one element: the expansion's conversion, then the `+` that appends it.
///
/// The conversion is classified on the UNTAGGED value, exactly as `type` is, and renders the ORIGINAL, exactly as
/// `tojson` does. A string is taken as it is, `null` becomes nothing at all, and anything else — an array, an object,
/// or one of jqf's non-JSON kinds — reaches `+` unconverted and is refused there.
fn push_item(text: &mut String, item: &Value, resources: &ResourceContext<'_>) -> Result<(), EngineRunError> {
    match item.untagged() {
        Value::Null => Ok(()),
        Value::String(piece) => push_text(text, piece),
        Value::Number(_) | Value::Bool(_) => push_text(text, &render::to_json(item)?),
        _ => Err(refuse(text, item, resources)),
    }
}

/// The refusal `+` raises for an operand it cannot add to the buffer.
///
/// The buffer is rebuilt as a value and handed to [`apply_binary`] rather than having its message spelled here: that
/// call IS the `+` the expansion reached, so the operand rendering, the truncation, and the failure class are the
/// operator's own. It is a cold path — the pair it applies always fails.
fn refuse(text: &str, operand: &Value, resources: &ResourceContext<'_>) -> EngineRunError {
    let Ok(accumulated) = Value::try_string(text) else {
        return EngineRunError::allocation_failure();
    };
    match apply_binary(BinaryKind::Add, &accumulated, operand, resources) {
        Ok(_) => EngineRunError::internal_contract("a refused join operand added cleanly"),
        Err(error) => error,
    }
}

/// Appends one piece, growing the buffer fallibly.
fn push_text(text: &mut String, piece: &str) -> Result<(), EngineRunError> {
    text.try_reserve(piece.len())
        .map_err(|_| EngineRunError::allocation_failure())?;
    text.push_str(piece);
    Ok(())
}

const TOSTRING_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::TOSTRING),
    canonical_name: "tostring",
    category: "text",
    summary: "The input as a string: a string unchanged, anything else as \
              compact JSON.",
    detail: "A string is NOT re-quoted, which is the one place `tostring` and \
             `tojson` disagree. The rendering is JSON's own text and does not \
             depend on the session's output codec.",
};

const TOSTRING_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::TOSTRING),
    family: BuiltinFamilyId::new(id::TOSTRING),
    canonical_name: "tostring",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "[.[] | tostring]",
            input: "[1,\"1\",[1],null,true]",
            expected: "[\"1\",\"1\",\"[1]\",\"null\",\"true\"]\n",
        },
        BuiltinExample {
            program: "tostring",
            input: "{\"a\":1}",
            expected: "\"{\\\"a\\\":1}\"\n",
        },
    ],
};

const TOJSON_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::TOJSON),
    canonical_name: "tojson",
    category: "text",
    summary: "The input's compact JSON text, as a string.",
    detail: "Unlike `tostring`, a string input is re-quoted and re-escaped, so \
             `tojson | tojson` doubles the quoting.",
};

const TOJSON_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::TOJSON),
    family: BuiltinFamilyId::new(id::TOJSON),
    canonical_name: "tojson",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "tojson",
            input: "{\"a\":[1,2]}",
            expected: "\"{\\\"a\\\":[1,2]}\"\n",
        },
        BuiltinExample {
            program: "tojson",
            input: "\"x\"",
            expected: "\"\\\"x\\\"\"\n",
        },
    ],
};

const JOIN_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::JOIN),
    canonical_name: "join",
    category: "text",
    summary: "The input's elements concatenated with a separator between them.",
    detail: "Only `null`, numbers and booleans are converted (`null` to the \
             empty string, the other two through `tojson`); a string is used as \
             it is, and an array or object is handed to `+` unconverted, which \
             raises. An empty input is the empty string, and an OBJECT input \
             joins its values.",
};

const JOIN_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::JOIN),
    family: BuiltinFamilyId::new(id::JOIN),
    canonical_name: "join",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "join(\",\")",
            input: "[1,null,\"a\"]",
            expected: "\"1,,a\"\n",
        },
        BuiltinExample {
            program: "join(\",\")",
            input: "[]",
            expected: "\"\"\n",
        },
    ],
};
