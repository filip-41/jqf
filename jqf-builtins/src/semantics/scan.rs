//! The NUMBER-LITERAL reader: the acceptance `tonumber` applies, which is looser than JSON's.
//!
//! One job: decide whether a piece of text is a number the reader accepts, and hand back the value it becomes. It is
//! [`super::render`]'s mirror image — that module exists so the engine can spell the JSON WRITER without reaching for
//! a codec, and this one exists for the same reason on the reading side. The engine's dependency list has no JSON
//! codec, deliberately: a codec owns its format's I/O, the engine owns the value semantics.
//!
//! Even if the dependency were allowed, the codec could not serve here. Its acceptance is STRICT and this reader's is
//! not:
//!
//! ```text
//! "00123" → 123
//! ".5"    → 0.5
//! "5."    → 5
//! "+5.43" → 5.43
//! ```
//!
//! and its failures are `json.invalid-literal`-style diagnostics carrying source spans, where this reader's caller
//! wants one sentence naming the whole input.
//!
//! It ALSO accepts the non-finite family (`nan`, `snan`, `inf`, `Infinity`, each case-insensitive and optionally
//! signed), because the scalar-tails math vertical made non-finite values constructible and ordered: a NaN spelling
//! answers a NaN NUMBER (which renders `null` but is `type == "number"` and `isnan` — and is the SMALLEST number in
//! the total order), and an infinity spelling with an infinity (which renders the clamped `±1.7976931348623157e+308`).
//! The ordering tripwire this module's narrowing used to protect was fixed by that same vertical ([`super::order`]'s
//! NaN law), so the refusal is gone; `nan123`, `inf1` and `infin` are still refused.
//!
//! Negative space: it owns no message and no kind check — the caller names the builtin that refused — and it owns
//! no number SPELLING, which is `jqf-data`'s [`Number::try_json_literal`]. It only decides what reaches that acceptor.

use jqf_data::{Float, Number, Value};

/// The outcome of reading one number token: the value, a literal-grammar refusal, or an allocation failure inside the
/// numeric storage — which the decode paths surface as the machine allocation class, not as a parse message
/// (`json_stream_next`'s "returns only the machine allocation class").
pub enum NumberParse {
    /// The token read a value.
    Value(Value),
    /// The token is not a number this grammar accepts.
    Refused,
    /// The value's canonical storage could not be allocated.
    Allocation,
}

/// Reads `text` as one number literal.
///
/// A refused spelling is an ANSWER here and not a fault: [`NumberParse::Refused` is the caller's cue to raise ITS
/// message — this module names no builtin — and the number payload's own allocation is charged inside `jqf-data`.
pub fn number(text: &str) -> NumberParse {
    if let Some(value) = non_finite(text) {
        return NumberParse::Value(value);
    }
    if !is_number_literal(text) {
        return NumberParse::Refused;
    }
    match Number::try_json_literal(text) {
        Ok(number) => NumberParse::Value(Value::Number(number)),
        Err(jqf_data::NumericError::Allocation) => NumberParse::Allocation,
        Err(_) => NumberParse::Refused,
    }
}

/// The non-finite number spellings, each case-insensitive and optionally signed: `nan`/`snan` answer a NaN number and
/// `inf`/`infinity` answer ±infinity. The values are the SAME ones `nan` and `infinite` construct — a NaN number
/// prints `null`, an infinity prints the clamped widest finite binary64 — so `tonumber`/`fromjson` and the literals
/// cannot drift apart.
fn non_finite(text: &str) -> Option<Value> {
    let (negative, unsigned) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    if unsigned.eq_ignore_ascii_case("nan") || unsigned.eq_ignore_ascii_case("snan") {
        return Some(Value::Number(Number::float(Float::new(f64::NAN))));
    }
    if unsigned.eq_ignore_ascii_case("inf") || unsigned.eq_ignore_ascii_case("infinity") {
        let value = if negative { f64::NEG_INFINITY } else { f64::INFINITY };
        return Some(Value::Number(Number::float(Float::new(value))));
    }
    None
}

/// Whether `text` is spelled the way the number reader requires.
///
/// The grammar is the finite subset: an optional sign, then digits with at most one decimal point and at least one
/// digit somewhere, then an optional exponent whose own digits are mandatory. That is looser than JSON in three ways
/// users rely on (a leading `+`, redundant leading zeroes, and a point with nothing on one side of it) and stricter in
/// none.
///
/// The check exists because [`Number::try_json_literal`] is the codec's acceptor too: loosening IT to serve `tonumber`
/// would loosen document decoding, so the looseness lives here and the acceptor stays where it is. In practice the two
/// agree on everything but the guard below; the guard is what makes that a property of this module rather than a
/// coincidence to be discovered later.
fn is_number_literal(text: &str) -> bool {
    let unsigned = text.strip_prefix(['-', '+']).unwrap_or(text);
    let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
        Some(index) => (&unsigned[..index], Some(&unsigned[index + 1..])),
        None => (unsigned, None),
    };
    if !is_mantissa(mantissa) {
        return false;
    }
    match exponent {
        None => true,
        Some(digits) => is_exponent(digits),
    }
}

/// Digits with at most one point and at least one digit: `1`, `00123`, `.5`, `5.`
/// — but not `""`, `"."` or `"1.2.3"`.
fn is_mantissa(mantissa: &str) -> bool {
    let mut digits = 0_usize;
    let mut points = 0_usize;
    for byte in mantissa.bytes() {
        match byte {
            b'0'..=b'9' => digits += 1,
            b'.' => points += 1,
            _ => return false,
        }
    }
    digits > 0 && points <= 1
}

/// An exponent: an optional sign and at least one digit.
fn is_exponent(exponent: &str) -> bool {
    let digits = exponent.strip_prefix(['-', '+']).unwrap_or(exponent);
    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_grammar_is_lenient_and_not_strict_json() {
        // Looser than JSON, exactly where the reader is.
        for accepted in [
            "1", "1.5", "00123", ".5", "5.", "+5.43", "-0", "1e1000", "1E2", "1e+5", "1E-5", "1.50", "0", "-1.", "+.5",
        ] {
            assert!(is_number_literal(accepted), "accepted {accepted:?}");
        }
        // Refused too.
        for refused in [
            "", " 4", "4 ", "0x10", "1_0", ".", "-", "+", "1e", "e1", "E1", "1.2.3", "--1", "nan123", "inf1", "infin",
            "1e1.5", "1e+", "1e-",
        ] {
            assert!(!is_number_literal(refused), "refused {refused:?}");
        }
        // The non-finite family IS accepted (the values are constructible), answering the same numbers the `nan` and
        // `infinite` literals construct.
        for accepted in ["nan", "NaN", "NAN", "snan", "SNaN", "-nan", "+nan"] {
            assert!(
                matches!(number(accepted), NumberParse::Value(value) if value.kind() == jqf_data::ValueKind::Number),
                "jq accepts {accepted:?} as a NaN number"
            );
        }
        for accepted in ["inf", "Infinity", "iNfInItY", "-inf", "+Infinity"] {
            assert!(
                matches!(number(accepted), NumberParse::Value(value) if value.kind() == jqf_data::ValueKind::Number),
                "jq accepts {accepted:?} as a number"
            );
        }
        assert!(matches!(number("nan123"), NumberParse::Refused));
        assert!(matches!(number("inf1"), NumberParse::Refused));
        assert!(matches!(number("infin"), NumberParse::Refused));
    }
}
