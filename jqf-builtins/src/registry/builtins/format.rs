//! `format/1` and the ten `@name` format filters.
//!
//! One job: own the `format/1` family record and the ten transforms `@name` selects between. `@name` is not sugar with
//! its own semantics — the reference parser rewrites it to `format("name")`, so registering the builtin gives BOTH
//! spellings from one implementation and puts the name check where the reference has it, at RUN time. That placement is
//! observable:
//!
//! ```text
//! "a" | @frobnicate        # exit 5: frobnicate is not a valid format
//! "a" | @frobnicate "x"    # exit 0: "x"
//! ```
//!
//! A template with no interpolation never applies its format, so an invalid name there is never noticed. That falls out
//! of the lowering (a hole-free template is one literal) and is not special-cased anywhere.
//!
//! Two coercion policies, and the split is the reference's: `@csv` and `@tsv` REQUIRE an array and refuse anything
//! else, while the other eight stringify first — `tostring`'s law — and transform the resulting text. `1 | @html`
//! is `"1"`, and `[1,"<"] | @html` escapes the quotes of the rendered JSON, because what is escaped is the TEXT and not
//! the value.
//!
//! The extension kinds (bytes, the four temporals) have no reference answer, so each cell/word position states its own
//! rule. In stringifying position a temporal or byte string joins at its BARE text (the same law `tostring` applies). A
//! delimited ROW accepts a bytes CELL as its base64url text but refuses a temporal cell (a row cell must be JSON-shaped
//! or already text; the bare-text rule above does not reach inside an array). A shell word refuses both: a word must be
//! a scalar or a string, and only the JSON kinds qualify.
//!
//! Three of the reference's own wordings are preserved verbatim rather than tidied: `@tsv`'s CELL refusal says "csv
//! row" (the reference's copy-paste), `@sh` says "can not" as two words, and `@base64d` stringifies BEFORE it
//! validates, so a number input is reported as the string it became. All three are byte-oracle facts.
//!
//! Negative space: it owns no number spelling and no string escaping — [`crate::semantics::render`] owns both, which
//! is why `@csv`'s `1.50` and `@json`'s escapes cannot drift from `tojson`'s.

use alloc::borrow::Cow;
use alloc::string::String;
use alloc::vec::Vec;

use jqf_data::Value;
use jqf_resource::ResourceContext;

use super::id;
use super::strings::push_char;
use super::text;
use crate::error::{EngineRunError, message};
use crate::registry::record::{
    BuiltinExample, BuiltinExecution, BuiltinFamilyId, BuiltinFamilyRecord, BuiltinOverloadId, BuiltinOverloadRecord,
    DemandTransfer, Effects, ParameterKind, SemanticRevision,
};
use crate::semantics::order;
use crate::semantics::path::raise;
use crate::semantics::render;

/// The `format/1` family record.
pub const FAMILIES: &[BuiltinFamilyRecord] = &[FORMAT_FAMILY];

/// The `format/1` overload record.
pub const OVERLOADS: &[BuiltinOverloadRecord] = &[FORMAT_OVERLOAD];

/// The format execution payloads, aligned one-to-one with [`OVERLOADS`].
///
/// Every entry carries its overload id so the const coverage walk in `registry::dispatch` can prove pairwise alignment.
#[derive(Clone, Copy, Debug)]
pub enum FormatPayload {
    /// `format/1` — one of the ten `@name` transforms, chosen at RUN time.
    Format,
}

pub const PAYLOADS: &[(u16, FormatPayload)] = &[(id::FORMAT, FormatPayload::Format)];

/// One of the ten formats, resolved from a name at RUN time.
#[derive(Clone, Copy)]
enum FormatKind {
    /// `@text` — `tostring`.
    Text,
    /// `@json` — `tojson`.
    Json,
    /// `@html` — the five-entity escape over the stringified input.
    Html,
    /// `@uri` — percent-encoding over the stringified input's UTF-8 bytes.
    Uri,
    /// `@urid` — percent-DEcoding, which requires the result to be valid UTF-8.
    UriDecode,
    /// `@csv` — a comma row of quoted cells; the input must be an array.
    Csv,
    /// `@tsv` — a tab row of escaped cells; the input must be an array.
    Tsv,
    /// `@sh` — shell words, single-quoted.
    Sh,
    /// `@base64` — standard base64 with padding.
    Base64,
    /// `@base64d` — standard base64 decoding, LOSSY on invalid UTF-8.
    Base64Decode,
}

impl FormatKind {
    /// The format table, read by name. An unknown name is `None`, which becomes the runtime refusal rather than a
    /// compile error — `@base32` and `@frobnicate` are the same failure.
    const fn named(name: &str) -> Option<Self> {
        Some(match name.as_bytes() {
            b"text" => Self::Text,
            b"json" => Self::Json,
            b"html" => Self::Html,
            b"uri" => Self::Uri,
            b"urid" => Self::UriDecode,
            b"csv" => Self::Csv,
            b"tsv" => Self::Tsv,
            b"sh" => Self::Sh,
            b"base64" => Self::Base64,
            b"base64d" => Self::Base64Decode,
            _ => return None,
        })
    }
}

/// Evaluates `format($name)` over an owned input.
///
/// # Errors
///
/// Returns the name refusal for a non-string or unknown name, the format's own domain refusal (`@csv` over a non-array,
/// `@sh` over an object, `@urid` over a malformed escape, `@base64d` over non-base64 text), or an allocation failure.
pub fn format(input: &Value, name: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let Value::String(spelling) = name.untagged() else {
        let operand = message::dump_trunc_owned(name)?;
        let text = message::invalid_format_kind_message(name.kind(), &operand)?;
        return Err(raise(&text, resources));
    };
    let Some(kind) = FormatKind::named(spelling.as_str()) else {
        let text = message::invalid_format_message(spelling)?;
        return Err(raise(&text, resources));
    };
    let formatted = match kind {
        FormatKind::Text => return text::tostring(input, resources),
        FormatKind::Json => return text::tojson(input, resources),
        FormatKind::Html => escape_html(&subject_text(input)?)?,
        FormatKind::Uri => encode_uri(&subject_text(input)?)?,
        FormatKind::UriDecode => decode_uri(&subject_text(input)?, resources)?,
        FormatKind::Csv => delimited_row(input, Delimiter::Comma, resources)?,
        FormatKind::Tsv => delimited_row(input, Delimiter::Tab, resources)?,
        FormatKind::Sh => shell_words(input, resources)?,
        FormatKind::Base64 => {
            if let Some(text) = render::bytes_text(input)? {
                text
            } else {
                use base64::Engine as _;
                base64::engine::general_purpose::STANDARD.encode(subject_text(input)?.as_bytes())
            }
        }
        FormatKind::Base64Decode => decode_base64(&subject_text(input)?, resources)?,
    };
    Value::try_string(&formatted).map_err(|_| EngineRunError::allocation_failure())
}

/// The per-format input coercion: a string IS its text, a temporal or a byte string its canonical BARE text, anything
/// else its compact JSON.
///
/// This is `tostring`'s law ([`super::text::tostring`]), borrowed rather than restated, and it is why eight of the ten
/// formats accept every kind. The two bare-text arms matter exactly where `tostring`'s are: a temporal already IS text,
/// and a byte string's text is its base64url spelling, so quoting either would transform the QUOTES and not the value.
/// (`@base64` keeps its own explicit bytes arm for history, with the same answer this reaches.) The two formats that do
/// not call this (`@csv`, `@tsv`) never stringify whole.
fn subject_text(input: &Value) -> Result<Cow<'_, str>, EngineRunError> {
    let input = input.untagged();
    if let Value::String(text) = input {
        return Ok(Cow::Borrowed(text.as_str()));
    }
    if let Some(text) = render::temporal_text(input)? {
        return Ok(Cow::Owned(text));
    }
    if let Some(text) = render::bytes_text(input)? {
        return Ok(Cow::Owned(text));
    }
    Ok(Cow::Owned(render::to_json(input)?))
}

/// `@html`: the five entities, and nothing else.
fn escape_html(subject: &str) -> Result<String, EngineRunError> {
    let mut out = String::new();
    reserve(&mut out, subject.len())?;
    for character in subject.chars() {
        match character {
            '<' => push(&mut out, "&lt;")?,
            '>' => push(&mut out, "&gt;")?,
            '&' => push(&mut out, "&amp;")?,
            '\'' => push(&mut out, "&apos;")?,
            '"' => push(&mut out, "&quot;")?,
            other => push_char(&mut out, other)?,
        }
    }
    Ok(out)
}

/// The bytes `@uri` leaves alone: the reference's unreserved set, which is narrower than RFC 3986's — `*`, `'`, `(`,
/// `)` and `!` ARE encoded, and `~` is not.
const fn is_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~')
}

/// `@uri`: percent-encode every byte outside the unreserved set, uppercase hex, one escape per UTF-8 BYTE.
fn encode_uri(subject: &str) -> Result<String, EngineRunError> {
    let mut out = String::new();
    reserve(&mut out, subject.len())?;
    for byte in subject.bytes() {
        if is_unreserved(byte) {
            push_char(&mut out, char::from(byte))?;
        } else {
            reserve(&mut out, 3)?;
            out.push('%');
            out.push(hex_digit(byte >> 4));
            out.push(hex_digit(byte & 0x0f));
        }
    }
    Ok(out)
}

/// One uppercase hex digit of a nibble.
const fn hex_digit(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'A' + (nibble - 10)) as char,
    }
}

/// The value of one hex digit, or `None` — the stdlib spelling.
fn hex_value(byte: u8) -> Option<u8> {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "a base-16 digit is at most 15, so the u32 -> u8 cast cannot truncate"
    )]
    char::from(byte).to_digit(16).map(|digit| digit as u8)
}

/// `@urid`: percent-DEcode, then require the bytes to be valid UTF-8.
///
/// Both halves can fail with the SAME message, and both name the whole input rather than the offending part: a
/// truncated escape (`"%4"`), a non-hex escape (`"%ZZ"`), and a well-formed escape whose bytes are not UTF-8 (`"%FF"`,
/// `"%C0%80"`, a surrogate, a codepoint past U+10FFFF) are one refusal class.
/// `+` is NOT decoded to a space — that is form encoding, not URI encoding.
fn decode_uri(subject: &str, resources: &ResourceContext<'_>) -> Result<String, EngineRunError> {
    let mut bytes = Vec::new();
    reserve_bytes(&mut bytes, subject.len())?;
    let raw = subject.as_bytes();
    let mut cursor = 0;
    while cursor < raw.len() {
        let byte = raw[cursor];
        if byte != b'%' {
            bytes.push(byte);
            cursor += 1;
            continue;
        }
        let decoded = raw
            .get(cursor + 1)
            .and_then(|high| hex_value(*high))
            .zip(raw.get(cursor + 2).and_then(|low| hex_value(*low)))
            .map(|(high, low)| (high << 4) | low);
        let Some(decoded) = decoded else {
            return Err(refuse_uri(subject, resources));
        };
        bytes.push(decoded);
        cursor += 3;
    }
    let Ok(text) = core::str::from_utf8(&bytes) else {
        return Err(refuse_uri(subject, resources));
    };
    let mut out = String::new();
    push(&mut out, text)?;
    Ok(out)
}

/// The one refusal both halves of `@urid` raise.
fn refuse_uri(subject: &str, resources: &ResourceContext<'_>) -> EngineRunError {
    match message::dump_trunc_text(subject).and_then(|operand| message::invalid_uri_encoding_message(&operand)) {
        Ok(text) => raise(&text, resources),
        Err(error) => error,
    }
}

/// Which delimited row is being written.
///
/// One enum rather than two functions because the LAW is one law — an array of scalars, `null` as the empty cell —
/// and only the separator and the string cell's spelling differ.
#[derive(Clone, Copy)]
enum Delimiter {
    /// `@csv`: `,` between cells, each string double-quoted with `"` doubled.
    Comma,
    /// `@tsv`: a tab between cells, each string unquoted with four escapes.
    Tab,
}

impl Delimiter {
    /// The name the NON-ARRAY refusal uses. The CELL refusal says "csv" for both (the reference's own wording) and does
    /// not read this.
    const fn name(self) -> &'static str {
        match self {
            Self::Comma => "csv",
            Self::Tab => "tsv",
        }
    }

    const fn between(self) -> char {
        match self {
            Self::Comma => ',',
            Self::Tab => '\t',
        }
    }
}

/// `@csv` / `@tsv`: the input must be an ARRAY, and every cell a scalar.
fn delimited_row(
    input: &Value,
    delimiter: Delimiter,
    resources: &ResourceContext<'_>,
) -> Result<String, EngineRunError> {
    let Value::Array(array) = input.untagged() else {
        let operand = message::dump_trunc_owned(input)?;
        let text = message::not_formattable_message(input.kind(), &operand, delimiter.name())?;
        return Err(raise(&text, resources));
    };
    let mut out = String::new();
    for (position, cell) in array.iter().enumerate() {
        if position > 0 {
            push_char(&mut out, delimiter.between())?;
        }
        push_cell(&mut out, cell, delimiter, resources)?;
    }
    Ok(out)
}

/// One cell of a delimited row.
///
/// `null` writes nothing at all — an empty cell, not the text `null` — while a number or a boolean writes its JSON.
/// Anything that is not a scalar is the refusal that names a "csv row" whichever row is being written.
///
/// A NaN number is the reference's EMPTY cell too, and this is the ONE place a NaN does not simply render as the JSON
/// `null` it prints as: `[1,nan,2] | @csv` is `"1,,2"` while `[1,nan,2] | @sh` is `"1 null 2"`. The infinities are NOT
/// special here — they write the clamped double these rows print everywhere else.
fn push_cell(
    out: &mut String,
    cell: &Value,
    delimiter: Delimiter,
    resources: &ResourceContext<'_>,
) -> Result<(), EngineRunError> {
    match cell.untagged() {
        Value::Null => Ok(()),
        Value::Number(number) if order::to_f64(number).is_nan() => Ok(()),
        Value::Bool(_) | Value::Number(_) => push(out, &render::to_json(cell)?),
        Value::String(text) => match delimiter {
            Delimiter::Comma => push_quoted_cell(out, text),
            Delimiter::Tab => push_escaped_cell(out, text),
        },
        Value::Bytes(_) => {
            let text = render::bytes_text(cell)?
                .ok_or_else(|| EngineRunError::internal_contract("a bytes cell with no base64url text"))?;
            match delimiter {
                Delimiter::Comma => push_quoted_cell(out, &text),
                Delimiter::Tab => push_escaped_cell(out, &text),
            }
        }
        other => {
            let operand = message::dump_trunc_owned(other)?;
            let text = message::invalid_row_cell_message(other.kind(), &operand)?;
            Err(raise(&text, resources))
        }
    }
}

/// A `@csv` string cell: always quoted, `"` doubled, everything else verbatim (a newline inside a cell is NOT escaped).
/// A NUL writes the two-character text `\0`, matching the reference's quoting.
fn push_quoted_cell(out: &mut String, text: &str) -> Result<(), EngineRunError> {
    push_char(out, '"')?;
    for character in text.chars() {
        match character {
            '"' => push(out, "\"\"")?,
            '\0' => push(out, "\\0")?,
            other => push_char(out, other)?,
        }
    }
    push_char(out, '"')
}

/// A `@tsv` string cell: never quoted, with the five escapes and nothing else.
fn push_escaped_cell(out: &mut String, text: &str) -> Result<(), EngineRunError> {
    for character in text.chars() {
        match character {
            '\t' => push(out, "\\t")?,
            '\n' => push(out, "\\n")?,
            '\r' => push(out, "\\r")?,
            '\\' => push(out, "\\\\")?,
            '\0' => push(out, "\\0")?,
            other => push_char(out, other)?,
        }
    }
    Ok(())
}

/// `@sh`: an ARRAY becomes space-separated words, anything else becomes one word.
fn shell_words(input: &Value, resources: &ResourceContext<'_>) -> Result<String, EngineRunError> {
    let mut out = String::new();
    match input.untagged() {
        Value::Array(array) => {
            for (position, word) in array.iter().enumerate() {
                if position > 0 {
                    push_char(&mut out, ' ')?;
                }
                push_word(&mut out, word, resources)?;
            }
        }
        other => push_word(&mut out, other, resources)?,
    }
    Ok(out)
}

/// One shell word: a string single-quoted, a scalar as its JSON, a container refused.
///
/// The refusal names the OFFENDING value, so a nested array inside an array reports the inner one — and it says "can
/// not", two words, because the reference does.
/// A NUL inside the string writes the two-character text `\0` inside the quotes, matching the reference.
fn push_word(out: &mut String, word: &Value, resources: &ResourceContext<'_>) -> Result<(), EngineRunError> {
    match word.untagged() {
        Value::Null | Value::Bool(_) | Value::Number(_) => push(out, &render::to_json(word)?),
        Value::String(text) => {
            push_char(out, '\'')?;
            for character in text.chars() {
                match character {
                    '\'' => push(out, "'\\''")?,
                    '\0' => push(out, "\\0")?,
                    other => push_char(out, other)?,
                }
            }
            push_char(out, '\'')
        }
        other => {
            let operand = message::dump_trunc_owned(other)?;
            let text = message::not_shell_escapable_message(other.kind(), &operand)?;
            Err(raise(&text, resources))
        }
    }
}

/// The value of one base64 character, or `None`.
const fn base64_value(byte: u8) -> Option<u8> {
    Some(match byte {
        b'A'..=b'Z' => byte - b'A',
        b'a'..=b'z' => byte - b'a' + 26,
        b'0'..=b'9' => byte - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => return None,
    })
}

/// `@base64d`: decode, then read the bytes as text through the reference's own sanitization.
///
/// Three of the reference's behaviours here are easy to get wrong and each is pinned by a corpus row: `=` TERMINATES
/// the scan (so `"YQ==ZZZZ"` is `"a"` and `"YQ=x"` is `"a"` too), a character outside the alphabet is `is not valid
/// base64 data` while a LENGTH that leaves six orphan bits is the different `trailing base64 byte found`, and bytes
/// that are not UTF-8 are replaced rather than refused (`"////"` decodes to three replacement characters).
fn decode_base64(subject: &str, resources: &ResourceContext<'_>) -> Result<String, EngineRunError> {
    let mut bytes = Vec::new();
    // Upper bound for ANY prefix of the subject: every full group of four characters yields three bytes and a partial
    // group at most two, so with the +2 slack no push below can grow the buffer.
    reserve_bytes(&mut bytes, subject.len() / 4 * 3 + 2)?;
    let mut packed = 0_u32;
    let mut filled = 0_u32;
    for byte in subject.bytes() {
        if byte == b'=' {
            break;
        }
        let Some(value) = base64_value(byte) else {
            return Err(refuse_base64(subject, Base64Fault::Alphabet, resources));
        };
        packed = (packed << 6) | u32::from(value);
        filled += 6;
        if filled >= 8 {
            filled -= 8;
            let octet = (packed >> filled) & 0xff;
            #[allow(
                clippy::cast_possible_truncation,
                reason = "the mask above bounds the octet to one byte"
            )]
            bytes.push(octet as u8);
        }
    }
    if filled == 6 {
        return Err(refuse_base64(subject, Base64Fault::Trailing, resources));
    }
    let mut out = String::new();
    push_sanitized(&mut out, &bytes)?;
    Ok(out)
}

/// Which of `@base64d`'s two refusals applies.
#[derive(Clone, Copy)]
enum Base64Fault {
    /// A character outside the alphabet.
    Alphabet,
    /// A length leaving six orphan bits (`n % 4 == 1`).
    Trailing,
}

/// The refusal for one base64 fault, naming the STRINGIFIED subject.
fn refuse_base64(subject: &str, fault: Base64Fault, resources: &ResourceContext<'_>) -> EngineRunError {
    let built = message::dump_trunc_text(subject).and_then(|operand| match fault {
        Base64Fault::Alphabet => message::invalid_base64_message(&operand),
        Base64Fault::Trailing => message::trailing_base64_message(&operand),
    });
    match built {
        Ok(text) => raise(&text, resources),
        Err(error) => error,
    }
}

/// Writes arbitrary bytes as text, replacing every invalid sequence with one U+FFFD — the reference's own
/// string-construction law, which is NOT Rust's.
///
/// The two disagree about HOW MANY bytes one invalid sequence eats, and the difference is visible in the output:
/// `"6WU="` decodes to `E9 65`, where the reference prints one replacement character (the truncated three-byte sequence
/// swallows the `65`) and Rust's lossy decoding prints a replacement followed by `e`.
/// They also disagree about surrogates and overlong spellings, where Rust replaces every byte and the reference
/// replaces the sequence: `"7aCA"` is one U+FFFD in the reference and three in Rust.
fn push_sanitized(out: &mut String, bytes: &[u8]) -> Result<(), EngineRunError> {
    let mut cursor = 0;
    while cursor < bytes.len() {
        let (decoded, width) = next_codepoint(&bytes[cursor..]);
        match decoded {
            Some(character) => push_char(out, character)?,
            None => push(out, "\u{fffd}")?,
        }
        cursor += width;
    }
    Ok(())
}

/// The table entry for a byte that can only continue a sequence, never start one.
const CONTINUATION: u8 = u8::MAX;

/// The UTF-8 lead-byte table: how many bytes a sequence starting here claims, [`CONTINUATION`] for a trailing byte, and
/// `0` for a byte that can start no sequence at all.
///
/// The two `0` ranges are why the reference's replacement COUNT differs from a decoder that only checks structure:
/// `0xC0`/`0xC1` could encode nothing but an overlong two-byte sequence, and `0xF5` and above nothing but a codepoint
/// past U+10FFFF, so the reference refuses them as lead bytes before it ever looks at what follows.
#[allow(
    clippy::match_same_arms,
    reason = "the two `0` arms are two different refusals that happen to share an answer — an \
              overlong-only lead and a past-U+10FFFF lead — and the table reads as the reference's table \
              only while each range keeps its own row"
)]
const fn sequence_length(byte: u8) -> u8 {
    match byte {
        0x00..=0x7f => 1,
        0x80..=0xbf => CONTINUATION,
        0xc0 | 0xc1 => 0,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        0xf5..=0xff => 0,
    }
}

/// The smallest codepoint a `length`-byte sequence may spell; anything below it is an overlong spelling, which the
/// reference replaces as ONE sequence.
const fn minimum_codepoint(length: u8) -> u32 {
    match length {
        1 => 0,
        2 => 0x80,
        3 => 0x800,
        _ => 0x1_0000,
    }
}

/// The codepoint at the front of `bytes`, or `None` for an invalid sequence, together with how many bytes were
/// consumed.
///
/// The WIDTH is the whole reason this exists rather than a call to [`core::str::from_utf8`]. Three of its rules are the
/// reference's and each is pinned by a corpus row: a TRUNCATED sequence consumes every remaining byte (`E9 65` is one
/// replacement, not two), a sequence broken by a non-continuation byte consumes only up to the break (`E0 80 41` is one
/// replacement then `A`), and a structurally sound sequence whose codepoint is a surrogate, an overlong spelling, or
/// past U+10FFFF consumes its FULL width for one replacement.
fn next_codepoint(bytes: &[u8]) -> (Option<char>, usize) {
    // The only caller slices behind a non-empty cursor (`while cursor < bytes.len()`), so an empty slice would be a
    // broken contract, not an answer.
    let Some(&first) = bytes.first() else {
        unreachable!("next_codepoint is called only with a non-empty tail")
    };
    if first < 0x80 {
        return (Some(char::from(first)), 1);
    }
    let length = sequence_length(first);
    if length == CONTINUATION || length == 0 {
        return (None, 1);
    }
    let width = usize::from(length);
    if width > bytes.len() {
        return (None, bytes.len());
    }
    let mut codepoint = u32::from(first) & ((1 << (7 - u32::from(length))) - 1);
    for (offset, &byte) in bytes[1..width].iter().enumerate() {
        if sequence_length(byte) != CONTINUATION {
            return (None, offset + 1);
        }
        codepoint = (codepoint << 6) | (u32::from(byte) & 0x3f);
    }
    if codepoint < minimum_codepoint(length) {
        return (None, width);
    }
    // `char::from_u32` is the surrogate and out-of-range check, in one.
    (char::from_u32(codepoint), width)
}

/// Grows a buffer fallibly.
fn reserve(out: &mut String, extra: usize) -> Result<(), EngineRunError> {
    out.try_reserve(extra).map_err(|_| EngineRunError::allocation_failure())
}

/// Grows a byte buffer fallibly.
fn reserve_bytes(out: &mut Vec<u8>, extra: usize) -> Result<(), EngineRunError> {
    out.try_reserve(extra).map_err(|_| EngineRunError::allocation_failure())
}

/// Appends a piece, growing fallibly.
fn push(out: &mut String, piece: &str) -> Result<(), EngineRunError> {
    reserve(out, piece.len())?;
    out.push_str(piece);
    Ok(())
}

const FORMAT_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::FORMAT),
    canonical_name: "format",
    category: "text",
    summary: "The input rendered in one of the ten named formats.",
    detail: "`@name` is this builtin with a literal name, and `@name \"…\"` \
             applies it to each interpolation hole. The name is checked at RUN \
             time, so an unknown one raises `<name> is not a valid format` \
             rather than failing to compile. `@csv` and `@tsv` require an array \
             input; the other eight stringify their input first.",
};

const FORMAT_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::FORMAT),
    family: BuiltinFamilyId::new(id::FORMAT),
    canonical_name: "format",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "@csv",
            input: "[1,\"a\",null]",
            expected: "\"1,\\\"a\\\",\"\n",
        },
        BuiltinExample {
            program: "format(\"base64\")",
            input: "\"hi\"",
            expected: "\"aGk=\"\n",
        },
    ],
};

#[cfg(test)]
mod tests {
    use super::format;
    use jqf_data::{Array, Number, Value};
    use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

    static CONTROL: ContinueControl = ContinueControl;

    fn ledger() -> ResourceContext<'static> {
        let limits = ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
        let account = RequestAccount::try_new(limits).expect("account");
        let work = WorkMeter::try_new_v1(1).expect("work");
        ResourceContext::new(account, &CONTROL, work).expect("ledger")
    }

    fn text_of(value: Value) -> alloc::string::String {
        match value {
            Value::String(text) => alloc::string::String::from(text.as_str()),
            other => panic!("expected a string, got {other:?}"),
        }
    }

    fn apply(name: &str, input: &Value) -> alloc::string::String {
        let resources = ledger();
        let name = Value::try_string(name).expect("name");
        text_of(format(input, &name, &resources).expect("format"))
    }

    /// `@base64` of bytes is the bytes' own base64url, not base64 of `null`.
    #[test]
    fn base64_of_bytes_is_the_bytes_own_projection() {
        let _resources = ledger();
        let input = Value::try_bytes(b"abc").expect("bytes");
        assert_eq!(apply("base64", &input), "YWJj");
        assert_eq!(apply("text", &input), "YWJj");
        assert_eq!(apply("json", &input), "\"YWJj\"");
    }

    /// `@csv` writes a bytes cell as its base64url projection, quoted.
    #[test]
    fn csv_renders_a_bytes_cell_as_base64url() {
        let _resources = ledger();
        let mut array = Array::try_new().expect("array");
        array
            .try_push(Value::try_bytes(b"abc").expect("bytes"))
            .expect("push bytes");
        array
            .try_push(Value::Number(Number::integer(jqf_data::Integer::from_i64(1))))
            .expect("push one");
        assert_eq!(apply("csv", &Value::Array(array)), "\"YWJj\",1");
    }

    /// A NUL inside a string writes the reference's two-character `\0` text in every string-cell formatter, not a raw
    /// U+0000.
    #[test]
    fn nul_in_a_string_cell_writes_the_reference_backslash_zero_text() {
        let _resources = ledger();
        let input = Value::try_string("a\u{0}b").expect("string");
        // @sh quotes the word and writes the two-character text \0 for NUL.
        assert_eq!(apply("sh", &input), "'a\\0b'");
        let mut array = Array::try_new().expect("array");
        array.try_push(input.clone()).expect("push");
        assert_eq!(apply("csv", &Value::Array(array.clone())), "\"a\\0b\"");
        // @tsv never quotes; only the escape appears.
        assert_eq!(apply("tsv", &Value::Array(array)), "a\\0b");
    }

    /// The stringifying formats take a temporal or byte subject at its BARE text — the same law `@text`/`tostring`
    /// applies — never the quoted JSON literal, which would transform the quotes instead of the value.
    #[test]
    fn stringifying_formats_take_extension_kinds_at_their_bare_text() {
        let _resources = ledger();
        let date = Value::LocalDate(jqf_data::LocalDate::new(2024, 1, 15).expect("date"));
        assert_eq!(apply("html", &date), "2024-01-15");
        let bytes = Value::try_bytes(b"abc").expect("bytes");
        // base64url of "abc" is "YWJj"; @uri leaves those bytes unreserved.
        assert_eq!(apply("uri", &bytes), "YWJj");
        assert_eq!(apply("base64d", &bytes), "abc");
    }
}
