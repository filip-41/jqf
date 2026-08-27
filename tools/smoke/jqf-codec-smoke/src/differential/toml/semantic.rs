//! Semantic-checksum witness shared by the jqf and `toml` decode oracles.
//!
//! The scheme is `toml-semantic-fnv1a64-v1`: object storage order is ignored
//! (the `toml` crate's `Table` is a `BTreeMap`; jqf preserves author order, so
//! a shared checksum must sort), and numeric spellings are normalized through
//! an exact `Decimal` reduced to its fully-canonical `(coefficient, scale)`
//! form (trailing zeros stripped from the coefficient, scale adjusted), so
//! `1e5`, `100000` and `100000.0` all meet — the exponent spelling is a
//! spelling, not a value. The one deliberate gap is binary64 precision: a
//! decimal with more significant digits than f64 can carry is rounded by the
//! `toml` crate and kept exact by jqf, which is the
//! `declared/exact-decimal-split` row. Date-times are reduced to their
//! canonical text form on BOTH sides — the one spelling both implementations
//! agree on for every legal TOML temporal.

use jqf_data::{Decimal, Number, NumberCategory, Value};

const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const PRIME: u64 = 0x0000_0100_0000_01b3;

/// Identifies the exact checksum scheme so reports are self-describing.
pub(crate) const SCHEMA: &str = "toml-semantic-fnv1a64-v2";

/// Hashes a `toml::Value`, the comparison oracle's decoded shape.
pub(crate) fn toml_value(root: &toml::Value) -> u64 {
    enum Task<'a> {
        Visit(&'a toml::Value),
        Key(&'a str),
        End(u8),
    }

    let mut hash = OFFSET;
    let mut tasks = vec![Task::Visit(root)];
    while let Some(task) = tasks.pop() {
        match task {
            Task::Visit(value) => match value {
                toml::Value::String(value) => hash = text(byte(hash, 0), value),
                toml::Value::Integer(value) => {
                    hash = number_spelling(byte(hash, 1), &value.to_string());
                }
                toml::Value::Float(value) => {
                    hash = float(byte(hash, 2), *value);
                }
                toml::Value::Boolean(value) => hash = byte(byte(hash, 3), u8::from(*value)),
                toml::Value::Datetime(value) => {
                    hash = text(byte(hash, 4), &toml_datetime_text(value));
                }
                toml::Value::Array(values) => {
                    hash = usize_hash(byte(hash, 5), values.len());
                    tasks.push(Task::End(5));
                    tasks.extend(values.iter().rev().map(Task::Visit));
                }
                toml::Value::Table(values) => {
                    hash = usize_hash(byte(hash, 6), values.len());
                    tasks.push(Task::End(6));
                    for (key, value) in values.iter().rev() {
                        tasks.push(Task::Visit(value));
                        tasks.push(Task::Key(key));
                    }
                }
            },
            Task::Key(key) => hash = text(byte(hash, 7), key),
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
                Value::String(value) => hash = text(byte(hash, 0), value),
                Value::Number(value) => hash = number(hash, value),
                Value::Bool(value) => hash = byte(byte(hash, 3), u8::from(*value)),
                Value::LocalDate(value) => {
                    hash = text(byte(hash, 4), &local_date_text(*value));
                }
                Value::LocalTime(value) => {
                    hash = text(byte(hash, 4), &local_time_text(value));
                }
                Value::LocalDateTime(value) => {
                    hash = text(byte(hash, 4), &local_datetime_text(value));
                }
                Value::OffsetDateTime(value) => {
                    hash = text(byte(hash, 4), &offset_datetime_text(value));
                }
                Value::Array(values) => {
                    hash = usize_hash(byte(hash, 5), values.len());
                    tasks.push(Task::End(5));
                    tasks.extend(values.iter().rev().map(Task::Visit));
                }
                Value::Object(values) => {
                    hash = usize_hash(byte(hash, 6), values.len());
                    tasks.push(Task::End(6));
                    let mut entries: Vec<_> = values.iter().collect();
                    entries.sort_unstable_by(|left, right| left.key().cmp(right.key()));
                    for entry in entries.into_iter().rev() {
                        tasks.push(Task::Visit(entry.value()));
                        tasks.push(Task::Key(entry.key()));
                    }
                }
                Value::Null | Value::Bytes(_) | Value::Tagged { .. } => {
                    unreachable!("TOML cannot produce non-TOML semantic values")
                }
            },
            Task::Key(key) => hash = text(byte(hash, 7), key),
            Task::End(marker) => hash = byte(hash, marker | 0x80),
        }
    }
    hash
}

fn number(hash: u64, value: &Number) -> u64 {
    match value.category() {
        NumberCategory::Integer => match value.as_machine() {
            Some(machine) => {
                let integer = jqf_data::Integer::from_i64(machine);
                number_spelling(byte(hash, 1), integer.as_str())
            }
            None => number_spelling(byte(hash, 1), value.as_integer().expect("integer category").as_str()),
        },
        NumberCategory::Decimal => {
            let decimal = value.as_decimal().expect("decimal category");
            hash_decimal(byte(hash, 2), decimal.coefficient().as_str(), decimal.scale())
        }
        NumberCategory::Float => float(byte(hash, 2), value.as_float().expect("float category").get()),
    }
}

fn number_spelling(hash: u64, spelling: &str) -> u64 {
    let decimal = Decimal::parse(spelling).expect("TOML number has an exact decimal form");
    hash_decimal(hash, decimal.coefficient().as_str(), decimal.scale())
}

/// Hashes a decimal in its FULLY canonical form: trailing zeros stripped from
/// the coefficient and the scale adjusted, so `1e5`, `100000` and `100000.0`
/// meet (they are one value).
fn hash_decimal(mut hash: u64, coefficient: &str, scale: i64) -> u64 {
    let (coefficient, scale) = reduce(coefficient, scale);
    hash = text(hash, &coefficient);
    bytes(hash, &scale.to_le_bytes())
}

/// Strips trailing zeros from a coefficient, adjusting the scale: `("100000",
/// 0)` -> `("1", -5)`. Zero stays `("0", 0)`.
fn reduce(coefficient: &str, scale: i64) -> (String, i64) {
    let negative = coefficient.starts_with('-');
    let unsigned = coefficient.strip_prefix('-').unwrap_or(coefficient);
    let digits = unsigned.trim_end_matches('0');
    if digits.is_empty() {
        ("0".to_owned(), 0)
    } else {
        let stripped = unsigned.len() - digits.len();
        let mut out = String::with_capacity(digits.len() + usize::from(negative));
        if negative {
            out.push('-');
        }
        out.push_str(digits);
        (out, scale - i64::try_from(stripped).expect("digit count fits i64"))
    }
}

/// Hashes a float: a finite value through its exact decimal spelling (so it
/// meets jqf's exact decimal of the same value), a non-finite value (the
/// legacy `inf`/`nan` spellings) by exact IEEE-754 bits.
fn float(mut hash: u64, value: f64) -> u64 {
    if value.is_finite() {
        number_spelling(hash, &value.to_string())
    } else {
        hash = byte(hash, 0x5a);
        bytes(hash, &value.to_bits().to_le_bytes())
    }
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

// --- canonical date-time text -------------------------------------------
//
// Both sides are reduced to the SAME spelling so the checksums can meet:
// jqf's own canonical `write_text` output on the one side, the `toml`
// crate's own `Display` on the other (date `YYYY-MM-DD`, `T`, time
// `HH:MM:SS[.fraction-trimmed]`, offset `Z` or `±HH:MM`).

fn local_date_text(date: jqf_data::LocalDate) -> String {
    let mut out = String::new();
    date.write_text(&mut out).expect("canonical date text");
    out
}

fn local_time_text(time: &jqf_data::LocalTime) -> String {
    let mut out = String::new();
    time.write_text(&mut out).expect("canonical time text");
    out
}

fn local_datetime_text(datetime: &jqf_data::LocalDateTime) -> String {
    let mut out = String::new();
    datetime.write_text(&mut out).expect("canonical datetime text");
    out
}

fn offset_datetime_text(datetime: &jqf_data::OffsetDateTime) -> String {
    let mut out = String::new();
    datetime.write_text(&mut out).expect("canonical offset text");
    out
}

/// Renders a `toml::Datetime` through the `toml` crate's own canonical
/// `Display` (date `YYYY-MM-DD`, `T`, time `HH:MM:SS[.fraction-trimmed]`,
/// offset `Z` or `±HH:MM`), which is the same spelling jqf's `write_text`
/// produces — so the two checksums can meet. The one deliberate gap is the
/// unknown-local-offset fact (`-00:00`), which the `toml` crate renders as
/// `+00:00`; that case is the `declared/negative-zero-offset-split` row.
fn toml_datetime_text(datetime: &toml::value::Datetime) -> String {
    datetime.to_string()
}
