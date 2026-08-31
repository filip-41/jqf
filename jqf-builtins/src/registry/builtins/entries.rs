//! The entries family: `keys_unsorted/0`, `to_entries/0`, `from_entries/0`, `with_entries/1`.
//!
//! One job: own the family and overload records for the vocabulary that turns an object into an ARRAY OF PAIRS and
//! back, plus the two evaluator payloads that do the turning. `keys_unsorted` lives here rather than beside `keys`
//! because it is `to_entries`' own first step (`def to_entries: [keys_unsorted[] as $k | {key: $k, value: .[$k]}]`),
//! which is also why the two share one evaluator parameterized by [`super::core::KeyOrder`].
//!
//! **`from_entries`' two lookups are NOT the same kind of lookup, and that is the whole law.** The reference spells the
//! key as a `//` chain and the value as a `has()` chain:
//!
//! ```text
//! key:   .key // .Key // .name // .Name
//! value: if has("value") then .value elif has("Value") then .Value else null end
//! ```
//!
//! So a FALSY key falls through to the next alias while a falsy VALUE does not:
//!
//! ```text
//! [{"key":false,"name":"z","value":1}] | from_entries  -> {"z":1}
//! [{"key":false,"value":1}]            | from_entries  # Cannot use null (null) as object key
//! [{"key":"x","value":false,"Value":9}]| from_entries  -> {"x":false}
//! ```
//!
//! The second line is the one an alias TABLE gets wrong: the chain exhausted, so the offending key the reference names
//! is `null` — the absent `.Name` — and not the `false` that was actually present. The alias set is closed and was
//! re-derived from the reference rather than ported: `key`, `Key`, `name`, `Name` for the key and `value`, `Value` for
//! the value, with `k`, `K`, `KEY`, `NAME`, `v`, `V` and `VALUE` deliberately absent.
//!
//! `with_entries(f)` is a REGISTERED LOWERING of `to_entries | map(f) | from_entries`, which is where its own surprises
//! come from: `map` collects ALL of `f`'s outputs, so `with_entries(.value+=1, .value+=2)` produces ONE object built
//! from four entries, and a `select` that drops an entry drops the whole key.
//!
//! Negative space: it owns no ordering (the object's own insertion order is the only order here) and renders no message
//! beyond naming the class.

use alloc::vec::Vec;

use jqf_data::{Integer, Number, ObjectBuilder, ObjectKey, Value};
use jqf_resource::ResourceContext;

use super::id;
use crate::error::EngineRunError;
use crate::error::message;
use crate::registry::record::{
    BuiltinExample, BuiltinExecution, BuiltinFamilyId, BuiltinFamilyRecord, BuiltinOverloadId, BuiltinOverloadRecord,
    DemandTransfer, Effects, ParameterKind, SemanticRevision,
};
use crate::semantics::path::raise;

/// The entries family records.
pub const FAMILIES: &[BuiltinFamilyRecord] = &[
    KEYS_UNSORTED_FAMILY,
    TO_ENTRIES_FAMILY,
    FROM_ENTRIES_FAMILY,
    WITH_ENTRIES_FAMILY,
];

/// The entries overload records.
pub const OVERLOADS: &[BuiltinOverloadRecord] = &[
    KEYS_UNSORTED_OVERLOAD,
    TO_ENTRIES_OVERLOAD,
    FROM_ENTRIES_OVERLOAD,
    WITH_ENTRIES_OVERLOAD,
];

/// The entries execution payloads, aligned one-to-one with [`OVERLOADS`].
///
/// The family spans evaluators and one lowering (`with_entries`), so it names its own payload enum;
/// `registry::dispatch` wraps it at table build time. Every entry carries its overload id so the const coverage walk
/// there can prove pairwise alignment.
#[derive(Clone, Copy, Debug)]
pub enum EntriesPayload {
    /// `keys_unsorted/0` — the insertion-order key array.
    KeysUnsorted,
    /// `to_entries/0` — the `{key,value}` array.
    ToEntries,
    /// `from_entries/0` — the `{key,value}` array's inverse.
    FromEntries,
    /// `with_entries/1` — expands to `to_entries | map(f) | from_entries`.
    WithEntries,
}

pub const PAYLOADS: &[(u16, EntriesPayload)] = &[
    (id::KEYS_UNSORTED, EntriesPayload::KeysUnsorted),
    (id::TO_ENTRIES, EntriesPayload::ToEntries),
    (id::FROM_ENTRIES, EntriesPayload::FromEntries),
    (id::WITH_ENTRIES, EntriesPayload::WithEntries),
];

/// The KEY aliases, in the `//` chain order. Closed: an alias not here does not work in the reference, and the chain
/// ORDER decides which of several present-and- truthy aliases wins.
const KEY_ALIASES: [&str; 4] = ["key", "Key", "name", "Name"];

/// The VALUE aliases, in the `has()` chain order. Closed, and shorter than the key chain by design.
const VALUE_ALIASES: [&str; 2] = ["value", "Value"];

/// Evaluates `to_entries` over an owned input.
///
/// An object publishes one `{"key": <name>, "value": <v>}` per entry in INSERTION order; an array publishes one per
/// index with a NUMBER key. Anything else has no keys, which is the error `keys_unsorted` would raise, because that is
/// literally the reference's first step.
///
/// # Errors
///
/// Returns the reference's no-keys error for a keyless input, or an allocation failure.
pub fn to_entries(input: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    match input.untagged() {
        Value::Object(object) => {
            let mut out =
                jqf_data::Array::try_with_capacity(object.len()).map_err(|_| EngineRunError::allocation_failure())?;
            for entry in object {
                let key = entry
                    .try_to_value_string()
                    .map_err(|_| EngineRunError::allocation_failure())?;
                let value = entry.value().clone();
                out.try_push(pair(key, value, resources)?)
                    .map_err(|_| EngineRunError::allocation_failure())?;
            }
            Ok(Value::Array(out))
        }
        Value::Array(array) => {
            let mut out =
                jqf_data::Array::try_with_capacity(array.len()).map_err(|_| EngineRunError::allocation_failure())?;
            for (position, element) in array.iter().enumerate() {
                let index =
                    i64::try_from(position).map_err(|_| EngineRunError::internal_contract("an index beyond i64"))?;
                let key = Value::Number(Number::integer(Integer::from_i64(index)));
                let value = element.clone();
                out.try_push(pair(key, value, resources)?)
                    .map_err(|_| EngineRunError::allocation_failure())?;
            }
            Ok(Value::Array(out))
        }
        other => {
            let operand = message::dump_trunc_owned(other)?;
            let text = message::no_keys_message(other.kind(), &operand)?;
            Err(raise(&text, resources))
        }
    }
}

/// Evaluates `from_entries` over an owned input.
///
/// Every child of the input (an array's elements, an OBJECT's values) is read through the two chains above; duplicate
/// keys keep FIRST-occurrence position and LAST value, which is exactly [`ObjectBuilder`]'s own finish law.
///
/// # Errors
///
/// Returns the iterate error for a non-iterable input, the index error for a child that cannot be indexed by a string,
/// the object-key error for a non-string key, or an allocation failure.
pub fn from_entries(input: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let children: Vec<&Value> = match input.untagged() {
        Value::Array(array) => array.iter().collect(),
        Value::Object(object) => object.iter().map(jqf_data::ObjectEntry::value).collect(),
        other => {
            let operand = message::dump_trunc_owned(other)?;
            let text = message::iterate_message(other.kind(), &operand)?;
            return Err(raise(&text, resources));
        }
    };
    let mut builder =
        ObjectBuilder::try_with_capacity(children.len()).map_err(|_| EngineRunError::allocation_failure())?;
    for child in children {
        let key = entry_key(child, resources)?;
        let Value::String(name) = key.untagged() else {
            let operand = message::dump_trunc_owned(&key)?;
            let text = message::object_key_message(key.kind(), &operand)?;
            return Err(raise(&text, resources));
        };
        let name = ObjectKey::try_from_str(name.as_str()).map_err(|_| EngineRunError::allocation_failure())?;
        builder
            .try_insert_last(name, entry_value(child)?)
            .map_err(|_| EngineRunError::allocation_failure())?;
    }
    builder
        .try_finish()
        .map(Value::Object)
        .map_err(|_| EngineRunError::allocation_failure())
}

/// The key one entry contributes, through the TRUTHINESS chain.
///
/// A falsy alias falls through, so an exhausted chain answers with the LAST alias's value — `null` when it is absent
/// — and never with the falsy value that was present earlier. That is what makes `[{"key":false,"value":1}]` report
/// `null` as the offending key.
fn entry_key(child: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let mut last = Value::Null;
    for alias in KEY_ALIASES {
        last = field(child, alias, resources)?;
        if !matches!(last.untagged(), Value::Null | Value::Bool(false)) {
            return Ok(last);
        }
    }
    Ok(last)
}

/// The value one entry contributes, through the PRESENCE chain.
///
/// Presence, not truthiness: a present `false` or `null` under `value` stops the chain, so a `Value` alias beside a
/// `null` `value` is never consulted.
fn entry_value(child: &Value) -> Result<Value, EngineRunError> {
    let Value::Object(object) = child.untagged() else {
        // The key chain has already rejected every non-object child.
        return Err(EngineRunError::internal_contract(
            "an entry value read from a non-object child",
        ));
    };
    for alias in VALUE_ALIASES {
        if let Some(value) = object.get(alias) {
            return Ok(value.clone());
        }
    }
    Ok(Value::Null)
}

/// `child.<name>` with the indexing law: an object reads (absent is `null`), a `null` reads `null`, and every other
/// kind is an index error.
fn field(child: &Value, name: &str, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    match child.untagged() {
        Value::Object(object) => match object.get(name) {
            Some(value) => Ok(value.clone()),
            None => Ok(Value::Null),
        },
        Value::Null => Ok(Value::Null),
        other => {
            let accessor = message::render_object_key(name)?;
            let text = message::index_message(other.kind(), &accessor)?;
            Err(raise(&text, resources))
        }
    }
}

/// One `{"key": k, "value": v}` entry object.
fn pair(key: Value, value: Value, _resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let mut builder = ObjectBuilder::try_with_capacity(2).map_err(|_| EngineRunError::allocation_failure())?;
    builder
        .try_insert_last(
            ObjectKey::try_from_str("key").map_err(|_| EngineRunError::allocation_failure())?,
            key,
        )
        .map_err(|_| EngineRunError::allocation_failure())?;
    builder
        .try_insert_last(
            ObjectKey::try_from_str("value").map_err(|_| EngineRunError::allocation_failure())?,
            value,
        )
        .map_err(|_| EngineRunError::allocation_failure())?;
    builder
        .try_finish()
        .map(Value::Object)
        .map_err(|_| EngineRunError::allocation_failure())
}

const KEYS_UNSORTED_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::KEYS_UNSORTED),
    canonical_name: "keys_unsorted",
    category: "entries",
    summary: "An object's keys in INSERTION order, or an array's index list.",
    detail: "The only difference from `keys` is the object case: the keys come \
             back in the order the object carries them rather than sorted. An \
             array answers `[0, n)` either way, and a keyless value raises the \
             same `has no keys` error.",
};

const KEYS_UNSORTED_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::KEYS_UNSORTED),
    family: BuiltinFamilyId::new(id::KEYS_UNSORTED),
    canonical_name: "keys_unsorted",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "keys_unsorted",
            input: "{\"b\":1,\"a\":2}",
            expected: "[\"b\",\"a\"]\n",
        },
        BuiltinExample {
            program: "keys_unsorted",
            input: "[1,2]",
            expected: "[0,1]\n",
        },
    ],
};

const TO_ENTRIES_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::TO_ENTRIES),
    canonical_name: "to_entries",
    category: "entries",
    summary: "An object as an array of `{\"key\":k,\"value\":v}` objects.",
    detail: "Entries come back in the object's own insertion order, not sorted. \
             An ARRAY input answers with NUMBER keys, which is why \
             `[1,2] | to_entries | from_entries` is an object-key error rather \
             than a round trip.",
};

const TO_ENTRIES_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::TO_ENTRIES),
    family: BuiltinFamilyId::new(id::TO_ENTRIES),
    canonical_name: "to_entries",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "to_entries",
            input: "{\"b\":1,\"a\":2}",
            expected: "[{\"key\":\"b\",\"value\":1},{\"key\":\"a\",\"value\":2}]\n",
        },
        BuiltinExample {
            program: "to_entries",
            input: "[1,2]",
            expected: "[{\"key\":0,\"value\":1},{\"key\":1,\"value\":2}]\n",
        },
    ],
};

const FROM_ENTRIES_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::FROM_ENTRIES),
    canonical_name: "from_entries",
    category: "entries",
    summary: "An array of key/value entries as one object.",
    detail: "The key is read through the truthiness chain \
             `.key // .Key // .name // .Name` and the value through the \
             presence chain `value`, `Value`, so a falsy KEY falls through to \
             the next alias while a falsy VALUE does not. Duplicate keys keep \
             first-occurrence position and last value.",
};

const FROM_ENTRIES_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::FROM_ENTRIES),
    family: BuiltinFamilyId::new(id::FROM_ENTRIES),
    canonical_name: "from_entries",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "from_entries",
            input: "[{\"key\":\"a\",\"value\":1},{\"name\":\"b\",\"Value\":2}]",
            expected: "{\"a\":1,\"b\":2}\n",
        },
        BuiltinExample {
            program: "from_entries",
            input: "[{\"key\":\"a\",\"value\":1},{\"key\":\"a\",\"value\":2}]",
            expected: "{\"a\":2}\n",
        },
    ],
};

const WITH_ENTRIES_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::WITH_ENTRIES),
    canonical_name: "with_entries",
    category: "entries",
    summary: "An object rebuilt after running a filter over each of its entries.",
    detail: "Exactly `to_entries | map(f) | from_entries`. A `select` that \
             drops an entry drops the key; a filter that renames `.key` renames \
             the key; and because `map` collects ALL of `f`'s outputs, a \
             multi-output filter contributes several entries to ONE object \
             rather than producing several objects.",
};

const WITH_ENTRIES_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::WITH_ENTRIES),
    family: BuiltinFamilyId::new(id::WITH_ENTRIES),
    canonical_name: "with_entries",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Lowering,
    demand_transfer: DemandTransfer::ViaLowering,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "with_entries(.value += 1)",
            input: "{\"a\":1}",
            expected: "{\"a\":2}\n",
        },
        BuiltinExample {
            program: "with_entries(select(.value > 1))",
            input: "{\"a\":1,\"b\":2}",
            expected: "{\"b\":2}\n",
        },
        // A MULTI-VALUED entry filter rebuilds the object once per output, so the LAST output wins — the mirror of
        // `map_values`, where the per-key update keeps the FIRST.
        BuiltinExample {
            program: "with_entries(.value += 10, .value += 100)",
            input: "{\"a\":1,\"b\":2}",
            expected: "{\"a\":101,\"b\":102}\n",
        },
    ],
};
