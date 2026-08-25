//! The shared scalar formatter and the render package's escaping laws.
//!
//! The scalar formatter is the `@1` closed law every renderer reads: `null`, lowercase booleans, minimal base-ten
//! integers, positional (non-exponent) decimals preserving the stored coefficient digits, finite binary64 through the
//! reference's shortest-round-trip placement (`jqf-data`'s `format_binary64`), the render spelling `inf`/`-inf`, and
//! `nan(0x` plus the 16 lowercase hex digits of the big-endian binary64 NaN bits. Bytes render as lowercase `0x` hex;
//! the four temporal kinds render their central ISO/RFC-3339 spellings including stored precision and offset.
//!
//! Strings are NOT this module's law beyond the two spellings the renderers need: a raw passthrough (plain text, table
//! cells before per-renderer escaping) and a strict-JSON-quoted spelling with the tree renderer's forced
//! C1/DEL/line-separator/bidi escapes. The GFM/HTML/grid/terminal renderers escape table cell text atom-by-atom in
//! `atom.rs`.

use alloc::string::String;
use core::fmt;

use jqf_codec_core::{CodecError, CodecFailureKind};
use jqf_data::{Number, Value, format_binary64};

use crate::error::contract;

/// How a string scalar is spelled by one renderer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StringStyle {
    /// Raw UTF-8, unquoted (plain text, table-cell source text).
    Raw,
    /// Strict-JSON-quoted with the tree renderer's forced control escapes.
    TreeQuoted,
    /// Terminal-safe raw text: backslash and every control becomes a visible escape, so no terminal control injection
    /// is possible.
    Terminal,
}

/// Appends one untagged core scalar's text to `out`.
///
/// `quote_strings` selects the string spelling. The value must be an untagged core scalar; the callers' preflight laws
/// guarantee that, so a tag layer here is an internal-contract violation rather than a silent omission.
pub(crate) fn write_scalar(out: &mut String, value: &Value, quote_strings: StringStyle) -> Result<(), CodecError> {
    match value {
        Value::Null => {
            push_str(out, "null");
            Ok(())
        }
        Value::Bool(true) => {
            push_str(out, "true");
            Ok(())
        }
        Value::Bool(false) => {
            push_str(out, "false");
            Ok(())
        }
        Value::Number(number) => write_number(out, number),
        Value::String(text) => match quote_strings {
            StringStyle::Raw => {
                push_str(out, text.as_str());
                Ok(())
            }
            StringStyle::TreeQuoted => {
                write_tree_quoted(out, text.as_str());
                Ok(())
            }
            StringStyle::Terminal => {
                write_terminal_string(out, text.as_str());
                Ok(())
            }
        },
        Value::Bytes(bytes) => {
            write_bytes(out, bytes.as_slice());
            Ok(())
        }
        Value::LocalDate(date) => date.write_text(out).map_err(temporal_codec_error),
        Value::LocalTime(time) => time.write_text(out).map_err(temporal_codec_error),
        Value::LocalDateTime(datetime) => datetime.write_text(out).map_err(temporal_codec_error),
        Value::OffsetDateTime(datetime) => datetime.write_text(out).map_err(temporal_codec_error),
        Value::Tagged { .. } => Err(contract("a tagged value reached the scalar formatter")),
        Value::Array(_) | Value::Object(_) => Err(contract("a container reached the scalar formatter")),
    }
}

/// Appends one owned number in the render scalar law.
fn write_number(out: &mut String, number: &Number) -> Result<(), CodecError> {
    // The inline machine arm renders its canonical spelling on demand; the boxed arm borrows its retained one.
    if let Some(machine) = number.as_machine() {
        let integer = jqf_data::Integer::from_i64(machine);
        push_str(out, integer.as_str());
        return Ok(());
    }
    if let Some(integer) = number.as_integer() {
        push_str(out, integer.as_str());
        return Ok(());
    }
    if let Some(decimal) = number.as_decimal() {
        return push_decimal_plain(out, decimal.coefficient().as_str(), decimal.scale());
    }
    match number.as_float() {
        Some(value) => write_float(out, value.get()),
        None => Err(contract("a number carrying no representation")),
    }
}

/// Appends an exact decimal positionally: `coefficient * 10^-scale` with no exponent. The stored coefficient digits are
/// preserved exactly, so `1500` at scale 2 renders `15.00` (stored precision is never silently dropped).
fn push_decimal_plain(out: &mut String, coefficient: &str, scale: i64) -> Result<(), CodecError> {
    let (negative, digits) = match coefficient.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, coefficient),
    };
    if digits.is_empty() {
        return Err(contract("a decimal with an empty coefficient"));
    }
    if negative {
        push_char(out, '-');
    }
    if scale <= 0 {
        push_str(out, digits);
        let zeros = usize::try_from(-scale).map_err(|_| contract("decimal scale overflow"))?;
        for _ in 0..zeros {
            push_char(out, '0');
        }
        return Ok(());
    }
    let point = usize::try_from(scale).map_err(|_| contract("decimal scale overflow"))?;
    if digits.len() > point {
        let split = digits.len() - point;
        push_str(out, digits.get(..split).unwrap_or_default());
        push_char(out, '.');
        push_str(out, digits.get(split..).unwrap_or_default());
    } else {
        push_str(out, "0.");
        for _ in 0..(point - digits.len()) {
            push_char(out, '0');
        }
        push_str(out, digits);
    }
    Ok(())
}

/// Appends a binary64 under the render non-finite law: `inf`/`-inf` and the NaN bits spelling are renderer-owned;
/// finite values ride the reference's shortest round-trip placement (which also spells `-0.0` as `-0`).
pub(crate) fn write_float(out: &mut String, value: f64) -> Result<(), CodecError> {
    if value.is_nan() {
        push_str(out, "nan(0x");
        push_fmt(out, format_args!("{:016x}", value.to_bits()));
        push_char(out, ')');
        return Ok(());
    }
    if value == f64::INFINITY {
        push_str(out, "inf");
        return Ok(());
    }
    if value == f64::NEG_INFINITY {
        push_str(out, "-inf");
        return Ok(());
    }
    let text = format_binary64(value).ok_or_else(|| contract("a binary64 with no dtoa form"))?;
    push_str(out, text.as_str());
    Ok(())
}

/// Appends lowercase `0x` plus lowercase hex bytes.
fn write_bytes(out: &mut String, bytes: &[u8]) {
    push_str(out, "0x");
    for byte in bytes {
        push_fmt(out, format_args!("{byte:02x}"));
    }
}

fn temporal_codec_error(_: jqf_data::TemporalError) -> CodecError {
    CodecError::new(CodecFailureKind::AllocationFailure)
}

/// Appends one strict-JSON-quoted string.
///
/// The base law is RFC 8259's required escapes plus `\b`/`\f`; DEL and every other scalar pass through raw unless
/// `tree_safe` forces them visible.
pub(crate) fn write_json_quoted(out: &mut String, text: &str, tree_safe: bool) {
    push_char(out, '"');
    for ch in text.chars() {
        let code = ch as u32;
        match ch {
            '"' => push_str(out, "\\\""),
            '\\' => push_str(out, "\\\\"),
            '\u{0008}' => push_str(out, "\\b"),
            '\u{000C}' => push_str(out, "\\f"),
            '\n' => push_str(out, "\\n"),
            '\r' => push_str(out, "\\r"),
            '\t' => push_str(out, "\\t"),
            _ if code < 0x20 => {
                push_str(out, "\\u00");
                push_fmt(out, format_args!("{code:02x}"));
            }
            other if tree_safe && is_tree_forced(other) => {
                push_str(out, "\\u");
                push_fmt(out, format_args!("{code:04x}"));
            }
            other => {
                let mut scratch = [0_u8; 4];
                push_str(out, other.encode_utf8(&mut scratch));
            }
        }
    }
    push_char(out, '"');
}

/// Appends one strict-JSON-quoted string under the TREE renderer's forced escaping law: strict JSON for C0 plus every
/// C1/DEL, U+2028/U+2029, and the named bidi controls as `\uXXXX`.
pub(crate) fn write_tree_quoted(out: &mut String, text: &str) {
    write_json_quoted(out, text, true);
}

/// Appends terminal-safe raw text: backslash and every control becomes a visible escape (`\\`, `\0`/`\t`/`\n`/`\r`,
/// `\xHH`, `\u{...}`), so no source scalar can inject a terminal control.
pub(crate) fn write_terminal_string(out: &mut String, text: &str) {
    for ch in text.chars() {
        let code = ch as u32;
        match ch {
            '\\' => push_str(out, "\\\\"),
            '\u{0000}' => push_str(out, "\\0"),
            '\t' => push_str(out, "\\t"),
            '\n' => push_str(out, "\\n"),
            '\r' => push_str(out, "\\r"),
            _ if code < 0x20 || (0x7F..=0x9F).contains(&code) => {
                push_str(out, "\\x");
                push_fmt(out, format_args!("{code:02x}"));
            }
            _ if matches!(code, 0x061C | 0x200E | 0x200F | 0x202A..=0x202E | 0x2066..=0x2069)
                || matches!(ch, '\u{2028}' | '\u{2029}') =>
            {
                push_str(out, "\\u{");
                push_fmt(out, format_args!("{code:04x}"));
                push_char(out, '}');
            }
            other => {
                let mut scratch = [0_u8; 4];
                push_str(out, other.encode_utf8(&mut scratch));
            }
        }
    }
}

/// The scalars the tree renderer forces visible beyond strict JSON's C0 set.
pub(crate) fn is_tree_forced(ch: char) -> bool {
    let code = ch as u32;
    matches!(code, 0x7F..=0x9F)
        || matches!(ch, '\u{2028}' | '\u{2029}')
        || matches!(code, 0x061C | 0x200E | 0x200F | 0x202A..=0x202E | 0x2066..=0x2069)
}

/// Appends one formatted integer. `String`'s `fmt::Write` impl appends in place, so no owned intermediate is built per
/// call (a byte-at-a-time hex loop otherwise pays one heap allocation per input byte).
fn push_fmt(out: &mut String, args: fmt::Arguments<'_>) {
    let _ = fmt::Write::write_fmt(out, args);
}

/// Appends `text` to `out`.
fn push_str(out: &mut String, text: &str) {
    out.push_str(text);
}

/// Appends one char to `out`.
fn push_char(out: &mut String, ch: char) {
    let mut scratch = [0_u8; 4];
    out.push_str(ch.encode_utf8(&mut scratch));
}
