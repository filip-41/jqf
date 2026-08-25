//! The engine's own compact-JSON writer for owned values.
//!
//! One job: render an owned [`Value`] as the exact bytes the compact-JSON writer produces — the text `tostring`,
//! `tojson` and `join` are defined against. Those builtins are defined against JSON's own text, not against whatever
//! codec the session happens to output through (`jq --output-format` does not change `tostring`), which is why they are
//! VALUE semantics and live here beside [`total_cmp`] rather than behind the JSON output codec. The escape law itself
//! is not re-derived here: it delegates to `jqf-codec-json`'s [`json_escape_byte`](jqf_codec_json::json_escape_byte).
//!
//! Two laws it owns and one it inherits:
//!
//! * **Escaping.** [`write_string_literal`] delegates its escape set to `jqf-codec-json`'s
//!   [`json_escape_byte`](jqf_codec_json::json_escape_byte) — the ONE machine-JSON escape law in the tree (quote,
//!   backslash, `\b` `\f` `\n` `\r` `\t`, every remaining C0 control as lowercase `\u00xx`, and DEL `U+007F`, which
//!   the set escapes though JSON does not require it; everything else, non-ASCII included, passes through as raw
//!   UTF-8).
//!   This module's writer adds only the surrounding quotes and the bounded `cap` for error operands — the escape law
//!   itself lives once. [`crate::error::message`] renders its bounded error operands through this same function, so an
//!   operand's spelling inside an error message and outside one can never drift apart.
//! * **Depth.** The printer stops at [`MAX_PRINT_DEPTH`]: a value nested deeper renders as the literal marker
//!   [`TOO_DEEP`] instead of its contents, INSIDE the output rather than as an error. The walk here is ITERATIVE for
//!   the same reason the marker exists — ten thousand levels is not a recursion depth to put on the machine stack. This
//!   is a VALUE law (`tostring`/`tojson`/`join` and error-message operands), and it keeps the marker for byte parity.
//!   The CLI PUBLICATION path is deliberately different: it refuses a value past the same ceiling with a clean typed
//!   error instead of emitting the non-JSON marker, so stdout can never contain `<skipped: too deep>`.
//! * **Numbers** are not this module's law: an exact decimal renders through
//!   `jqf-data`'s [`DecimalText`] and a computed binary64 through [`format_binary64`], which are the same two spellings
//!   the JSON encoder publishes. What this module does NOT do is preserve a number's AUTHORED literal — `1.0` renders
//!   as `1` — because an owned `Value::Number` has nowhere to keep it. The consumers are exactly
//!   `tostring`/`tojson`/`join`/error-message operands, and none of them may see a retained spelling the value cannot
//!   carry.
//! * **Temporals** are likewise not this module's law: the four kinds render
//!   through `jqf-data`'s own canonical RFC 3339 writers, quoted here as JSON strings. The compatibility model has no
//!   temporal kind, so there is no reference spelling to match — what there IS is the spelling TOML and CBOR already
//!   publish, and this module reads it rather than growing a fourth copy.
//! * **Bytes** render as unpadded base64url (RFC 8949 §6.5), the same
//!   projection the output codecs publish. The guard that writes that projection sits ABOVE the catch-all, so a byte
//!   string never becomes `null` or `""` on `tojson` / `tostring` / error-operand text.
//!
//! Negative space: it writes COMPACT form only — no indent, no key sorting, no colour, no located-document walk.
//! Pretty printing belongs to the CLI's codec, and a located value is materialized by its caller before it gets here.
//!
//! [`total_cmp`]: super::order::total_cmp

use alloc::string::String;
use alloc::vec::Vec;

use jqf_codec_json::json_escape_byte;
use jqf_data::{Array, DecimalText, Number, Object, ScalarView, Value, format_binary64};

use crate::error::EngineRunError;

/// The deepest value the printer renders: a value at a GREATER depth is replaced by [`TOO_DEEP`]. The root sits at
/// depth zero, so a scalar wrapped in 10,001 arrays is the first thing to disappear.
///
/// It is a PRINTER law and not a comparison or a parser law: the parser accepts exactly 10,000 levels of nesting and
/// the comparison cap is a separate constant with a separate message. The depth-cap module (`semantics::depth`) keeps
/// its own ceiling; this constant is the printer's.
pub const MAX_PRINT_DEPTH: usize = 10_000;

/// The literal text written in place of a value nested past [`MAX_PRINT_DEPTH`]. It is not valid JSON and is emitted
/// anyway.
pub const TOO_DEEP: &str = "<skipped: too deep>";

/// Renders one owned value as compact JSON.
///
/// # Errors
///
/// Returns [`EngineRunError::Codec`] (allocation failure) when reserving the output buffer fails, or an
/// internal-contract error for a number that carries none of the three representations `jqf-data` defines.
pub fn to_json(value: &Value) -> Result<String, EngineRunError> {
    let mut out = String::new();
    out.try_reserve(json_seed_bytes(value))
        .map_err(|_| EngineRunError::allocation_failure())?;
    write_value(&mut out, value)?;
    Ok(out)
}

/// A conservative first reservation so a small value does not realloc three to five times. This path is
/// tostring/tojson/interpolation, not the codec encoder — do not over-reserve.
fn json_seed_bytes(value: &Value) -> usize {
    match value.untagged() {
        Value::Null | Value::Bool(true) => 4,
        Value::Bool(false) => 5,
        Value::Number(_) => 8,
        Value::String(text) => text.len().saturating_add(2),
        Value::Array(array) => 2usize.saturating_add(array.len().saturating_mul(2)),
        Value::Object(object) => 2usize.saturating_add(object.len().saturating_mul(8)),
        Value::Bytes(bytes) => bytes.len().saturating_add(2),
        _ => 32,
    }
}

/// Appends one owned value's compact JSON to `buf`.
///
/// The walk is iterative over an explicit cursor stack: `[`/`{` are written when a container is entered, the separators
/// as its children are visited, and the closing delimiter when its cursor runs out. Recursion would put the
/// 10,000-level print depth on the machine stack.
///
/// # Errors
///
/// As [`to_json`].
pub fn write_value(buf: &mut String, root: &Value) -> Result<(), EngineRunError> {
    let mut stack: Vec<Cursor<'_>> = Vec::new();
    let mut next = Some(root);
    loop {
        if let Some(value) = next.take() {
            if stack.len() > MAX_PRINT_DEPTH {
                push(buf, TOO_DEEP)?;
            } else if let Some(cursor) = write_scalar_or_open(buf, value)? {
                stack.try_reserve(1).map_err(|_| EngineRunError::allocation_failure())?;
                stack.push(cursor);
                continue;
            }
        }
        match step(buf, &mut stack)? {
            Step::Child(child) => next = Some(child),
            Step::Closed => {}
            Step::Done => return Ok(()),
        }
    }
}

/// One container being walked, and how far the walk has got through it.
enum Cursor<'value> {
    Array { array: &'value Array, index: usize },
    Object { object: &'value Object, index: usize },
}

/// What one advance of the innermost cursor produced.
enum Step<'value> {
    /// The next child to write.
    Child(&'value Value),
    /// The innermost container finished and was closed; try its parent.
    Closed,
    /// The stack is empty — the whole value is written.
    Done,
}

/// Writes a scalar outright, or opens a container and reports the cursor to push.
fn write_scalar_or_open<'value>(
    buf: &mut String,
    value: &'value Value,
) -> Result<Option<Cursor<'value>>, EngineRunError> {
    // Extended kinds (bytes + the four temporals) have a declared JSON spelling — base64url / RFC 3339 — and the
    // guard that writes it MUST sit above the catch-all. A catch-all that swallows them first is how they used to
    // render as `null`.
    if let Some(scalar) = ScalarView::from_value(value)
        && write_extended_json(buf, &scalar, usize::MAX)?
    {
        return Ok(None);
    }
    match value.untagged() {
        Value::Array(array) => {
            push(buf, "[")?;
            return Ok(Some(Cursor::Array { array, index: 0 }));
        }
        Value::Object(object) => {
            push(buf, "{")?;
            return Ok(Some(Cursor::Object { object, index: 0 }));
        }
        Value::Bool(true) => push(buf, "true")?,
        Value::Bool(false) => push(buf, "false")?,
        Value::Number(number) => write_number(buf, number, usize::MAX)?,
        Value::String(text) => write_string_literal(buf, text, usize::MAX)?,
        // `untagged()` loops until the value is NOT `Tagged`, so the `Tagged` pattern below is unreachable
        // exhaustiveness: a tag wrapper around any payload renders as its payload. That stripping is this writer's
        // declared tag law — rendering stays tag-insensitive.
        Value::Null | Value::Tagged { .. } => push(buf, "null")?,
        Value::Bytes(_)
        | Value::LocalDate(_)
        | Value::LocalTime(_)
        | Value::LocalDateTime(_)
        | Value::OffsetDateTime(_) => {
            unreachable!("extended kinds return before the match")
        }
    }
    Ok(None)
}

/// The canonical text of a temporal value, or `None` for any other kind.
///
/// This is the BARE text — no quotes — because its two consumers want opposite things: `tostring` publishes it
/// as-is (a temporal passes through the way a string does), while [`write_scalar_or_open`] quotes and escapes it so
/// `tojson` stays valid JSON. The spelling itself is `jqf-data`'s, beside the storage it describes, so a codec and the
/// engine cannot drift apart on it.
///
/// # Errors
///
/// Returns [`EngineRunError::Codec`] (allocation failure) when the buffer cannot grow.
pub fn temporal_text(value: &Value) -> Result<Option<String>, EngineRunError> {
    let mut out = String::new();
    let written = match value {
        Value::LocalDate(date) => date.write_text(&mut out),
        Value::LocalTime(time) => time.write_text(&mut out),
        Value::LocalDateTime(datetime) => datetime.write_text(&mut out),
        Value::OffsetDateTime(datetime) => datetime.write_text(&mut out),
        _ => return Ok(None),
    };
    written.map_err(|_| EngineRunError::allocation_failure())?;
    Ok(Some(out))
}

/// The canonical unpadded base64url text of a `bytes` value, or `None` for any other kind.
///
/// Bare text, like [`temporal_text`]: `tostring` / `@text` / `@base64` publish it as-is, while [`write_extended_json`]
/// quotes it so `tojson` stays valid JSON. The alphabet is RFC 4648 §5 without padding (RFC 8949 §6.5) — the same
/// projection the output codecs publish.
///
/// # Errors
///
/// The `Result` matches [`temporal_text`]'s shape so the two pass-throughs compose identically; this path does not fail
/// today.
pub fn bytes_text(value: &Value) -> Result<Option<String>, EngineRunError> {
    match value.untagged() {
        Value::Bytes(bytes) => Ok(Some(base64url_text(bytes))),
        _ => Ok(None),
    }
}

/// Writes the JSON string projection of a bytes or temporal scalar.
///
/// Returns `Ok(true)` when `scalar` is an extended kind and its projection was written, `Ok(false)` when it is a core
/// JSON kind the caller must handle. This is the one guard both the compact-JSON writer and the error-operand writer
/// consult, so the two cannot disagree on a bytes or temporal spelling.
///
/// # Errors
///
/// As [`write_string_literal`].
pub fn write_extended_json(buf: &mut String, scalar: &ScalarView<'_>, cap: usize) -> Result<bool, EngineRunError> {
    let text = match *scalar {
        ScalarView::Bytes(bytes) => base64url_text(bytes),
        ScalarView::LocalDate(date) => temporal_scalar_text(|out| date.write_text(out))?,
        ScalarView::LocalTime(time) => temporal_scalar_text(|out| time.write_text(out))?,
        ScalarView::LocalDateTime(datetime) => temporal_scalar_text(|out| datetime.write_text(out))?,
        ScalarView::OffsetDateTime(datetime) => temporal_scalar_text(|out| datetime.write_text(out))?,
        _ => return Ok(false),
    };
    write_string_literal(buf, &text, cap)?;
    Ok(true)
}

fn temporal_scalar_text(
    write: impl FnOnce(&mut String) -> Result<(), jqf_data::TemporalError>,
) -> Result<String, EngineRunError> {
    let mut out = String::new();
    write(&mut out).map_err(|_| EngineRunError::allocation_failure())?;
    Ok(out)
}

fn base64url_text(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Advances the innermost cursor by one child, closing containers that are done.
fn step<'value>(buf: &mut String, stack: &mut Vec<Cursor<'value>>) -> Result<Step<'value>, EngineRunError> {
    let Some(cursor) = stack.last_mut() else {
        return Ok(Step::Done);
    };
    match cursor {
        Cursor::Array { array, index } => {
            let (array, at) = (*array, *index);
            if let Some(child) = array.get(at) {
                if at > 0 {
                    push(buf, ",")?;
                }
                *index = at + 1;
                return Ok(Step::Child(child));
            }
            push(buf, "]")?;
        }
        Cursor::Object { object, index } => {
            let (object, at) = (*object, *index);
            if let Some(entry) = object.get_index(at) {
                if at > 0 {
                    push(buf, ",")?;
                }
                *index = at + 1;
                write_string_literal(buf, entry.key(), usize::MAX)?;
                push(buf, ":")?;
                return Ok(Step::Child(entry.value()));
            }
            push(buf, "}")?;
        }
    }
    stack.pop();
    Ok(Step::Closed)
}

/// Writes one owned number in the three spellings `jqf-data` defines — an integer verbatim, an exact decimal through
/// the scientific-string form, a computed binary64 through the shortest-round-trip form — stopping once `cap` bytes
/// are in the buffer. `cap` is [`usize::MAX`] for a full rendering; the bounded error-message writer passes its own
/// budget so a large numeric operand never materializes on the cold path.
///
/// # Errors
///
/// Returns an internal-contract error for a number carrying none of the three.
pub fn write_number(buf: &mut String, number: &Number, cap: usize) -> Result<(), EngineRunError> {
    if let Some(value) = number.as_machine() {
        return push_i64(buf, value, cap);
    }
    if let Some(integer) = number.to_integer() {
        return push_bounded(buf, integer.as_str(), cap);
    }
    if let Some(decimal) = number.as_decimal() {
        return push_decimal(buf, decimal.coefficient().as_str(), decimal.scale(), cap);
    }
    match number.as_float() {
        Some(value) => push_binary64(buf, value.get(), cap),
        None => Err(EngineRunError::internal_contract(
            "a number carrying no integer, decimal or binary64 representation",
        )),
    }
}

/// Appends an exact decimal in the scientific-string form, stopping once `cap` bytes are in the buffer.
///
/// # Errors
///
/// Returns an internal-contract error for a coefficient/scale pair that is not a representable decimal, or an
/// allocation failure.
pub fn push_decimal(buf: &mut String, coefficient: &str, scale: i64, cap: usize) -> Result<(), EngineRunError> {
    let text = DecimalText::new(coefficient, scale)
        .ok_or_else(|| EngineRunError::internal_contract("a decimal with no scientific-string form"))?;
    for piece in text.pieces() {
        let piece = core::str::from_utf8(piece)
            .map_err(|_| EngineRunError::internal_contract("a decimal rendering that is not UTF-8"))?;
        if buf.len() >= cap {
            break;
        }
        push_bounded(buf, piece, cap)?;
    }
    Ok(())
}

/// Appends a computed binary64 value in the shortest-round-trip form, stopping once `cap` bytes are in the buffer.
///
/// # Errors
///
/// Returns an internal-contract error for a value with no shortest-round-trip form, or an allocation failure.
pub fn push_binary64(buf: &mut String, value: f64, cap: usize) -> Result<(), EngineRunError> {
    let text =
        format_binary64(value).ok_or_else(|| EngineRunError::internal_contract("a binary64 with no dtoa form"))?;
    push_bounded(buf, text.as_str(), cap)
}

/// Writes `value`'s canonical decimal digits into `buf`, matching [`jqf_data::Integer::from_i64`] spelling. No heap
/// integer is built.
fn push_i64(buf: &mut String, value: i64, cap: usize) -> Result<(), EngineRunError> {
    let mut digits = [0_u8; 20];
    push_bounded(buf, i64_decimal(value, &mut digits), cap)
}

/// Canonical signed decimal of a machine integer. `i64::MIN` is 20 bytes.
fn i64_decimal(value: i64, buf: &mut [u8; 20]) -> &str {
    let mut magnitude = value.unsigned_abs();
    let mut end = 20;
    loop {
        end -= 1;
        buf[end] = b'0' + (magnitude % 10) as u8;
        magnitude /= 10;
        if magnitude == 0 {
            break;
        }
    }
    if value < 0 {
        end -= 1;
        buf[end] = b'-';
    }
    // SAFETY: only ASCII digits and an optional leading '-'.
    unsafe { core::str::from_utf8_unchecked(&buf[end..]) }
}

/// Appends at most the bytes of `text` that fit under `cap`, never splitting a character.
///
/// The one writer for ASCII number spellings: a large coefficient run is cut at the byte budget instead of
/// materializing in full, which is what keeps the bounded message writer's promise for numeric operands. Number
/// spellings are ASCII, so a byte cut is a char boundary.
pub fn push_bounded(buf: &mut String, text: &str, cap: usize) -> Result<(), EngineRunError> {
    if buf.len() >= cap {
        return Ok(());
    }
    let budget = cap - buf.len();
    let cut = if text.len() > budget {
        // Number spellings are ASCII; a byte cut is a char boundary.
        &text[..budget]
    } else {
        text
    };
    push(buf, cut)
}

/// Writes a JSON string literal — quotes plus the shared escape set — stopping once `cap` bytes are in the buffer.
///
/// `cap` is [`usize::MAX`] for a full rendering; the bounded error-message writer passes its own budget so a large
/// operand never materializes on the cold path. The escape set is `jqf-codec-json`'s single law ([`json_escape_byte`]):
/// DEL is escaped even though the grammar does not require it, and every non-ASCII codepoint passes through raw even
/// though `\u` escapes would be legal.
///
/// # Errors
///
/// Returns [`EngineRunError::Codec`] (allocation failure) when reserving fails.
pub fn write_string_literal(buf: &mut String, text: &str, cap: usize) -> Result<(), EngineRunError> {
    push(buf, "\"")?;
    for ch in text.chars() {
        let mut scratch = [0_u8; 4];
        let bytes = ch.encode_utf8(&mut scratch).as_bytes();
        // Only ASCII bytes are in the escape set; every byte of a multi-byte char is >= 0x80, so the decision is read
        // from the first byte and a non-escaping char is pushed whole.
        let (len, escape) = json_escape_byte(bytes[0]);
        if len > 0 {
            push(
                buf,
                core::str::from_utf8(&escape[..len as usize]).expect("escapes are ASCII"),
            )?;
        } else {
            push(buf, core::str::from_utf8(bytes).expect("a char's own UTF-8 is valid"))?;
        }
        if buf.len() >= cap {
            break;
        }
    }
    push(buf, "\"")
}

/// Appends `text` to `buf`, fallibly.
fn push(buf: &mut String, text: &str) -> Result<(), EngineRunError> {
    buf.try_reserve(text.len())
        .map_err(|_| EngineRunError::allocation_failure())?;
    buf.push_str(text);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{bytes_text, temporal_text, to_json, write_number};
    use alloc::string::String;
    use jqf_data::{Array, Integer, LocalDate, Number, Value};
    use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

    static CONTROL: ContinueControl = ContinueControl;

    fn ledger() -> ResourceContext<'static> {
        let limits = ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
        let account = RequestAccount::try_new(limits).expect("account");
        let work = WorkMeter::try_new_v1(1).expect("work");
        ResourceContext::new(account, &CONTROL, work).expect("ledger")
    }

    fn bytes(payload: &[u8]) -> Value {
        Value::try_bytes(payload).expect("bytes")
    }

    /// The extended-kind matrix: bytes render as unpadded base64url, a temporal as its RFC 3339 text, and both stay
    /// that projection inside a container — never `null`.
    #[test]
    fn extended_kinds_render_as_their_declared_projection() {
        let abc = bytes(b"abc");
        assert_eq!(bytes_text(&abc).expect("text").as_deref(), Some("YWJj"));
        assert_eq!(to_json(&abc).expect("json"), "\"YWJj\"");

        let date = Value::LocalDate(LocalDate::new(2024, 1, 2).expect("date"));
        assert_eq!(temporal_text(&date).expect("text").as_deref(), Some("2024-01-02"));
        assert_eq!(to_json(&date).expect("json"), "\"2024-01-02\"");

        let _resources = ledger();
        let mut array = Array::try_new().expect("array");
        array.try_push(abc).expect("push bytes");
        array
            .try_push(Value::Number(Number::integer(jqf_data::Integer::from_i64(1))))
            .expect("push one");
        assert_eq!(to_json(&Value::Array(array)).expect("json"), "[\"YWJj\",1]");
    }

    #[test]
    fn machine_int_render_matches_integer_from_i64() {
        for n in [0_i64, -1, 1, 10, -10, i64::MIN, i64::MAX, 42, -999] {
            let mut direct = String::new();
            write_number(&mut direct, &Number::integer(Integer::from_i64(n)), usize::MAX).expect("write");
            assert_eq!(direct, Integer::from_i64(n).as_str(), "n={n}");
        }
    }
}
