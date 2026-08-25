//! Semantic checksums for the YAML differential: one for jqf's decoded
//! `Value`, one for the corpus's `json:` oracle parsed with `serde_json`.
//!
//! Both walk their value in the same order (objects sorted by key) so the
//! checksums are directly comparable. The jqf walk is payload-transparent
//! (`untagged`) — the corpus's `json:` projections are the untagged core
//! projections, so a non-core YAML tag does not appear in the oracle and
//! must not appear in the checksum.

use jqf_data::{Number, Value};

const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

#[inline]
fn mix(mut hash: u64, byte: u8) -> u64 {
    hash ^= u64::from(byte);
    hash = hash.rotate_left(13);
    hash.wrapping_mul(5).wrapping_add(0xe984_6a4c_eb1e_5f5d)
}

#[inline]
fn bytes(hash: u64, bytes: &[u8]) -> u64 {
    let mut hash = hash;
    for &byte in bytes {
        hash = mix(hash, byte);
    }
    hash
}

#[inline]
fn text(hash: u64, text: &str) -> u64 {
    bytes(hash, text.as_bytes())
}

/// Hashes a jqf `Value`, the value under test's decoded shape. The walk is
/// payload-transparent: `Value::Tagged` contributes only its payload.
#[must_use]
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
                Value::Null => hash = mix(hash, 0),
                Value::Bool(value) => hash = mix(mix(hash, 1), u8::from(*value)),
                Value::Number(value) => hash = number(hash, value),
                Value::String(value) => hash = text(hash, value),
                Value::Array(values) => {
                    hash = usize_hash(mix(hash, 4), values.len());
                    tasks.push(Task::End(4));
                    tasks.extend(values.iter().rev().map(Task::Visit));
                }
                Value::Object(values) => {
                    hash = usize_hash(mix(hash, 5), values.len());
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
                    unreachable!("untagged() removes every tag layer before this match")
                }
            },
            Task::Key(key) => hash = text(mix(hash, 6), key),
            Task::End(marker) => hash = mix(hash, marker | 0x80),
        }
    }
    hash
}

/// Hashes a `serde_json::Value` — the oracle side of the differential.
#[must_use]
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
                serde_json::Value::Null => hash = mix(hash, 0),
                serde_json::Value::Bool(value) => hash = mix(mix(hash, 1), u8::from(*value)),
                serde_json::Value::Number(value) => hash = number_text(hash, &value.to_string()),
                serde_json::Value::String(value) => hash = text(hash, value),
                serde_json::Value::Array(values) => {
                    hash = usize_hash(mix(hash, 4), values.len());
                    tasks.push(Task::End(4));
                    tasks.extend(values.iter().rev().map(Task::Visit));
                }
                serde_json::Value::Object(values) => {
                    hash = usize_hash(mix(hash, 5), values.len());
                    tasks.push(Task::End(5));
                    let mut entries: Vec<_> = values.iter().collect();
                    entries.sort_unstable_by(|left, right| left.0.cmp(right.0));
                    for (key, value) in entries.into_iter().rev() {
                        tasks.push(Task::Visit(value));
                        tasks.push(Task::Key(key));
                    }
                }
            },
            Task::Key(key) => hash = text(mix(hash, 6), key),
            Task::End(marker) => hash = mix(hash, marker | 0x80),
        }
    }
    hash
}

/// Hashes a jqf `Number` by its canonical spelling.
fn number(hash: u64, value: &Number) -> u64 {
    match value.category() {
        jqf_data::NumberCategory::Integer => {
            // The inline machine arm renders its canonical spelling on demand
            // (093 S1); the boxed arm borrows its retained one.
            match value.as_machine() {
                Some(machine) => {
                    let integer = jqf_data::Integer::from_i64(machine);
                    number_text(hash, integer.as_str())
                }
                None => number_text(hash, value.as_integer().expect("integer category").as_str()),
            }
        }
        jqf_data::NumberCategory::Decimal => {
            // The D1 numbers slice decodes a YAML float spelling as an EXACT
            // decimal; the corpus's `json:` projections carry the same values
            // through serde_json's f64 model (`450.00` projects to `450`).
            // Hash by the value's shortest round-trip f64 spelling so `4.2`
            // and `4.20`, and `450.00` and `450`, all agree — the exact
            // coefficient/scale bytes would NOT (`DecimalText` renders the
            // canonical `4.5E+2` for 450.00), breaking the oracle.
            let decimal = value.as_decimal().expect("decimal category");
            let value = decimal_to_f64(decimal);
            let spelling = format!("{value}");
            number_text(hash, &spelling)
        }
        jqf_data::NumberCategory::Float => {
            let float = value.as_float().expect("float category");
            // The YAML core schema produces floats from decimal spellings;
            // the corpus's json projections carry the same decimals. Hash by
            // the shortest round-trip spelling so 1.0 and 1.00 agree.
            let spelling = format!("{}", float.get());
            number_text(hash, &spelling)
        }
    }
}

/// The binary64 value of an exact decimal (`coefficient * 10^-scale`), the
/// oracle-side view the corpus's json projections are read through.
fn decimal_to_f64(decimal: &jqf_data::Decimal) -> f64 {
    let mut text = String::from(decimal.coefficient().as_str());
    text.push('e');
    let exponent = -decimal.scale();
    text.push_str(&exponent.to_string());
    text.parse().unwrap_or(f64::NAN)
}

/// Hashes a number's canonical spelling.
fn number_text(mut hash: u64, spelling: &str) -> u64 {
    hash = mix(hash, 2);
    text(hash, spelling)
}

fn usize_hash(mut hash: u64, value: usize) -> u64 {
    hash = mix(hash, u8::try_from(value & 0xff).unwrap_or(0));
    hash = mix(hash, u8::try_from((value >> 8) & 0xff).unwrap_or(0));
    hash = mix(hash, u8::try_from((value >> 16) & 0xff).unwrap_or(0));
    hash = mix(hash, u8::try_from((value >> 24) & 0xff).unwrap_or(0));
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_walk_is_order_stable() {
        // The oracle-side walk must be order-stable: hashing the same
        // content twice gives the same checksum, and reordered keys hash
        // the same (keys are sorted before hashing).
        let text_a = "{\"b\":2,\"a\":1}";
        let text_b = "{\"a\":1,\"b\":2}";
        let a: serde_json::Value = serde_json::from_str(text_a).unwrap();
        let b: serde_json::Value = serde_json::from_str(text_b).unwrap();
        assert_eq!(serde_value(&a), serde_value(&b));
        assert_eq!(serde_value(&a), serde_value(&a));
    }

    #[test]
    fn decode_l24t_matches_libyaml_eof_clip() {
        // A block scalar whose last content line ends at EOF without a
        // newline: the corpus's spec reading gives `x\n \n`, but libyaml
        // (and the codec, which matches it, and yq/go-yaml) give `x\n ` —
        // no line break to preserve. The disagreement is allowlisted in the
        // differential; this test pins the codec's own contract.
        let yaml = b"foo: |\n  x\n   ";
        let values = crate::differential::yaml::decode(yaml).expect("decode");
        let value = values.first().expect("one document");
        let jqf_hash = jqf_value(value);
        let libyaml: serde_json::Value = serde_json::from_str("{\"foo\":\"x\\n \"}").unwrap();
        assert_eq!(jqf_hash, serde_value(&libyaml));
        let corpus: serde_json::Value = serde_json::from_str("{\"foo\":\"x\\n \\n\"}").unwrap();
        assert_ne!(jqf_hash, serde_value(&corpus));
    }

    #[test]
    fn decode_4zym_checksum_matches_oracle() {
        // Decode the 4ZYM input through the codec (the same path main.rs
        // uses) and compare its checksum with the corpus oracle's.
        let yaml = b"plain: text\n  lines\nquoted: \"text\n  \tlines\"\nblock: |\n  text\n   \tlines\n";
        let values = crate::differential::yaml::decode(yaml).expect("decode");
        let value = values.first().expect("one document");
        let oracle: serde_json::Value = serde_json::from_str(
            "{\"plain\":\"text lines\",\"quoted\":\"text lines\",\"block\":\"text\\n \\tlines\\n\"}",
        )
        .unwrap();
        assert_eq!(
            jqf_value(value),
            serde_value(&oracle),
            "decoded value must hash like the oracle"
        );
        // Print the actual string bytes for a failing case.
        if let jqf_data::Value::Object(object) = value.untagged() {
            let entry = object.get("block").expect("block key").untagged();
            if let jqf_data::Value::String(text) = entry {
                eprintln!("decoded block = {:?}", text.as_str());
            }
        }
    }

    #[test]
    fn tab_and_space_agree_across_walks() {
        let from_str: serde_json::Value = serde_json::from_str("{\"block\":\"text\\n \\tlines\\n\"}").unwrap();
        let mut map = serde_json::Map::new();
        map.insert("block".into(), serde_json::Value::String("text\n \tlines\n".into()));
        let direct = serde_json::Value::Object(map);
        assert_eq!(serde_value(&from_str), serde_value(&direct));
    }
}
