//! Primitive value/type builtins: `length/0` and `keys/0`.
//!
//! One job: own the `length` and `keys` family/overload records and their pure evaluators. Both are `Evaluator`-kind,
//! exactly-one-output builtins the executor dispatches by id over a located or owned input. `length` applies the
//! sign-strip law to numbers (a non-negative located number's handle passes through unchanged; a negative one yields an
//! owned stripped number) and counts codepoints/elements/members otherwise; `keys` produces an owned, sorted key array.
//! Neither drives cardinality — the executor routes the single output and charges any owned result to the ledger.
//!
//! Negative space: the record prose defers non-JSON value kinds (bytes, temporal values, tags) under `length`/`keys`;
//! these evaluators cover exactly the JSON value domain and report a typed no-length/no-keys error otherwise.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use jqf_data::{Integer, IntrinsicTagSemantics, Number, ScalarView, Value, ValueKind, ValueView};
use jqf_resource::ResourceContext;

use super::id;
use crate::codec_result::EngineResult;
use crate::error::EngineRunError;
use crate::registry::record::{
    BuiltinExample, BuiltinExecution, BuiltinFamilyId, BuiltinFamilyRecord, BuiltinOverloadId, BuiltinOverloadRecord,
    DemandTransfer, Effects, SemanticRevision,
};
use crate::semantics::owned_kind;

/// The `length` family and `keys` family records.
pub const FAMILIES: &[BuiltinFamilyRecord] = &[LENGTH_FAMILY, KEYS_FAMILY, TYPE_FAMILY, TAG_FAMILY, NEGATE_FAMILY];

/// The `length/0` and `keys/0` overload records.
pub const OVERLOADS: &[BuiltinOverloadRecord] = &[
    LENGTH_OVERLOAD,
    KEYS_OVERLOAD,
    TYPE_OVERLOAD,
    TAG_OVERLOAD,
    NEGATE_OVERLOAD,
];

/// The crate-private execution payload of one core overload.
///
/// The family file stays free of `registry::dispatch` imports, so the payloads are named here and wrapped into the
/// engine-facing dispatch values at table build time. One variant per overload — five laws, five variants; unlike the
/// shared-shape families, a grouping variant would carry no information the payload does not.
#[derive(Clone, Copy, Debug)]
pub enum CorePayload {
    /// `length/0` — the sign-strip / element-count evaluator.
    Length,
    /// `keys/0` — the sorted key-array evaluator.
    Keys,
    /// `type/0` — the type-name evaluator.
    Type,
    /// `tag/0` — the non-core tag accessor.
    Tag,
    /// `_negate/0` — unary minus's value law (sign flip, spelling preserved).
    Negate,
}

/// The core execution payloads, aligned one-to-one with [`OVERLOADS`].
///
/// Every entry carries its overload id so the const coverage walk in `registry::dispatch` can prove pairwise alignment
/// — length equality alone would not catch a swapped pair.
pub const PAYLOADS: &[(u16, CorePayload)] = &[
    (id::LENGTH, CorePayload::Length),
    (id::KEYS, CorePayload::Keys),
    (id::TYPE, CorePayload::Type),
    (id::TAG, CorePayload::Tag),
    (id::NEGATE, CorePayload::Negate),
];

const TAG_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::TAG),
    canonical_name: "tag",
    category: "core",
    summary: "The input's tag, or null when it carries none.",
    detail: "`tag` emits the format-neutral text of a NON-CORE tag — a YAML \
             `!money`, a CBOR tag jqf does not resolve — and `null` for \
             everything else. A CORE tag is not a tag to a program: `!!str x` \
             IS a string, `type` already says so, and no residue is left to \
             read. This is the companion of the encode-side publish law: a \
             target without a native tag spelling publishes the bare payload, \
             and `tag` is how a program sees what it was published FROM. Total \
             over every value; it never raises.",
};

const TAG_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::TAG),
    family: BuiltinFamilyId::new(id::TAG),
    canonical_name: "tag",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    // The tag is an intrinsic fact of the input NODE: no payload at any depth is read to answer, which is exactly a
    // shallow observation.
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        // A tagless JSON value answers `null` — the negative arm. The positive arm (a non-core tag's text) needs a
        // tagged input, which the strict- JSON examples harness cannot express; codec-level tests own it.
        BuiltinExample {
            program: "tag",
            input: "[1,2]",
            expected: "null\n",
        },
    ],
};

const TYPE_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::TYPE),
    canonical_name: "type",
    category: "core",
    summary: "The input's type name, as one string.",
    detail: "`type` emits exactly one string: `\"null\"`, `\"boolean\"`, \
             `\"number\"`, `\"string\"`, `\"array\"` or `\"object\"` for the \
             core JSON kinds — plus `\"bytes\"` and the four temporal names \
             (`\"localdate\"`, `\"localtime\"`, `\"localdatetime\"`, \
             `\"offsetdatetime\"`) in sessions whose format can hold those \
             kinds. It is total over every value and never raises.",
};

const TYPE_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::TYPE),
    family: BuiltinFamilyId::new(id::TYPE),
    canonical_name: "type",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    // The NAME is derived from the value's KIND alone, never from its payload:
    // the input's shallow structure decides the whole output.
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "type",
            input: "[1,2]",
            expected: "\"array\"\n",
        },
        BuiltinExample {
            program: "type",
            input: "null",
            expected: "\"null\"\n",
        },
    ],
};

const LENGTH_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::LENGTH),
    canonical_name: "length",
    category: "core",
    summary: "The length of a value: element/member count, string codepoints, \
              absolute number, or zero for null.",
    detail: "An array's element count, an object's member count, a string's \
             Unicode-codepoint count (over decoded content), a byte string's \
             byte count, a number's absolute \
             value (sign stripped, exact class and spelling preserved), and 0 for \
             null. A boolean has no length. The remaining non-JSON value kinds are \
             deferred.",
};

const KEYS_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::KEYS),
    canonical_name: "keys",
    category: "core",
    summary: "The sorted keys of an object, or the indices of an array.",
    detail: "An object's keys sorted in ascending Unicode-codepoint (UTF-8 byte) \
             order, or an array's `[0, n)` index list. Null, booleans, numbers, \
             and strings have no keys. Non-JSON value kinds are deferred.",
};

const LENGTH_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::LENGTH),
    family: BuiltinFamilyId::new(id::LENGTH),
    canonical_name: "length",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::CountOfConstructedInput,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "length",
            input: "[1,2,3]",
            expected: "3\n",
        },
        BuiltinExample {
            program: "length",
            input: "\"résumé\"",
            expected: "6\n",
        },
    ],
};

const KEYS_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::KEYS),
    family: BuiltinFamilyId::new(id::KEYS),
    canonical_name: "keys",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    // The output is the input's member IDENTITIES — an object's keys, an array's index list — and no member payload
    // at any depth; a scalar's typed no-keys error is rendered from the scalar itself, which a shallow observation
    // carries whole.
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "keys",
            input: "{\"b\":2,\"a\":1}",
            expected: "[\"a\",\"b\"]\n",
        },
        BuiltinExample {
            program: "keys",
            input: "[10,20,30]",
            expected: "[0,1,2]\n",
        },
    ],
};

const NEGATE_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::NEGATE),
    canonical_name: "_negate",
    category: "core",
    summary: "Unary minus's value law: a number with its sign flipped.",
    detail: "`_negate` is what the unary `-` operator runs. A number's sign is \
             flipped with its exact class and spelling preserved (`1.500` \
             negates to `-1.500`), and a zero of either sign negates to a \
             POSITIVE zero (`-0.0` is `0.0`). Every other value kind is the \
             `cannot be negated` refusal, which is why `-` is not the \
             subtraction `0 - x`: that names an operand the program never \
             wrote. The reference carries a builtin of this exact name and keeps it out of \
             `builtins`, along with every other underscore-prefixed name.",
};

const NEGATE_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::NEGATE),
    family: BuiltinFamilyId::new(id::NEGATE),
    canonical_name: "_negate",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    // The answer is the input's own scalar, re-signed, and the refusal renders a bounded prefix of whatever the input
    // was: the law reads the value, so it demands the value.
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "-.",
            input: "1.500",
            expected: "-1.500\n",
        },
        BuiltinExample {
            program: "-.",
            input: "-0.0",
            expected: "0.0\n",
        },
    ],
};

/// Evaluates `_negate` over one owned input: unary minus's value law.
///
/// # Errors
///
/// Returns the `cannot be negated` refusal for every value kind but a number, or an allocation failure while building
/// the re-signed number.
pub fn negate(input: &EngineResult<'_>, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    match input {
        // A LOCATED number is re-signed from its borrowed coefficient and scale, never through an owned round trip:
        // materializing first canonicalizes a retained decimal, and `-1.500` would answer `-1.5`.
        EngineResult::Located(located) => {
            let view = payload_view(located)?;
            if let Some(ScalarView::Number(number)) = view.scalar().map_err(internal)? {
                return Ok(Value::Number(
                    number.try_negated().map_err(|_| EngineRunError::allocation_failure())?,
                ));
            }
            let kind = view.kind().map_err(internal)?;
            let operand = crate::error::message::dump_trunc_view(&view)?;
            Err(refuse_negation(kind, &operand, resources))
        }
        EngineResult::Owned(owned) => {
            if let Value::Number(number) = owned.untagged() {
                return Ok(Value::Number(
                    number.try_negated().map_err(|_| EngineRunError::allocation_failure())?,
                ));
            }
            let operand = crate::error::message::dump_trunc_owned(owned)?;
            Err(refuse_negation(owned.kind(), &operand, resources))
        }
    }
}

/// The `<kind> (<operand>) cannot be negated` message, as a catch-eligible raise.
fn refuse_negation(kind: ValueKind, operand: &str, resources: &ResourceContext<'_>) -> EngineRunError {
    match crate::error::message::not_negatable_message(kind, operand) {
        Ok(text) => crate::semantics::path::raise(&text, resources),
        Err(error) => error,
    }
}

/// Evaluates `length` over one input value.
///
/// A number yields an owned sign-stripped number preserving its exact class and spelling — `length` never returns the
/// input's own handle.
/// Strings count Unicode codepoints over decoded content, arrays and objects count elements/members, a byte string
/// counts its bytes, and null is `0`. A boolean has no length.
///
/// # Errors
///
/// Returns [`EngineRunError::NoLength`] for a value with no length (a boolean, or a non-JSON kind), or
/// [`EngineRunError::Codec`] for an internal contract violation over an otherwise valid document.
pub fn length(input: &EngineResult<'_>, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    match input {
        EngineResult::Located(located) => {
            let view = payload_view(located)?;
            located_length(&view, resources)
        }
        EngineResult::Owned(owned) => owned_length(owned, resources),
    }
}

/// The canonical text of a temporal scalar under `--types-as-strings`, or `None` when the scalar is not a temporal or
/// the flag is off. The located string operations read a temporal's kind as string under the flag; this is how they
/// then obtain the TEXT the way a real string's scalar does.
pub fn flag_temporal_text(
    scalar: Option<&ScalarView<'_>>,
    resources: &ResourceContext<'_>,
) -> Option<alloc::string::String> {
    if !resources.types_as_strings() {
        return None;
    }
    let mut out = alloc::string::String::new();
    let written = match scalar {
        Some(ScalarView::LocalDate(date)) => date.write_text(&mut out),
        Some(ScalarView::LocalTime(time)) => time.write_text(&mut out),
        Some(ScalarView::LocalDateTime(datetime)) => datetime.write_text(&mut out),
        Some(ScalarView::OffsetDateTime(datetime)) => datetime.write_text(&mut out),
        _ => return None,
    };
    written.ok().map(|()| out)
}

fn located_length(view: &ValueView<'_, '_>, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let kind = view.kind().map_err(internal)?;
    // Under `--types-as-strings` a located temporal's kind reads as string, and its canonical text IS the length.
    let kind = match kind {
        kind if kind.is_temporal() && resources.types_as_strings() => ValueKind::String,
        other => other,
    };
    match kind {
        // The null-as-empty-container cell: `null | length` answers `0` where the query assumed a container. The
        // zero-length family (`0`, `""`, `{}`, `[]`) is the reference's DEFINED length law and is deliberately not a
        // cell.
        ValueKind::Null => {
            let count = count_value(0)?;
            crate::error::mismatch::resolve_at(
                resources,
                crate::error::mismatch::MismatchCell::NullAsEmptyContainer,
                false,
                count,
            )
        }
        ValueKind::Bytes => {
            // The byte string's length is its byte count: every byte is one unit of a byte string, which is also what
            // `utf8bytelength` would answer for the same payload. It is the one non-JSON kind that actually carries a
            // natural length.
            let count = match view.scalar().map_err(internal)? {
                Some(ScalarView::Bytes(bytes)) => bytes.len(),
                _ => return Err(internal_contract("bytes node without a bytes scalar")),
            };
            Ok(count_value(count)?)
        }
        ValueKind::String => {
            let scalar = view.scalar().map_err(internal)?;
            let count = match scalar {
                Some(ScalarView::String(text)) => text.chars().count(),
                // Under `--types-as-strings` a located temporal's kind reads as string, and its canonical text IS the
                // length.
                _ => match flag_temporal_text(scalar.as_ref(), resources) {
                    Some(text) => text.chars().count(),
                    None => {
                        return Err(internal_contract("string node without a string scalar"));
                    }
                },
            };
            Ok(count_value(count)?)
        }
        ValueKind::Number => {
            let Some(ScalarView::Number(number)) = view.scalar().map_err(internal)? else {
                return Err(internal_contract("number node without a number scalar"));
            };
            // `length` on a number returns a FRESH number — `5 | length` is a new value, never the input's handle —
            // which path mode's emission check observes (`path(.a | length)` raises `Invalid path expression with
            // result 5`). The sign-strip law builds the owned number for BOTH signs: a non-negative's strip is
            // value-identical but freshly owned, so the located handle never passes through.
            let stripped = number
                .try_sign_stripped()
                .map_err(|_| EngineRunError::allocation_failure())?;
            Ok(Value::Number(stripped))
        }
        ValueKind::Array => {
            let array = view
                .array()
                .map_err(internal)?
                .ok_or_else(|| internal_contract("array node without an array projection"))?;
            Ok(count_value(array.len())?)
        }
        ValueKind::Object => {
            let object = view
                .object()
                .map_err(internal)?
                .ok_or_else(|| internal_contract("object node without an object projection"))?;
            Ok(count_value(object.len())?)
        }
        kind => Err(EngineRunError::NoLength {
            actual_type: kind,
            operand: crate::error::message::dump_trunc_view(view)?,
        }),
    }
}

/// `length` over an ALREADY-owned value, shared with `transpose`'s measuring phase: the pivot needs every row's length
/// before it indexes any of them, and the row lengths are the `length` law's, boolean refusal included.
pub fn owned_length(value: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    match value {
        // The null-as-empty-container cell's owned half; see `located_length`.
        Value::Null => crate::error::mismatch::resolve_at(
            resources,
            crate::error::mismatch::MismatchCell::NullAsEmptyContainer,
            false,
            count_value(0),
        )?,
        Value::String(text) => count_value(text.chars().count()),
        Value::Bytes(bytes) => count_value(bytes.len()),
        Value::Array(array) => count_value(array.len()),
        Value::Object(object) => count_value(object.len()),
        Value::Number(number) => {
            // `length` on a number returns a FRESH number — `5 | length` is a new value, never the input's handle —
            // which path mode's emission check observes (`path(5 | length)` raises `Invalid path expression with result
            // 5`). `try_sign_stripped` builds the owned number for BOTH signs; a non-negative's strip is
            // value-identical but freshly owned, so the input's allocation never rides through.
            Ok(Value::Number(
                number
                    .try_sign_stripped()
                    .map_err(|_| EngineRunError::allocation_failure())?,
            ))
        }
        Value::Tagged { payload, .. } => owned_length(payload, resources),
        other => Err(EngineRunError::NoLength {
            actual_type: owned_kind(other),
            operand: crate::error::message::dump_trunc_owned(other)?,
        }),
    }
}

/// Which order an object's keys are published in.
///
/// The two orders are the ONLY difference between `keys` and `keys_unsorted`:
/// an array answers `[0, n)` either way, and every keyless value raises the same error. One evaluator with this
/// parameter is what stops the two spellings from drifting on the parts they share.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyOrder {
    /// Ascending UTF-8-byte (codepoint) order — `keys`.
    Sorted,
    /// The object's own insertion order — `keys_unsorted`.
    Insertion,
}

/// Evaluates `keys`/`keys_unsorted` over one input value, producing an owned key array.
///
/// An object yields its keys in `order`; an array yields its `[0, n)` index list. Every other value has no keys.
///
/// # Errors
///
/// Returns [`EngineRunError::NoKeys`] for a value with no keys, or [`EngineRunError::Codec`] for an internal contract
/// violation.
pub fn keys(
    input: &EngineResult<'_>,
    order: KeyOrder,
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    match input {
        EngineResult::Located(located) => {
            let view = payload_view(located)?;
            match view.kind().map_err(internal)? {
                ValueKind::Object => {
                    let object = view
                        .object()
                        .map_err(internal)?
                        .ok_or_else(|| internal_contract("object node without an object projection"))?;
                    let mut names = Vec::new();
                    for entry in object.iter() {
                        let entry = entry.map_err(internal)?;
                        push_str(&mut names, entry.key())?;
                    }
                    key_array(names, order, resources)
                }
                ValueKind::Array => {
                    let array = view
                        .array()
                        .map_err(internal)?
                        .ok_or_else(|| internal_contract("array node without an array projection"))?;
                    index_array(array.len(), resources)
                }
                kind => Err(EngineRunError::NoKeys {
                    actual_type: kind,
                    operand: crate::error::message::dump_trunc_view(&view)?,
                }),
            }
        }
        EngineResult::Owned(owned) => owned_keys(owned, order, resources),
    }
}

fn owned_keys(value: &Value, order: KeyOrder, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    match value {
        Value::Object(object) => {
            let mut names = Vec::new();
            let mut index = 0;
            while let Some(entry) = object.get_index(index) {
                push_str(&mut names, entry.key())?;
                index += 1;
            }
            key_array(names, order, resources)
        }
        Value::Array(array) => index_array(array.len(), resources),
        Value::Tagged { payload, .. } => owned_keys(payload, order, resources),
        other => Err(EngineRunError::NoKeys {
            actual_type: owned_kind(other),
            operand: crate::error::message::dump_trunc_owned(other)?,
        }),
    }
}

/// `type/0` — the input's type NAME.
///
/// The core names are exactly six: `"null"`, `"boolean"`, `"number"`, `"string"`, `"array"`, `"object"`. Those are the
/// only kinds a JSON codec can produce.
///
/// jqf's value vocabulary is wider (bytes, the temporal kinds), and the TOML codec CAN produce the temporal kinds —
/// so this law was extended deliberately when that codec landed: each temporal kind gets its own lowercase name
/// (`"localdate"`, `"localtime"`, `"localdatetime"`, `"offsetdatetime"`), and bytes get `"bytes"`. A new name is not a
/// silent lie to a program branching on the answer — folding a temporal into `"string"` or `"number"` would be —
/// and JSON sessions can never observe the extension, so the reference corpus is untouched. An unnameable kind remains
/// a loud internal contract violation, and the error says so.
pub fn type_name(input: &EngineResult<'_>, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let name = type_of(input, resources)?.name();
    Value::try_string(name).map_err(|_| internal_contract("type/0 name allocation"))
}

/// `tag/0` — the input's NON-CORE tag as a string, or null when it carries none.
///
/// The line this draws is the same one the value model draws. A CORE tag is a resolution instruction the decoder
/// already obeyed: `!!str 12.5` decodes to the string `"12.5"`, and after that there is no tag left for a program to
/// observe — `type` is the complete answer. A NON-CORE tag survives decode precisely because nothing could be done
/// with it, and it is the one a program needs to see: it is what `type` cannot report, what an encoder without a native
/// tag spelling drops when it publishes the payload, and (until now) a fact the value model carried that no program
/// could read.
///
/// Null rather than an empty string for untagged, because a tag's text is never empty (`TagError::Empty` is a
/// construction failure) — an empty answer would be a value no tagged input can produce, while `null` is the tree's
/// standing spelling for an absent fact.
pub fn tag_name(input: &EngineResult<'_>, _resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let Some(tag) = non_core_tag(input)? else {
        return Ok(Value::Null);
    };
    Value::try_string(tag).map_err(|_| EngineRunError::allocation_failure())
}

/// Reads the input's non-core tag text from whichever authority holds it.
///
/// The owned model wraps ONLY non-core tags (a core tag is resolved into the value's kind at decode), so the wrapper's
/// presence is the whole test there.
/// A located node keeps both classes as intrinsic facts and must be asked which class it is.
fn non_core_tag<'input>(input: &'input EngineResult<'_>) -> Result<Option<&'input str>, EngineRunError> {
    match input {
        EngineResult::Owned(Value::Tagged { tag, .. }) => Ok(Some(tag.as_str())),
        EngineResult::Owned(_) => Ok(None),
        // The OUTERMOST node is the authority here, not the payload: descending is what `type_of` does, and doing it
        // too would answer `null` for every tagged value in the one document shape that spells a tag as a layer.
        EngineResult::Located(located) => {
            let view = located
                .product()
                .document()
                .value_view(located.node())
                .map_err(internal)?;
            if view.tag_semantics().map_err(internal)? != Some(IntrinsicTagSemantics::Tagged) {
                return Ok(None);
            }
            let tag = view
                .tag()
                .map_err(internal)?
                .ok_or_else(|| internal_contract("a node with Tagged semantics has no tag"))?;
            Ok(Some(tag.as_str()))
        }
    }
}

/// One of the type names, as a value rather than as a heap string.
///
/// This is the shared half of the `type/0` law: [`type_name`] spells it into an owned `Value::String`, and the
/// kind-filter family ([`super::kinds`]) tests it directly. Splitting the naming from the spelling is what lets
/// `objects` decide with no allocation while still deciding by EXACTLY the law `select(type == "object")` decided by.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TypeName {
    Null,
    Boolean,
    Number,
    String,
    Array,
    Object,
    /// The byte-string kind's extended name (not a JSON kind).
    Bytes,
    /// The four temporal kinds' extended names (TOML codec vocabulary).
    LocalDate,
    LocalTime,
    LocalDateTime,
    OffsetDateTime,
}

impl TypeName {
    /// The spelling of this type.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Boolean => "boolean",
            Self::Number => "number",
            Self::String => "string",
            Self::Array => "array",
            Self::Object => "object",
            Self::Bytes => "bytes",
            Self::LocalDate => "localdate",
            Self::LocalTime => "localtime",
            Self::LocalDateTime => "localdatetime",
            Self::OffsetDateTime => "offsetdatetime",
        }
    }
}

/// Reads the input's type. The six JSON kinds map to the core names; the temporal kinds the TOML codec can produce map
/// to their own extended names (the deliberate law extension [`type_name`] documents); no kind is left unnameable by a
/// codec that can produce it.
pub fn type_of(input: &EngineResult<'_>, resources: &ResourceContext<'_>) -> Result<TypeName, EngineRunError> {
    let kind = match input {
        // A tag LAYER is kindless by construction, so the descent to the payload is what makes this total — the same
        // payload transparency `owned_kind` spells for `Value::Tagged`, over the other authority.
        EngineResult::Located(located) => payload_view(located)?.kind().map_err(internal)?,
        EngineResult::Owned(value) => owned_kind(value),
    };
    match kind {
        ValueKind::Null => Ok(TypeName::Null),
        ValueKind::Bool => Ok(TypeName::Boolean),
        ValueKind::Number => Ok(TypeName::Number),
        ValueKind::String => Ok(TypeName::String),
        ValueKind::Array => Ok(TypeName::Array),
        ValueKind::Object => Ok(TypeName::Object),
        ValueKind::Bytes => Ok(TypeName::Bytes),
        // Under `--types-as-strings` the extended temporal kinds read as their canonical text, so `type` names them
        // string — the flag's whole point is that a jq program sees only the six jq types.
        kind if kind.is_temporal() && resources.types_as_strings() => Ok(TypeName::String),
        ValueKind::LocalDate => Ok(TypeName::LocalDate),
        ValueKind::LocalTime => Ok(TypeName::LocalTime),
        ValueKind::LocalDateTime => Ok(TypeName::LocalDateTime),
        ValueKind::OffsetDateTime => Ok(TypeName::OffsetDateTime),
    }
}

/// The payload-transparent view of a located node.
///
/// A format that resolves one tag per value (YAML) records it as an intrinsic fact ON the node, so the node already IS
/// its own payload. A format whose tags nest and can tag a container (CBOR's uninterpreted tags) cannot say that in one
/// node and builds jqf-data's kindless tag-LAYER instead, which has to be descended before anything asks the node what
/// it is. The one descent implementation lives on the document ([`jqf_data::Document::payload_view`]); this engine copy
/// delegates to it.
fn payload_view<'located, 'source>(
    located: &'located jqf_codec_core::LocatedProduct<'source>,
) -> Result<ValueView<'located, 'source>, EngineRunError> {
    located
        .product()
        .document()
        .payload_view(located.node())
        .map_err(internal)
}

/// Builds an owned `Value::Array` of one object's keys, in `order`.
///
/// [`KeyOrder::Sorted`] uses the allocation-conscious unstable byte-order sort; UTF-8 byte order is codepoint order, so
/// `str`'s own `Ord` is the collation.
fn key_array(
    mut names: Vec<String>,
    order: KeyOrder,
    _resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    if order == KeyOrder::Sorted {
        names.sort_unstable();
    }
    let mut array =
        jqf_data::Array::try_with_capacity(names.len()).map_err(|_| EngineRunError::allocation_failure())?;
    for name in names {
        let name = Value::try_string(&name).map_err(|_| EngineRunError::allocation_failure())?;
        array.try_push(name).map_err(|_| EngineRunError::allocation_failure())?;
    }
    Ok(Value::Array(array))
}

/// Builds an owned `Value::Array` of the `[0, len)` array indices.
fn index_array(len: usize, _resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let mut array = jqf_data::Array::try_with_capacity(len).map_err(|_| EngineRunError::allocation_failure())?;
    for index in 0..len {
        let index = count_value(index)?;
        array
            .try_push(index)
            .map_err(|_| EngineRunError::allocation_failure())?;
    }
    Ok(Value::Array(array))
}

/// Copies `text` into `names` with fallible reservations at both levels.
fn push_str(names: &mut Vec<String>, text: &str) -> Result<(), EngineRunError> {
    let mut owned = String::new();
    owned
        .try_reserve_exact(text.len())
        .map_err(|_| EngineRunError::allocation_failure())?;
    owned.push_str(text);
    names.try_reserve(1).map_err(|_| EngineRunError::allocation_failure())?;
    names.push(owned);
    Ok(())
}

/// An exact non-negative integer count as an owned `Value::Number`.
///
/// `Integer::from_i64` renders the decimal straight into the integer's own inline storage, so a count never becomes a
/// heap `String` on the way to being parsed back out of one. The wider arm cannot be reached by a count of anything
/// that fits memory, and is kept exact rather than refused.
fn count_value(count: usize) -> Result<Value, EngineRunError> {
    let integer = match i64::try_from(count) {
        Ok(value) => Integer::from_i64(value),
        Err(_) => Integer::parse(&count.to_string()).map_err(|_| EngineRunError::allocation_failure())?,
    };
    Ok(Value::Number(
        Number::try_integer_unaccounted(integer).map_err(|_| EngineRunError::allocation_failure())?,
    ))
}

fn internal(_: jqf_data::DataError) -> EngineRunError {
    internal_contract("builtin evaluation over a valid located document failed")
}

fn internal_contract(contract: &'static str) -> EngineRunError {
    EngineRunError::internal_contract(contract)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

    fn resources() -> ResourceContext<'static> {
        ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                .expect("account"),
            &ContinueControl,
            WorkMeter::try_new_v1(4_096).expect("work"),
        )
        .expect("resources")
    }

    fn owned(value: Value) -> EngineResult<'static> {
        EngineResult::Owned(value)
    }

    fn owned_type_name(value: Value) -> String {
        let resources = resources();
        let Value::String(text) = type_name(&owned(value), &resources).expect("type name") else {
            panic!("type name is not a string");
        };
        alloc::string::String::from(text.as_str())
    }

    /// `type/0`'s answer set is a CHECKED invariant: eleven truthful names by default (a twelfth kind a codec adds
    /// tomorrow must not silently widen `type` again), and `--types-as-strings` closes it to jq's six — the
    /// product-path twin of this bound is `jqf-cli/tests/types_as_strings.rs`, which samples every program-visible
    /// value source (materialized decoded values AND builtin outputs). The mapping from each `ValueKind` to one of
    /// these names is pinned by `type_names_the_temporal_kinds` below (one value per kind); this test pins the answer
    /// VOCABULARY itself.
    #[test]
    fn types_codomain_is_exactly_the_ruled_set() {
        use alloc::collections::BTreeSet;
        let names: BTreeSet<&'static str> = [
            TypeName::Null,
            TypeName::Boolean,
            TypeName::Number,
            TypeName::String,
            TypeName::Array,
            TypeName::Object,
            TypeName::Bytes,
            TypeName::LocalDate,
            TypeName::LocalTime,
            TypeName::LocalDateTime,
            TypeName::OffsetDateTime,
        ]
        .into_iter()
        .map(TypeName::name)
        .collect();
        let ruled: BTreeSet<&'static str> = [
            "null",
            "boolean",
            "number",
            "string",
            "array",
            "object",
            "bytes",
            "localdate",
            "localtime",
            "localdatetime",
            "offsetdatetime",
        ]
        .into_iter()
        .collect();
        assert_eq!(names, ruled);
    }

    /// The extended type law: the TOML codec can produce the temporal kinds, so `type/0` names them rather than raising
    /// the unnameable-kind contract violation. JSON sessions can never observe the extension.
    #[test]
    fn type_names_the_temporal_kinds() {
        let date = jqf_data::LocalDate::new(1979, 5, 27).expect("date");
        let time = jqf_data::LocalTime::new(7, 32, 0, jqf_data::FractionalSecond::default()).expect("time");
        let local = Value::LocalDate(date);
        assert_eq!(owned_type_name(local), "localdate");
        let owned_time = Value::LocalTime(time.clone());
        assert_eq!(owned_type_name(owned_time), "localtime");
        let datetime = Value::LocalDateTime(jqf_data::LocalDateTime {
            date,
            time: time.clone(),
        });
        assert_eq!(owned_type_name(datetime), "localdatetime");
        let offset = Value::OffsetDateTime(jqf_data::OffsetDateTime {
            local: jqf_data::LocalDateTime {
                date,
                time: time.clone(),
            },
            offset: jqf_data::UtcOffset::KnownSeconds(jqf_data::KnownUtcOffset::new(0).expect("utc")),
        });
        assert_eq!(owned_type_name(offset), "offsetdatetime");
        // The six core names are untouched.
        assert_eq!(owned_type_name(Value::Null), "null");
        assert_eq!(owned_type_name(Value::Bool(true)), "boolean");
        assert_eq!(
            owned_type_name(Value::try_string("x").expect("string value"),),
            "string"
        );
    }

    /// `tostring` over a temporal is its CANONICAL TEXT, the way `tostring` over a string is the string: the kind
    /// already is text, so quoting it as JSON (or, as it did before, publishing `"null"`) loses the one rendering the
    /// value has.
    #[test]
    fn tostring_over_a_temporal_renders_exact_canonical_text() {
        let date = jqf_data::LocalDate::new(1979, 5, 27).expect("date");
        let time = jqf_data::LocalTime::new(7, 32, 0, jqf_data::FractionalSecond::default()).expect("time");
        let resources = resources();
        let value = Value::OffsetDateTime(jqf_data::OffsetDateTime {
            local: jqf_data::LocalDateTime {
                date,
                time: time.clone(),
            },
            offset: jqf_data::UtcOffset::KnownSeconds(jqf_data::KnownUtcOffset::new(0).expect("utc")),
        });
        let text = super::super::text::tostring(&value, &resources).expect("tostring");
        let Value::String(text) = text else {
            panic!("tostring is not a string");
        };
        assert_eq!(text.as_str(), "1979-05-27T07:32:00Z");
    }
}
