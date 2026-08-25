//! Stable semantic witnesses shared by jqf and comparison JSON lanes.

use jqf_data::{Decimal, Number, NumberCategory, Value};

const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const PRIME: u64 = 0x0000_0100_0000_01b3;

pub(crate) const SCHEMA: &str = "json-semantic-fnv1a64-v2";

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

#[cfg(test)]
mod tests {
    use jqf_data::{Decimal, Integer, Number, ObjectBuilder, Value};

    use super::{OFFSET, byte, jqf_value, number_spelling, serde_value};

    #[test]
    fn jqf_and_serde_witnesses_ignore_object_storage_order() {
        let mut object = ObjectBuilder::try_with_capacity(2).expect("builder");
        object
            .try_insert_last(
                jqf_data::ObjectKey::try_from_str("b").expect("fixture key"),
                Value::Bool(true),
            )
            .expect("b");
        object
            .try_insert_last(
                jqf_data::ObjectKey::try_from_str("a").expect("fixture key"),
                Value::Number(Number::decimal(Decimal::parse("10.25").expect("decimal"))),
            )
            .expect("a");
        let jqf = Value::Object(object.try_finish().expect("finish"));
        let serde = serde_json::json!({"a": 10.25, "b": true});
        assert_eq!(jqf_value(&jqf), serde_value(&serde));
    }

    #[test]
    fn canonical_integer_and_decimal_text_match_serde_json() {
        for (jqf, serde) in [
            (
                Value::Number(Number::integer(Integer::parse("2048").expect("integer"))),
                serde_json::json!(2048),
            ),
            (
                Value::Number(Number::decimal(Decimal::parse("0.0025").expect("decimal"))),
                serde_json::json!(0.0025),
            ),
        ] {
            assert_eq!(jqf_value(&jqf), serde_value(&serde));
        }
    }

    #[test]
    fn numeric_witness_normalizes_json_spellings_without_precision_loss() {
        for equivalent in [
            &["1", "1.0", "1e0"][..],
            &["100", "100.0", "1e2", "10e1"][..],
            &["0", "-0", "-0.0", "0e999"][..],
        ] {
            let mut hashes = equivalent.iter().map(|spelling| {
                let serde: serde_json::Value = serde_json::from_str(spelling).expect("JSON number");
                serde_value(&serde)
            });
            let first = hashes.next().expect("nonempty equivalence class");
            assert!(hashes.all(|hash| hash == first), "{equivalent:?}");
        }

        for spelling in [
            "184467440737095516161844674407370955161",
            "12345678901234567890.12345678901234567890",
            "-9.876543210123456789e-123",
        ] {
            let jqf = Value::Number(Number::decimal(Decimal::parse(spelling).expect("exact jqf decimal")));
            assert_eq!(
                jqf_value(&jqf),
                number_spelling(byte(OFFSET, 2), spelling),
                "{spelling}",
            );
            let rounded: serde_json::Value = serde_json::from_str(spelling).expect("serde accepts finite JSON number");
            assert_ne!(
                jqf_value(&jqf),
                serde_value(&rounded),
                "serde's rounded value must not impersonate exact jqf semantics for {spelling}",
            );
        }
    }
}
