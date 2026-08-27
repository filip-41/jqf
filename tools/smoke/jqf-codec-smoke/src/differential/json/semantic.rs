//! Semantic-checksum witness shared by the jqf and serde decode oracles.
//!
//! Object storage order is ignored; numeric spellings are normalized through
//! an exact `Decimal`.

use jqf_data::{Decimal, Number, NumberCategory, Value};

const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const PRIME: u64 = 0x0000_0100_0000_01b3;

/// Hashes a serde `Value`, the comparison oracle's decoded shape.
pub(crate) fn serde_value(root: &serde_json::Value) -> u64 {
    enum Task<'a> {
        Visit(&'a serde_json::Value),
        Key(&'a str),
        End(u8),
    }

    let mut hash = OFFSET;
    let mut tasks = vec![Task::Visit(root)];
    while let Some(task) = tasks.pop() {
        match task {
            Task::Visit(value) => match value {
                serde_json::Value::Null => hash = byte(hash, 0),
                serde_json::Value::Bool(value) => {
                    hash = byte(byte(hash, 1), u8::from(*value));
                }
                serde_json::Value::Number(value) => {
                    hash = number_spelling(byte(hash, 2), &value.to_string());
                }
                serde_json::Value::String(value) => hash = text(byte(hash, 3), value),
                serde_json::Value::Array(values) => {
                    hash = usize_hash(byte(hash, 4), values.len());
                    tasks.push(Task::End(4));
                    tasks.extend(values.iter().rev().map(Task::Visit));
                }
                serde_json::Value::Object(values) => {
                    hash = usize_hash(byte(hash, 5), values.len());
                    tasks.push(Task::End(5));
                    let mut entries: Vec<_> = values.iter().collect();
                    entries.sort_unstable_by(|left, right| left.0.cmp(right.0));
                    for (key, value) in entries.into_iter().rev() {
                        tasks.push(Task::Visit(value));
                        tasks.push(Task::Key(key));
                    }
                }
            },
            Task::Key(key) => hash = text(byte(hash, 6), key),
            Task::End(marker) => hash = byte(hash, marker | 0x80),
        }
    }
    hash
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
            Task::Visit(value) => match value.untagged() {
                Value::Null => hash = byte(hash, 0),
                Value::Bool(value) => hash = byte(byte(hash, 1), u8::from(*value)),
                Value::Number(value) => hash = number(byte(hash, 2), value),
                Value::String(value) => hash = text(byte(hash, 3), value),
                Value::Array(values) => {
                    hash = usize_hash(byte(hash, 4), values.len());
                    tasks.push(Task::End(4));
                    tasks.extend(values.iter().rev().map(Task::Visit));
                }
                Value::Object(values) => {
                    hash = usize_hash(byte(hash, 5), values.len());
                    tasks.push(Task::End(5));
                    let mut entries: Vec<_> = values.iter().collect();
                    entries.sort_unstable_by(|left, right| left.key().cmp(right.key()));
                    for entry in entries.into_iter().rev() {
                        tasks.push(Task::Visit(entry.value()));
                        tasks.push(Task::Key(entry.key()));
                    }
                }
                Value::Bytes(_)
                | Value::LocalDate(_)
                | Value::LocalTime(_)
                | Value::LocalDateTime(_)
                | Value::OffsetDateTime(_)
                | Value::Tagged { .. } => {
                    unreachable!("strict JSON cannot produce non-JSON semantic values")
                }
            },
            Task::Key(key) => hash = text(byte(hash, 6), key),
            Task::End(marker) => hash = byte(hash, marker | 0x80),
        }
    }
    hash
}

fn number(mut hash: u64, value: &Number) -> u64 {
    match value.category() {
        NumberCategory::Integer => {
            // The inline machine arm renders its canonical spelling on demand;
            // the boxed arm borrows its retained one.
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
        NumberCategory::Float => {
            let spelling = serde_json::Number::from_f64(value.as_float().expect("float category").get())
                .expect("jqf Float is finite")
                .to_string();
            number_spelling(hash, &spelling)
        }
    }
}

fn number_spelling(mut hash: u64, spelling: &str) -> u64 {
    let decimal = Decimal::parse(spelling).expect("strict JSON number has an exact decimal form");
    hash = text(hash, decimal.coefficient().as_str());
    bytes(hash, &decimal.scale().to_le_bytes())
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

/// Identifies the exact checksum scheme so reports are self-describing.
pub(crate) const SCHEMA: &str = "json-semantic-fnv1a64-v2";
