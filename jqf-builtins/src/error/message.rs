//! Parity error-message rendering for catch-visible and CLI-visible strings.
//!
//! One job: turn a runtime semantic error into the EXACT message string the reference renders, so a `try … catch .`
//! sees byte-identical text and the CLI's uncaught-raise line matches. It owns two crate-private pieces the vertical
//! needs: [`kind_name`] (the lowercase value-kind words) and [`dump_trunc`] / [`dump_trunc_owned`] (a BOUNDED
//! compact-JSON writer applying the truncation law — no codec dependency). The operand text is captured at the raise
//! site (bounded, so the full string never materializes); the class templates are assembled here.
//!
//! Negative space: it owns neither number formatting nor string escaping — both come from
//! [`crate::semantics::render`], the engine's one compact-JSON writer, so an operand's spelling inside a message and
//! outside one cannot drift apart (a divergence that was live until this vertical: the reference writes `\b` and `\f`
//! where this module used to write `\u0008` and `\u000c`). What stays here is the BOUNDED, TRUNCATING walk over a
//! located view, which the renderer has no business knowing about. It parses nothing and drives no execution.

use alloc::string::String;

use jqf_data::{NumberView, ScalarView, Value, ValueKind, ValueView};

use crate::codec_result::EngineResult;
use crate::error::EngineRunError;
use crate::semantics::render::{
    push_binary64, push_bounded, push_decimal, write_extended_json, write_number, write_string_literal,
};

/// The lowercase value-kind word (`null`, `boolean`, `number`, `string`, `array`, `object`). Bytes and the four
/// temporals fall back to `string` because their JSON projection is a string; the operand text beside the word is that
/// projection (base64url / RFC 3339), never `null` or `""`.
pub const fn kind_name(kind: ValueKind) -> &'static str {
    match kind {
        ValueKind::Null => "null",
        ValueKind::Bool => "boolean",
        ValueKind::Number => "number",
        ValueKind::Array => "array",
        ValueKind::Object => "object",
        // Bytes and the four temporals project as JSON strings, so the kind word is `string`. The operand text is
        // written by [`crate::semantics::render::write_extended_json`].
        _ => "string",
    }
}

/// The byte budget for the bounded writer: enough to observe the 29-byte keep threshold and carry the
/// 26-byte-plus-delimiter truncation, without ever materializing a large operand's full JSON on the (cold) error path.
const CAP: usize = 48;

/// The budget for the ONE class the reference prints untruncated: an uncaught raised non-string, whose value the
/// program already materialized (rendering it is O(bytes that exist), so it opens no new unboundedness).
const UNBOUNDED: usize = usize::MAX;

/// Renders one operand (located or owned) to its bounded, truncated compact JSON.
///
/// # Errors
///
/// Returns [`EngineRunError::Codec`] (allocation failure) when reserving the bounded buffer fails, or an
/// internal-contract error when a located view over a valid document is malformed.
pub fn dump_trunc(result: &EngineResult<'_>) -> Result<String, EngineRunError> {
    let mut buf = String::new();
    write_result(&mut buf, result, CAP)?;
    truncate(buf)
}

/// Renders one owned operand to its bounded, truncated compact JSON.
///
/// # Errors
///
/// Returns [`EngineRunError::Codec`] (allocation failure) when reserving fails.
pub fn dump_trunc_owned(value: &Value) -> Result<String, EngineRunError> {
    let mut buf = String::new();
    write_owned(&mut buf, value, CAP)?;
    truncate(buf)
}

/// Renders raw TEXT as a bounded, truncated JSON string literal.
///
/// For the messages whose operand is a string the caller PRODUCED rather than a value it holds: `@base64d` and `@urid`
/// stringify their input before they validate it, so `1 | @base64d` names `string ("1")` and there is no `Value`
/// carrying that text.
///
/// # Errors
///
/// Returns [`EngineRunError::Codec`] (allocation failure) when reserving fails.
pub fn dump_trunc_text(text: &str) -> Result<String, EngineRunError> {
    let mut buf = String::new();
    write_string_literal(&mut buf, text, CAP).map_err(|_| EngineRunError::allocation_failure())?;
    truncate(buf)
}

/// The text an UNCAUGHT program-raised error prints AFTER the frame's colon: a string value verbatim, a non-string
/// value as its compact JSON.
///
/// The non-string case is marked with a ` (not a string)` clause placed BEFORE the colon, not in this body —
/// [`raised_frame_note`] carries that clause. The JSON is UNTRUNCATED: unlike a kind-dispatch operand (whose interior
/// the program never consumed), a raised value is fully materialized at the raise site, so rendering it costs only the
/// bytes that already exist.
///
/// # Errors
///
/// Returns [`EngineRunError::Codec`] (allocation failure) when reserving fails.
pub fn raised_body(value: &Value) -> Result<String, EngineRunError> {
    let mut out = String::new();
    match value {
        Value::String(text) => push(&mut out, text)?,
        other => write_owned(&mut out, other, UNBOUNDED)?,
    }
    Ok(out)
}

/// The clause the reference inserts between an uncaught raise's location and its colon:
/// empty for a raised string, ` (not a string)` for every other raised value.
#[must_use]
pub const fn raised_frame_note(value: &Value) -> &'static str {
    match value {
        Value::String(_) => "",
        _ => " (not a string)",
    }
}

/// Renders one located view to its bounded, truncated compact JSON.
///
/// # Errors
///
/// Returns [`EngineRunError::Codec`] (allocation failure) when reserving fails, or an internal-contract error when the
/// view over a valid document is malformed.
pub fn dump_trunc_view(view: &ValueView<'_, '_>) -> Result<String, EngineRunError> {
    let mut buf = String::new();
    write_view(&mut buf, view, CAP)?;
    truncate(buf)
}

/// Renders an object key as the typed key interpolation (`string ("a")`).
///
/// # Errors
///
/// Returns [`EngineRunError::Codec`] (allocation failure) when reserving fails.
pub fn render_object_key(name: &str) -> Result<String, EngineRunError> {
    let mut out = String::new();
    push(&mut out, "string (")?;
    write_string_literal(&mut out, name, CAP)?;
    push(&mut out, ")")?;
    Ok(out)
}

/// Renders an array index as the typed key interpolation (`number (0)`).
///
/// # Errors
///
/// Returns [`EngineRunError::Codec`] (allocation failure) when reserving fails.
pub fn render_array_key(index: i64) -> Result<String, EngineRunError> {
    let mut out = String::new();
    push(&mut out, "number (")?;
    push_display(&mut out, index)?;
    push(&mut out, ")")?;
    Ok(out)
}

/// Renders a `.[$x]` dynamic index accessor as the typed key interpolation (`boolean (true)`, `number (1)`, `null
/// (null)`) — the bound value's own kind word plus its bounded compact JSON. A string bound value renders exactly as
/// [`render_object_key`] would, and a number exactly as [`render_array_key`].
///
/// The bound operand is a located handle or an owned value (env slots hold either), and both render through the same
/// bounded writer — the message text does not depend on which representation the slot happens to hold.
///
/// # Errors
///
/// Returns [`EngineRunError::Codec`] (allocation failure) when reserving fails, or an internal-contract error when a
/// located view is malformed.
pub fn render_dynamic_key(value: &EngineResult<'_>) -> Result<String, EngineRunError> {
    let json = dump_trunc(value)?;
    join(&[kind_name(result_kind(value)?), " (", &json, ")"])
}

/// Renders an OWNED operand as the typed key interpolation — the owned-value half of [`render_dynamic_key`], for an
/// accessor the engine SYNTHESIZES rather than reads out of the program or an env slot.
///
/// Its caller is the slice mismatch, whose accessor is the object `{"start":…,"end":…}` built from the AUTHORED
/// bounds:
/// `Cannot index object with object ({"start":1.5,"end":3})`. The operand is a synthesized value, so the writer must
/// treat it as ordinary owned JSON and never try to navigate or charge it like a document value — which is exactly
/// what [`dump_trunc_owned`] does, under the same truncation law.
///
/// # Errors
///
/// Returns [`EngineRunError::Codec`] (allocation failure) when reserving fails.
pub fn render_owned_key(value: &Value) -> Result<String, EngineRunError> {
    let json = dump_trunc_owned(value)?;
    join(&[kind_name(crate::semantics::owned_kind(value)), " (", &json, ")"])
}

/// The math-family refusal for a non-number operand:
/// `<kind> (<operand>) number required` — `string ("hello") number required`, `null (null) number required`, `object
/// ({}) number required`. The math vertical's /0, /2 and /3 laws all raise this same sentence for the value they tried
/// to read as a number (`abs` excepted, which is the `if . < 0 then - . else . end` definition and refuses through
/// negate).
///
/// # Errors
///
/// Returns [`EngineRunError::Codec`] (allocation failure) when reserving fails.
pub fn number_required(value: &Value) -> Result<String, EngineRunError> {
    let operand = dump_trunc_owned(value)?;
    join(&[
        kind_name(crate::semantics::owned_kind(value)),
        " (",
        &operand,
        ") number required",
    ])
}

/// The payload-transparent semantic category of a located or owned operand, for the dynamic key's kind word. The ONE
/// `owned_kind` law lives in [`crate::semantics::owned_kind`]; this module renders its kind words, so an owned bytes or
/// temporal scalar reports its own kind, never a folded `number`.
fn result_kind(value: &EngineResult<'_>) -> Result<ValueKind, EngineRunError> {
    match value {
        EngineResult::Owned(owned) => Ok(crate::semantics::owned_kind(owned)),
        EngineResult::Located(located) => located
            .product()
            .document()
            .payload_view(located.node())
            .and_then(ValueView::kind)
            .map_err(|_| internal()),
    }
}

/// Applies the truncation law: a compact JSON dump of length ≤ 29 is kept whole; otherwise the leading 25 bytes
/// (delimited kinds) or 26 bytes (numbers), UTF-8-backtracked to a char boundary, get `...` appended, and the closing
/// delimiter re-appended for a string/array/object. The longest rendered message is 29 bytes.
fn truncate(json: String) -> Result<String, EngineRunError> {
    if json.len() <= 29 {
        return Ok(json);
    }
    let (keep, delim) = match json.as_bytes().first() {
        Some(b'"') => (25usize, Some('"')),
        Some(b'[') => (25, Some(']')),
        Some(b'{') => (25, Some('}')),
        _ => (26, None),
    };
    // The `keep.min(json.len())` clamp that used to stand here can never bind: the early return above already refused
    // `len <= 29`, and keep is 25 or 26.
    let mut boundary = keep;
    while boundary > 0 && !json.is_char_boundary(boundary) {
        boundary -= 1;
    }
    let mut out = String::new();
    push(&mut out, &json[..boundary])?;
    push(&mut out, "...")?;
    if let Some(delim) = delim {
        out.try_reserve(delim.len_utf8())
            .map_err(|_| EngineRunError::allocation_failure())?;
        out.push(delim);
    }
    Ok(out)
}

/// Appends `text` to `buf`, fallibly.
fn push(buf: &mut String, text: &str) -> Result<(), EngineRunError> {
    buf.try_reserve(text.len())
        .map_err(|_| EngineRunError::allocation_failure())?;
    buf.push_str(text);
    Ok(())
}

/// Appends a `Display` value (a small integer) to `buf`, fallibly — the scratch `String` the old body wrote into
/// first was an allocation whose OOM path aborted identically to a direct `write!`.
fn push_display(buf: &mut String, value: i64) -> Result<(), EngineRunError> {
    use core::fmt::Write as _;
    write!(buf, "{value}").map_err(|_| EngineRunError::allocation_failure())
}

/// Writes one located-or-owned operand's compact JSON, stopping once the bounded budget is reached (the cold error path
/// never renders a large operand fully).
fn write_result(buf: &mut String, result: &EngineResult<'_>, cap: usize) -> Result<(), EngineRunError> {
    match result {
        EngineResult::Owned(value) => write_owned(buf, value, cap),
        EngineResult::Located(located) => {
            // The payload-transparent view: a tag LAYER is descended before the node is rendered, so a tagged operand's
            // error message shows the payload the law speaks about.
            let view = located
                .product()
                .document()
                .payload_view(located.node())
                .map_err(|_| internal())?;
            write_view(buf, &view, cap)
        }
    }
}

/// Writes one owned value's compact JSON, stopping once `cap` bytes are in the buffer ([`CAP`] for a kind-dispatch
/// operand, [`UNBOUNDED`] for a raised value).
fn write_owned(buf: &mut String, value: &Value, cap: usize) -> Result<(), EngineRunError> {
    if buf.len() >= cap {
        return Ok(());
    }
    // Extended kinds must be formatted before the catch-all: bytes used to render as `""` and temporals as `null`.
    if let Some(scalar) = ScalarView::from_value(value)
        && write_extended_json(buf, &scalar, cap)?
    {
        return Ok(());
    }
    match value {
        Value::Null => push(buf, "null"),
        Value::Bool(true) => push(buf, "true"),
        Value::Bool(false) => push(buf, "false"),
        Value::Number(number) => write_number(buf, number, cap),
        Value::String(text) => write_string_literal(buf, text, cap),
        Value::Bytes(_)
        | Value::LocalDate(_)
        | Value::LocalTime(_)
        | Value::LocalDateTime(_)
        | Value::OffsetDateTime(_) => {
            unreachable!("extended kinds return before the match")
        }
        Value::Array(array) => {
            push(buf, "[")?;
            let mut index = 0;
            while let Some(child) = array.get(index) {
                if index > 0 {
                    push(buf, ",")?;
                }
                if buf.len() >= cap {
                    return Ok(());
                }
                write_owned(buf, child, cap)?;
                index += 1;
            }
            push(buf, "]")
        }
        Value::Object(object) => {
            push(buf, "{")?;
            let mut index = 0;
            while let Some(entry) = object.get_index(index) {
                if index > 0 {
                    push(buf, ",")?;
                }
                if buf.len() >= cap {
                    return Ok(());
                }
                write_string_literal(buf, entry.key(), cap)?;
                push(buf, ":")?;
                write_owned(buf, entry.value(), cap)?;
                index += 1;
            }
            push(buf, "}")
        }
        Value::Tagged { payload, .. } => write_owned(buf, payload, cap),
    }
}

/// Writes one located view's compact JSON, stopping once `cap` bytes are in the buffer (see [`write_owned`]).
#[allow(
    clippy::match_same_arms,
    reason = "the explicit Null arm and the non-JSON fallback both render `null` but state \
              distinct intents: a real null versus a best-effort non-JSON rendering"
)]
fn write_view(buf: &mut String, view: &ValueView<'_, '_>, cap: usize) -> Result<(), EngineRunError> {
    if buf.len() >= cap {
        return Ok(());
    }
    if let Some(scalar) = view.scalar().map_err(|_| internal())?
        && write_extended_json(buf, &scalar, cap)?
    {
        return Ok(());
    }
    match view.kind().map_err(|_| internal())? {
        ValueKind::Null => push(buf, "null"),
        ValueKind::Bool => match view.scalar().map_err(|_| internal())? {
            Some(ScalarView::Bool(true)) => push(buf, "true"),
            Some(ScalarView::Bool(false)) => push(buf, "false"),
            _ => Err(internal()),
        },
        ValueKind::Number => match view.scalar().map_err(|_| internal())? {
            Some(ScalarView::Number(number)) => write_number_view(buf, number, cap),
            _ => Err(internal()),
        },
        ValueKind::String => match view.scalar().map_err(|_| internal())? {
            Some(ScalarView::String(text)) => write_string_literal(buf, text, cap),
            _ => Err(internal()),
        },
        ValueKind::Array => {
            let array = view.array().map_err(|_| internal())?.ok_or_else(internal)?;
            push(buf, "[")?;
            let mut index = 0;
            let mut first = true;
            while let Some(child) = array.get(index) {
                if !first {
                    push(buf, ",")?;
                }
                first = false;
                if buf.len() >= cap {
                    return Ok(());
                }
                write_view(buf, &child, cap)?;
                index += 1;
            }
            push(buf, "]")
        }
        ValueKind::Object => {
            let object = view.object().map_err(|_| internal())?.ok_or_else(internal)?;
            push(buf, "{")?;
            let mut index = 0;
            while let Some(entry) = object.get_index(index).map_err(|_| internal())? {
                if index > 0 {
                    push(buf, ",")?;
                }
                if buf.len() >= cap {
                    return Ok(());
                }
                write_string_literal(buf, entry.key(), cap)?;
                push(buf, ":")?;
                write_view(buf, &entry.value(), cap)?;
                index += 1;
            }
            push(buf, "}")
        }
        // Bytes and temporals return before this match. A future kind falls back to `null` rather than aborting the
        // error path.
        _ => push(buf, "null"),
    }
}

/// Writes a located number in the SAME text the JSON encoder would publish:
/// integers verbatim, exact decimals through the scientific-string form, computed binary64 values through the
/// shortest-round-trip form, stopping once `cap` bytes are in the buffer.
fn write_number_view(buf: &mut String, number: NumberView<'_>, cap: usize) -> Result<(), EngineRunError> {
    match number {
        NumberView::Integer(text) => push_bounded(buf, text, cap),
        NumberView::Number(value) => write_number(buf, value, cap),
        NumberView::Decimal { coefficient, scale } => push_decimal(buf, coefficient, scale, cap),
        NumberView::Float(value) => push_binary64(buf, value.get(), cap),
    }
}

/// The internal-contract error for a malformed located view over a valid document.
fn internal() -> EngineRunError {
    EngineRunError::internal_contract("operand rendering over a valid located document failed")
}

/// Joins message parts into one fallibly-allocated string.
pub fn join(parts: &[&str]) -> Result<String, EngineRunError> {
    let total: usize = parts.iter().map(|part| part.len()).sum();
    let mut out = String::new();
    out.try_reserve_exact(total)
        .map_err(|_| EngineRunError::allocation_failure())?;
    for part in parts {
        out.push_str(part);
    }
    Ok(out)
}

/// the index-class message: `Cannot index <container> with <key>`.
pub fn index_message(container: ValueKind, key: &str) -> Result<String, EngineRunError> {
    join(&["Cannot index ", kind_name(container), " with ", key])
}

/// the iterate-class message: `Cannot iterate over <kind> (<operand>)`.
pub fn iterate_message(kind: ValueKind, operand: &str) -> Result<String, EngineRunError> {
    join(&["Cannot iterate over ", kind_name(kind), " (", operand, ")"])
}

/// the no-length message: `<kind> (<operand>) has no length`.
pub fn no_length_message(kind: ValueKind, operand: &str) -> Result<String, EngineRunError> {
    join(&[kind_name(kind), " (", operand, ") has no length"])
}

/// the no-keys message: `<kind> (<operand>) has no keys`.
pub fn no_keys_message(kind: ValueKind, operand: &str) -> Result<String, EngineRunError> {
    join(&[kind_name(kind), " (", operand, ") has no keys"])
}

/// the negation refusal: `<kind> (<operand>) cannot be negated`.
///
/// Unary `-` has its OWN class, which is why negation is not lowered to `0 - expr`: the subtraction would say `number
/// (0) and string ("a") cannot be subtracted`, naming an operand the program never wrote.
pub fn not_negatable_message(kind: ValueKind, operand: &str) -> Result<String, EngineRunError> {
    join(&[kind_name(kind), " (", operand, ") cannot be negated"])
}

/// the has-key refusal: `Cannot check whether <kind> has a <keykind> key`.
///
/// The one message class here that names TWO kinds and NEITHER operand: the reference reports the container's kind and
/// the key's kind and renders no value at all, so a refusal over a megabyte-wide object is a fixed-length sentence.
pub fn has_key_message(container: ValueKind, key: ValueKind) -> Result<String, EngineRunError> {
    join(&[
        "Cannot check whether ",
        kind_name(container),
        " has a ",
        kind_name(key),
        " key",
    ])
}

/// the SINGLE-operand ordering message, which only the arity-0 `sort` and `unique` raise: `<kind> (<operand>) cannot be
/// sorted, as it is not an array`.
pub fn not_sortable_message(kind: ValueKind, operand: &str) -> Result<String, EngineRunError> {
    join(&[
        kind_name(kind),
        " (",
        operand,
        ") cannot be sorted, as it is not an array",
    ])
}

/// the DOUBLED ordering message, raised by the `_by` forms of `sort`, `group` and `unique`: `<k> (<l>) and <k> (<r>)
/// cannot be sorted, as they are not both arrays`.
///
/// The right operand is the boxed key array the `map([f])` spelling materializes and jqf's keyed frame does not — the
/// frame fabricates it for this message alone.
pub fn not_both_arrays_message(
    left_kind: ValueKind,
    left: &str,
    right_kind: ValueKind,
    right: &str,
) -> Result<String, EngineRunError> {
    join(&[
        kind_name(left_kind),
        " (",
        left,
        ") and ",
        kind_name(right_kind),
        " (",
        right,
        ") cannot be sorted, as they are not both arrays",
    ])
}

/// the DOUBLED iteration message, raised by the min/max family:
/// `<k> (<l>) and <k> (<r>) cannot be iterated over`.
///
/// The arity-0 forms pass the SAME value in both positions (the `f_min` hands its input to the two-operand
/// implementation as both the values and the keys), which is why `null | min` names `null` twice.
pub fn not_iterable_pair_message(
    left_kind: ValueKind,
    left: &str,
    right_kind: ValueKind,
    right: &str,
) -> Result<String, EngineRunError> {
    join(&[
        kind_name(left_kind),
        " (",
        left,
        ") and ",
        kind_name(right_kind),
        " (",
        right,
        ") cannot be iterated over",
    ])
}

/// the `bsearch` domain message: `<kind> (<operand>) cannot be searched from`.
pub fn not_searchable_message(kind: ValueKind, operand: &str) -> Result<String, EngineRunError> {
    join(&[kind_name(kind), " (", operand, ") cannot be searched from"])
}

/// the unknown-format message: `<name> is not a valid format`.
///
/// The one class that renders its operand RAW — no kind word, no quotes, no truncation — because the operand is a
/// format NAME and not a value:
/// `format("")` reports the empty name as an empty span.
pub fn invalid_format_message(name: &str) -> Result<String, EngineRunError> {
    join(&[name, " is not a valid format"])
}

/// the non-string format-name message:
/// `<kind> (<operand>) is not a valid format`.
pub fn invalid_format_kind_message(kind: ValueKind, operand: &str) -> Result<String, EngineRunError> {
    join(&[kind_name(kind), " (", operand, ") is not a valid format"])
}

/// the delimited-row input refusal:
/// `<kind> (<operand>) cannot be <format>-formatted, only array`.
pub fn not_formattable_message(kind: ValueKind, operand: &str, format: &str) -> Result<String, EngineRunError> {
    join(&[
        kind_name(kind),
        " (",
        operand,
        ") cannot be ",
        format,
        "-formatted, only array",
    ])
}

/// the delimited-row CELL refusal: `<kind> (<operand>) is not valid in a csv row`.
///
/// It says "csv" inside `@tsv` too. That is the reference's own wording, verified by probe, and it is preserved rather
/// than corrected — the compat corpus pins it.
pub fn invalid_row_cell_message(kind: ValueKind, operand: &str) -> Result<String, EngineRunError> {
    join(&[kind_name(kind), " (", operand, ") is not valid in a csv row"])
}

/// the `@sh` refusal: `<kind> (<operand>) can not be escaped for shell`.
///
/// "can not", two words, is the spelling.
pub fn not_shell_escapable_message(kind: ValueKind, operand: &str) -> Result<String, EngineRunError> {
    join(&[kind_name(kind), " (", operand, ") can not be escaped for shell"])
}

/// the `@urid` refusal: `string (<operand>) is not a valid uri encoding`.
///
/// Raised both for a malformed escape and for well-formed escapes whose bytes are not UTF-8; the kind is always
/// `string`, because the format stringifies its input first.
pub fn invalid_uri_encoding_message(operand: &str) -> Result<String, EngineRunError> {
    join(&["string (", operand, ") is not a valid uri encoding"])
}

/// the `@base64d` LENGTH refusal: `string (<operand>) trailing base64 byte found`.
pub fn trailing_base64_message(operand: &str) -> Result<String, EngineRunError> {
    join(&["string (", operand, ") trailing base64 byte found"])
}

/// the `@base64d` ALPHABET refusal: `string (<operand>) is not valid base64 data`.
pub fn invalid_base64_message(operand: &str) -> Result<String, EngineRunError> {
    join(&["string (", operand, ") is not valid base64 data"])
}

/// the `tonumber` refusal: `<kind> (<operand>) cannot be parsed as a number`.
pub fn unparsable_number_message(kind: ValueKind, operand: &str) -> Result<String, EngineRunError> {
    join(&[kind_name(kind), " (", operand, ") cannot be parsed as a number"])
}

/// the `toboolean` refusal: `<kind> (<operand>) cannot be parsed as a boolean`.
///
/// Raised for every kind but the two booleans and the two exact strings `"true"` and `"false"` — an embedded NUL does
/// not terminate the comparison, so `"true\u{0}x"` is refused too.
pub fn unparsable_boolean_message(kind: ValueKind, operand: &str) -> Result<String, EngineRunError> {
    join(&[kind_name(kind), " (", operand, ") cannot be parsed as a boolean"])
}

/// the `fromjson` KIND refusal: `<kind> (<operand>) only strings can be parsed`.
///
/// The operand is bounded like every other one here; the parse message below is the exception that is not.
pub fn only_strings_message(kind: ValueKind, operand: &str) -> Result<String, EngineRunError> {
    join(&[kind_name(kind), " (", operand, ") only strings can be parsed"])
}

/// Where a `fromjson` parse fault sits, in the accounting.
pub struct JsonPlace {
    /// 1 plus the newlines the reader consumed.
    pub line: u32,
    /// Bytes consumed on that line, so a newline resets it to zero.
    pub column: u32,
    /// Whether the fault was found after the last byte, which the reference says out loud.
    pub at_eof: bool,
}

/// the `fromjson` parse message:
/// `<reason>[ at EOF][ at line L, column C] (while parsing '<input>')`.
///
/// The echoed input is NOT bounded, unlike every operand rendering in this module: the reference prints the whole text
/// it was given, and a suite case asserts that a 20 076-character echo arrives intact. `place` is absent for exactly
/// the two reasons that describe the whole input rather than a position in it.
pub fn json_parse_message(reason: &str, place: Option<&JsonPlace>, echo: &str) -> Result<String, EngineRunError> {
    let mut out = json_parse_message_head(reason, place)?;
    push(&mut out, " (while parsing '")?;
    push(&mut out, echo)?;
    push(&mut out, "')")?;
    Ok(out)
}

/// the parse message WITHOUT the echoed input:
/// `<reason>[ at EOF][ at line L, column C]`.
///
/// This is the form the `--slurpfile` reports (`Bad JSON in --slurpfile NAME FILE: <this>`), where the input is a named
/// file and no echo follows; `decode.rs`'s adjacent-value reader is the only caller.
pub fn json_parse_message_head(reason: &str, place: Option<&JsonPlace>) -> Result<String, EngineRunError> {
    let mut out = String::new();
    push(&mut out, reason)?;
    if let Some(place) = place {
        if place.at_eof {
            push(&mut out, " at EOF")?;
        }
        push(&mut out, " at line ")?;
        push_display(&mut out, i64::from(place.line))?;
        push(&mut out, ", column ")?;
        push_display(&mut out, i64::from(place.column))?;
    }
    Ok(out)
}

/// the `utf8bytelength` refusal:
/// `<kind> (<operand>) only strings have UTF-8 byte length`.
pub fn not_measurable_message(kind: ValueKind, operand: &str) -> Result<String, EngineRunError> {
    join(&[kind_name(kind), " (", operand, ") only strings have UTF-8 byte length"])
}

/// the `implode` CELL refusal: `<kind> (<operand>) can't be imploded, unicode codepoint needs to be numeric`.
pub fn not_a_codepoint_message(kind: ValueKind, operand: &str) -> Result<String, EngineRunError> {
    join(&[
        kind_name(kind),
        " (",
        operand,
        ") can't be imploded, unicode codepoint needs to be numeric",
    ])
}

/// the operand-LESS input refusals, one per name the reference spells.
///
/// Three input classes report a bare sentence rather than naming the offending value, and the name in the sentence is
/// not always the builtin that raised it:
/// `ascii_downcase` says `explode` (the definition DEFINES the case pair over `explode`), and `ltrim`/`rtrim` both say
/// `trim`. Those borrowings are the reason this takes the name as a parameter instead of the callers each spelling
/// their own.
/// The refusal the prefix predicates report: `<name>() requires string inputs`.
///
/// One message for both sides, which is the reference's choice and not a shortcut: the text names the BUILTIN and never
/// says which operand was wrong, so `startswith(1)` and `1 | startswith("a")` are byte-identical. The `…str` trims
/// borrow it — `ltrimstr` reports `startswith` and `rtrimstr` reports `endswith` — because the reference defines
/// them over the predicates.
pub fn requires_string_inputs_message(name: &str) -> Result<String, EngineRunError> {
    join(&[name, "() requires string inputs"])
}

/// `_strindices`'s INPUT refusal: `<kind> (<operand>) cannot be searched, as it is not a string`.
///
/// A different message from [`not_searchable_message`], which is `bsearch`'s; The reference spells the two searches'
/// refusals separately.
///
/// Its own wording, and not the `requires string inputs` family's: the internal builtin names the operand rather than
/// the call, and it distinguishes the two sides where the prefix predicates deliberately do not.
///
/// # Errors
///
/// Returns [`EngineRunError::Codec`] (allocation failure) when reserving fails.
pub fn not_searchable_string_message(kind: ValueKind, operand: &str) -> Result<String, EngineRunError> {
    join(&[
        kind_name(kind),
        " (",
        operand,
        ") cannot be searched, as it is not a string",
    ])
}

/// `_strindices`'s ARGUMENT refusal: `<kind> (<operand>) is not a string`.
///
/// # Errors
///
/// Returns [`EngineRunError::Codec`] (allocation failure) when reserving fails.
pub fn not_a_string_message(kind: ValueKind, operand: &str) -> Result<String, EngineRunError> {
    join(&[kind_name(kind), " (", operand, ") is not a string"])
}

/// the containment refusal: `<k> (<l>) and <k> (<r>) cannot have their containment checked`.
///
/// Raised only for a TOP-LEVEL kind mismatch, and the value model counts `true` and `false` as two kinds — so a
/// boolean pair that disagrees reports this while a number pair that disagrees answers `false`. One level down the same
/// mismatch is `false` too, which is why the relation itself never reaches here.
///
/// # Errors
///
/// Returns [`EngineRunError::Codec`] (allocation failure) when reserving fails.
pub fn containment_message(
    left_kind: ValueKind,
    left: &str,
    right_kind: ValueKind,
    right: &str,
) -> Result<String, EngineRunError> {
    join(&[
        kind_name(left_kind),
        " (",
        left,
        ") and ",
        kind_name(right_kind),
        " (",
        right,
        ") cannot have their containment checked",
    ])
}

/// `split/1`'s refusal: `split input and separator must be strings`.
///
/// One fixed text for both sides and for every wrong kind — the builtin does not borrow the `/` operator's `cannot be
/// divided` message even though it borrows the operator's cut.
///
/// # Errors
///
/// Returns [`EngineRunError::Codec`] (allocation failure) when reserving fails.
pub fn split_inputs_message() -> Result<String, EngineRunError> {
    join(&["split input and separator must be strings"])
}

pub fn wrong_input_kind_message(name: &str, expected: &str) -> Result<String, EngineRunError> {
    join(&[name, " input must be ", expected])
}

/// the slice-bound message. Unlike every other class here it carries NO operand: the reference reports the bare
/// sentence for any non-numeric, non-null bound over an array or string, whatever the offending bound was.
pub fn slice_indices_message() -> Result<String, EngineRunError> {
    join(&["Array/string slice indices must be integers"])
}

/// The engine-cardinality message: a `~generator` filter emitted two or more values where exactly one is required.
/// jqf-specific (the reference has no `~`), so the wording names the phase that over-emitted.
pub fn engine_cardinality_message(constructor: &str, phase: &str) -> Result<String, EngineRunError> {
    join(&[
        "~",
        constructor,
        " ",
        phase,
        " produced 2 or more values; exactly one is required per pull",
    ])
}

/// the update-class message: `Cannot update field at <k> index of <t>`.
///
/// Distinct from [`index_message`]: a pair can be READABLE and not writable — `[1,2,3] | setpath([[0]];9)` reads the
/// array with an array component (the index search) and only then fails to write it.
pub fn update_field_message(component: ValueKind, container: ValueKind) -> Result<String, EngineRunError> {
    join(&[
        "Cannot update field at ",
        kind_name(component),
        " index of ",
        kind_name(container),
    ])
}

/// the per-element path rejection: `Path must be specified as array, not <k>`.
pub fn not_an_array_path_message(kind: ValueKind) -> Result<String, EngineRunError> {
    join(&["Path must be specified as array, not ", kind_name(kind)])
}

/// the memberless-container delete rejection: `Cannot delete fields from <k>`.
pub fn cannot_delete_from_message(kind: ValueKind) -> Result<String, EngineRunError> {
    join(&["Cannot delete fields from ", kind_name(kind)])
}

/// the wrong-component delete rejection: `Cannot delete <kind><suffix>`, where the suffix names the container's member
/// vocabulary (` field of object`, ` element of array`).
pub fn cannot_delete_message(component: ValueKind, suffix: &str) -> Result<String, EngineRunError> {
    join(&["Cannot delete ", kind_name(component), suffix])
}

/// the path-mode emission rejection: `Invalid path expression with result <v>`.
///
/// The value is already bounded by [`dump_trunc_owned`] — it is truncated under the same 30-byte law every other
/// operand takes.
pub fn invalid_path_result_message(value: &str) -> Result<String, EngineRunError> {
    join(&["Invalid path expression with result ", value])
}

/// the path-mode navigation rejection:
/// `Invalid path expression near attempt to access element <k> of <v>`.
pub fn invalid_path_access_message(key: &str, container: &str) -> Result<String, EngineRunError> {
    join(&[
        "Invalid path expression near attempt to access element ",
        key,
        " of ",
        container,
    ])
}

/// the path-mode ITERATION rejection:
/// `Invalid path expression near attempt to iterate through <v>`.
///
/// `.[]` names no component, so it takes its own sentence rather than [`invalid_path_access_message`]'s.
pub fn invalid_path_iterate_message(container: &str) -> Result<String, EngineRunError> {
    join(&["Invalid path expression near attempt to iterate through ", container])
}

/// the object-key message: `Cannot use <kind> (<operand>) as object key`.
pub fn object_key_message(kind: ValueKind, operand: &str) -> Result<String, EngineRunError> {
    join(&["Cannot use ", kind_name(kind), " (", operand, ") as object key"])
}

/// the arithmetic message family (`<l> (lj) and <r> (rj) cannot be <verb>` and the two divide-by-zero forms). The zero
/// forms are always number/number.
pub fn arith_message(failure: super::ArithFailure, left: &str, right: &str) -> Result<String, EngineRunError> {
    use super::{ArithFailure, ArithMismatchOp};
    match failure {
        ArithFailure::TypeMismatch {
            op,
            left: lkind,
            right: rkind,
        } => {
            let verb = match op {
                ArithMismatchOp::Add => "added",
                ArithMismatchOp::Subtract => "subtracted",
                ArithMismatchOp::Multiply => "multiplied",
                ArithMismatchOp::Divide => "divided",
                ArithMismatchOp::Modulo => "divided (remainder)",
            };
            join(&[
                kind_name(lkind),
                " (",
                left,
                ") and ",
                kind_name(rkind),
                " (",
                right,
                ") cannot be ",
                verb,
            ])
        }
        ArithFailure::DivideByZero => join(&[
            "number (",
            left,
            ") and number (",
            right,
            ") cannot be divided because the divisor is zero",
        ]),
        ArithFailure::RemainderByZero => join(&[
            "number (",
            left,
            ") and number (",
            right,
            ") cannot be divided (remainder) because the divisor is zero",
        ]),
        // jqf-only reserved class (no reference analog): best-effort, not corpus-pinned.
        ArithFailure::NumericRange => join(&["number (", left, ") is out of range"]),
    }
}

#[cfg(test)]
mod tests {
    use super::dump_trunc_owned;
    use alloc::string::String;
    use jqf_data::{Number, Value};

    fn dumped(spelling: &str) -> String {
        dump_trunc_owned(&Value::Number(Number::try_json_literal(spelling).expect("literal"))).expect("dump")
    }

    /// The MESSAGE WRITER, which renders the operand spelling inside `number (…) and string (…) cannot be added`.
    ///
    /// It reads the integer through the same borrowed retained spelling the JSON encoder writes, so the storage arm
    /// must be invisible to it. The rows cross the arm boundary at the `i64` extremes and one step past.
    #[test]
    fn the_message_writer_is_blind_to_the_integer_storage_arm() {
        for spelling in [
            "0",
            "1",
            "-1",
            "10",
            "1000",
            "9007199254740992",
            "9007199254740993",
            "999999999999999999",
            "1000000000000000000",
            "9223372036854775807",
            "-9223372036854775808",
            "9223372036854775808",
            "-9223372036854775809",
            "99999999999999999999",
            "12345678901234567890123456789",
        ] {
            assert_eq!(dumped(spelling), spelling, "message spelling {spelling}");
        }
    }

    /// A retained `-0` integer keeps its own bytes in a message: it is the one value the machine arm refuses, precisely
    /// so those bytes survive.
    #[test]
    fn the_message_writer_keeps_a_retained_negative_zero() {
        let value = Value::Number(Number::integer(
            jqf_data::Integer::from_canonical(String::from("-0")).expect("canonical"),
        ));
        assert_eq!(dump_trunc_owned(&value).expect("dump"), "-0");
    }

    /// Extended kinds render their JSON projection in operand text, not `null` or `""`.
    #[test]
    fn dump_trunc_owned_renders_extended_kinds_as_their_json_projection() {
        use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
        static CONTROL: ContinueControl = ContinueControl;
        let limits = ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
        let account = RequestAccount::try_new(limits).expect("account");
        let work = WorkMeter::try_new_v1(1).expect("work");
        let _resources = ResourceContext::new(account, &CONTROL, work).expect("ledger");
        let bytes = Value::try_bytes(b"abc").expect("bytes");
        assert_eq!(dump_trunc_owned(&bytes).expect("dump"), "\"YWJj\"");
        let date = Value::LocalDate(jqf_data::LocalDate::new(2024, 1, 2).expect("date"));
        assert_eq!(dump_trunc_owned(&date).expect("dump"), "\"2024-01-02\"");
    }
}
