//! Semantic-checksum witness shared by the jqf and `ciborium` decode oracles.
//!
//! The scheme is `cbor-semantic-fnv1a64-v1`. Two comparison decisions are made
//! here, explicitly:
//!
//! - **Map order is ignored** (both sides hash sorted by key text). jqf
//!   preserves author order; `ciborium`'s `Map` is an ordered `Vec`, and RFC
//!   8949 maps are unordered by value, so a shared checksum must sort.
//! - **Number kinds stay apart.** An integer and a float are DIFFERENT CBOR
//!   major types (0/1 vs 7), so `Integer(1)` and `Float(1.0)` must not meet:
//!   integers hash through their exact decimal spelling, floats through their
//!   exact IEEE-754 bits (which also preserves `-0.0` and non-finite values).
//! - **Retained tags meet.** jqf's `Value::Tagged { tag: "cbor:tag:N" }` and
//!   `ciborium`'s `Tag(tag, value)` are the same fact. Recognized tags 0–5
//!   are the opposite: jqf projects them to native semantics (datetimes,
//!   bignums, decimals) while `ciborium` retains every tag — those cases are
//!   the declared-split rows. Simple values are a recorded incumbent gap:
//!   jqf preserves `undefined` as `cbor:simple:23` and every legal simple
//!   value as `cbor:simple:N`, while `ciborium` folds `undefined` into `null`
//!   (the remaining simple-value declared row). Two-byte simples below 32
//!   are reserved and both sides reject.

use jqf_data::{Decimal, Number, NumberCategory, Value};

const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const PRIME: u64 = 0x0000_0100_0000_01b3;

/// Identifies the exact checksum scheme so reports are self-describing.
pub(crate) const SCHEMA: &str = "cbor-semantic-fnv1a64-v1";

/// Hashes a `ciborium::value::Value`, the comparison oracle's decoded shape.
pub(crate) fn ciborium_value(root: &ciborium::value::Value) -> u64 {
    enum Task<'a> {
        Visit(&'a ciborium::value::Value),
        Key(String),
        End(u8),
    }

    let mut hash = OFFSET;
    let mut tasks = vec![Task::Visit(root)];
    while let Some(task) = tasks.pop() {
        match task {
            Task::Visit(value) => match value {
                ciborium::value::Value::Null => hash = byte(hash, 0),
                ciborium::value::Value::Bool(value) => {
                    hash = byte(byte(hash, 1), u8::from(*value));
                }
                ciborium::value::Value::Integer(value) => {
                    // `Integer` is an i128-backed newtype with no Display;
                    // its i128 projection is the exact value.
                    hash = number_spelling(hash, &i128::from(*value).to_string());
                }
                ciborium::value::Value::Float(value) => hash = float(hash, *value),
                ciborium::value::Value::Text(value) => hash = text(byte(hash, 3), value),
                ciborium::value::Value::Bytes(value) => {
                    hash = bytes_hash(byte(hash, 4), value);
                }
                ciborium::value::Value::Tag(tag, value) => {
                    hash = text(byte(hash, 6), &format!("cbor:tag:{tag}"));
                    tasks.push(Task::Visit(value));
                }
                ciborium::value::Value::Array(values) => {
                    hash = usize_hash(byte(hash, 7), values.len());
                    tasks.push(Task::End(7));
                    tasks.extend(values.iter().rev().map(Task::Visit));
                }
                ciborium::value::Value::Map(values) => {
                    hash = usize_hash(byte(hash, 8), values.len());
                    tasks.push(Task::End(8));
                    let mut entries: Vec<_> = values.iter().collect();
                    entries.sort_unstable_by(|left, right| {
                        let left_key = key_text(&left.0);
                        let right_key = key_text(&right.0);
                        left_key.cmp(&right_key)
                    });
                    for (key, value) in entries.into_iter().rev() {
                        tasks.push(Task::Visit(value));
                        tasks.push(Task::Key(key_text(key)));
                    }
                }
                // `Value` is `#[non_exhaustive]`; a future variant is a
                // comparison gap and must fail closed, not silently skip.
                _ => unreachable!("ciborium Value gained a variant the CBOR differential does not know"),
            },
            Task::Key(key) => hash = text(byte(hash, 9), &key),
            Task::End(marker) => hash = byte(hash, marker | 0x80),
        }
    }
    hash
}

/// Renders one map key for sorting and hashing. Text keys hash by their own
/// text; every other key kind (which only appears in the declared-split rows
/// that jqf rejects, so never reaches a checksum comparison) renders through
/// its Debug spelling — deterministic, and never confused with a text key.
fn key_text(key: &ciborium::value::Value) -> String {
    // Text keys hash by their own text, matching jqf's object keys. A
    // non-text key (which only appears in declared-split rows that jqf
    // rejects, so never reaches a checksum comparison) renders through its
    // Debug spelling, deterministically.
    match key {
        ciborium::value::Value::Text(text) => text.clone(),
        other => format!("{other:?}"),
    }
}

/// Hashes a jqf `Value`, the value under test's decoded shape.
pub(crate) fn jqf_value(root: &Value) -> u64 {
    enum Task<'a> {
        Visit(&'a Value),
        Key(&'a str),
        End(u8),
    }

    let mut hash = OFFSET;
    let mut tasks = vec![Task::Visit(root)];
    while let Some(task) = tasks.pop() {
        match task {
            // NOTE: deliberately NOT `value.untagged()`: a CBOR tag layer is a
            // real semantic fact (the retained `cbor:tag:N` identity) and must
            // be hashed, not stripped.
            Task::Visit(value) => match value {
                Value::Null => hash = byte(hash, 0),
                Value::Bool(value) => hash = byte(byte(hash, 1), u8::from(*value)),
                Value::Number(value) => hash = number(hash, value),
                Value::String(value) => hash = text(byte(hash, 3), value),
                Value::Bytes(value) => {
                    hash = bytes_hash(byte(hash, 4), value);
                }
                Value::OffsetDateTime(value) => {
                    let mut out = String::new();
                    value.write_text(&mut out).expect("canonical offset datetime");
                    hash = text(byte(hash, 5), &out);
                }
                Value::Tagged { tag, payload } => {
                    hash = text(byte(hash, 6), tag.as_str());
                    tasks.push(Task::Visit(payload));
                }
                Value::Array(values) => {
                    hash = usize_hash(byte(hash, 7), values.len());
                    tasks.push(Task::End(7));
                    tasks.extend(values.iter().rev().map(Task::Visit));
                }
                Value::Object(values) => {
                    hash = usize_hash(byte(hash, 8), values.len());
                    tasks.push(Task::End(8));
                    let mut entries: Vec<_> = values.iter().collect();
                    entries.sort_unstable_by(|left, right| left.key().cmp(right.key()));
                    for entry in entries.into_iter().rev() {
                        tasks.push(Task::Visit(entry.value()));
                        tasks.push(Task::Key(entry.key()));
                    }
                }
                Value::LocalDate(_) | Value::LocalTime(_) | Value::LocalDateTime(_) => {
                    unreachable!("CBOR produces no local temporal kinds")
                }
            },
            Task::Key(key) => hash = text(byte(hash, 9), key),
            Task::End(marker) => hash = byte(hash, marker | 0x80),
        }
    }
    hash
}

fn number(mut hash: u64, value: &Number) -> u64 {
    match value.category() {
        NumberCategory::Integer => {
            // The inline machine arm renders its canonical spelling on demand
            // (093 S1); the boxed arm borrows its retained one.
            match value.as_machine() {
                Some(machine) => {
                    let integer = jqf_data::Integer::from_i64(machine);
                    number_spelling(hash, integer.as_str())
                }
                None => number_spelling(hash, value.as_integer().expect("integer category").as_str()),
            }
        }
        NumberCategory::Decimal => {
            let decimal = value.as_decimal().expect("decimal category");
            hash = text(hash, decimal.coefficient().as_str());
            bytes(hash, &decimal.scale().to_le_bytes())
        }
        NumberCategory::Float => float(hash, value.as_float().expect("float category").get()),
    }
}

fn number_spelling(mut hash: u64, spelling: &str) -> u64 {
    let decimal = Decimal::parse(spelling).expect("CBOR integer has an exact decimal form");
    hash = text(hash, decimal.coefficient().as_str());
    bytes(hash, &decimal.scale().to_le_bytes())
}

/// Hashes a float by exact IEEE-754 bits, keeping it apart from the integer
/// hash (CBOR major type 7 is a distinct semantic kind) and preserving
/// `-0.0`, infinity and NaN.
fn float(mut hash: u64, value: f64) -> u64 {
    hash = byte(hash, 0x5a);
    bytes(hash, &value.to_bits().to_le_bytes())
}

fn bytes_hash(mut hash: u64, value: &[u8]) -> u64 {
    hash = usize_hash(hash, value.len());
    bytes(hash, value)
}

fn byte(mut hash: u64, value: u8) -> u64 {
    hash ^= u64::from(value);
    hash.wrapping_mul(PRIME)
}

fn bytes(mut hash: u64, value: &[u8]) -> u64 {
    for &value in value {
        hash = byte(hash, value);
    }
    hash
}

fn usize_hash(hash: u64, value: usize) -> u64 {
    bytes(hash, &value.to_le_bytes())
}

fn text(hash: u64, value: &str) -> u64 {
    bytes(usize_hash(hash, value.len()), value.as_bytes())
}
