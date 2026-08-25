//! The JSON string-escape scanner, ONE copy for the two readers that run it:
//! the whole-value reader ([`super::decode`]) and the streaming event parser ([`super::stream_events`]).
//!
//! One copy because the law must not drift: the simple-escape table, the surrogate-pair law, and jq's exact refusal
//! sentences all live here, and each caller maps a fault onto its own verbatim reason type. Both readers must keep
//! tracking jq's stderr text byte for byte; a wording change lands here once or it silently diverges `--stream` from
//! the `fromjson`/read family.
//!
//! The law: the six simple C0 escapes, `\uXXXX`, a HIGH surrogate followed by a `\uXXXX` low surrogate (12 bytes), a
//! LONE LOW surrogate to U+FFFD (not a fault), and any codepoint past U+10FFFF to U+FFFD.

/// Why one escape was refused. Each caller maps these onto its own sentence-carrying reason type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum EscapeFault {
    /// The string body ends on a backslash.
    AtEnd,
    /// The escaped byte is not one of the JSON escapes.
    Invalid,
    /// Fewer than four hex digits remain after `\u`.
    ShortUnicode,
    /// A hex digit is not hexadecimal.
    BadDigits,
    /// A high surrogate without a well-formed low-surrogate `\uXXXX` pair.
    SurrogatePair,
}

impl EscapeFault {
    /// jq's exact refusal sentence for this fault — the text both readers must report.
    #[must_use]
    pub(super) const fn text(self) -> &'static str {
        match self {
            Self::AtEnd => "Expected escape character at end of string",
            Self::Invalid => "Invalid escape",
            Self::ShortUnicode => "Invalid \\uXXXX escape",
            Self::BadDigits => "Invalid characters in \\uXXXX escape",
            Self::SurrogatePair => "Invalid \\uXXXX\\uXXXX surrogate pair escape",
        }
    }
}

/// The Unicode replacement character a lone low surrogate decodes to.
const REPLACEMENT: char = '\u{fffd}';

/// Decodes one escape from `tail` (which starts at the backslash), returning its character and the bytes it consumed.
pub(super) fn parse_escape(tail: &[u8]) -> Result<(char, usize), EscapeFault> {
    let Some(&marker) = tail.get(1) else {
        return Err(EscapeFault::AtEnd);
    };
    let simple = match marker {
        b'"' => Some('"'),
        b'\\' => Some('\\'),
        b'/' => Some('/'),
        b'b' => Some('\u{8}'),
        b'f' => Some('\u{c}'),
        b'n' => Some('\n'),
        b'r' => Some('\r'),
        b't' => Some('\t'),
        b'u' => None,
        _ => return Err(EscapeFault::Invalid),
    };
    if let Some(character) = simple {
        return Ok((character, 2));
    }
    unicode_escape(tail)
}

/// Decodes a `\uXXXX` escape, and the second half of a surrogate pair with it. A HIGH surrogate must be followed by a
/// `\uXXXX` low surrogate; a LONE LOW surrogate is not a fault — it decodes to the replacement character.
fn unicode_escape(tail: &[u8]) -> Result<(char, usize), EscapeFault> {
    let leading = code_unit(tail, 2)?;
    if !(0xd800..=0xdbff).contains(&leading) {
        let character = char::from_u32(leading).unwrap_or(REPLACEMENT);
        return Ok((character, 6));
    }
    if tail.get(6..8) != Some(b"\\u".as_slice()) {
        return Err(EscapeFault::SurrogatePair);
    }
    let Some(trailing) = code_unit(tail, 8).ok().filter(|unit| (0xdc00..=0xdfff).contains(unit)) else {
        return Err(EscapeFault::SurrogatePair);
    };
    let scalar = 0x1_0000 + (((leading - 0xd800) << 10) | (trailing - 0xdc00));
    let character = char::from_u32(scalar).unwrap_or(REPLACEMENT);
    Ok((character, 12))
}

/// The four hex digits of a `\u` escape at `offset` (past the backslash).
fn code_unit(tail: &[u8], offset: usize) -> Result<u32, EscapeFault> {
    let Some(digits) = tail.get(offset..offset + 4) else {
        return Err(EscapeFault::ShortUnicode);
    };
    let mut value = 0u32;
    for &digit in digits {
        // The stdlib hex decoder over one ASCII byte.
        let Some(nibble) = char::from(digit).to_digit(16) else {
            return Err(EscapeFault::BadDigits);
        };
        value = (value << 4) | nibble;
    }
    Ok(value)
}
