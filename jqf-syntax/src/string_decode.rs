//! Shared string-literal validation and allocation-free decoding.
//!
//! Also owns [`SyntaxViewError`] and the checked span view every decode starts from: a span is borrowed from bound
//! source text only after it proves it belongs to that source.
//!
//! Owns the fixed one-character escape authority ([`decode_escape`]) and the lone-low-surrogate-decodes-to-U+FFFD law.
//! Validation runs before decoding: [`literal_errors`] scans the whole segment and `active_interpolation` scans it for
//! a live interpolation marker, and only then does `DecodedStringChunks` walk it. The walk trusts neither scan for its
//! own safety — an escape it cannot decode ends the walk with an error. [`decode_literal_into`] sizes the whole walk
//! and reserves output capacity before appending, so any failure leaves the output unchanged.

use alloc::string::String;
use core::fmt;

use jqf_source::{SourceRef, Span};

use crate::{SyntaxErrorKind, SyntaxSource};

/// Failure to derive a typed view from checked syntax source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyntaxViewError {
    /// The requested source identity differs from the checked source.
    SourceMismatch,
    /// The requested span is outside the checked source or not UTF-8 aligned.
    InvalidSpan,
}

impl fmt::Display for SyntaxViewError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::SourceMismatch => "view source does not match checked syntax source",
            Self::InvalidSpan => "view span is outside checked syntax source",
        })
    }
}

impl core::error::Error for SyntaxViewError {}

impl<'source> SyntaxSource<'source> {
    /// Borrows the checked source text under `span`, after proving `span` belongs to this source.
    fn view_text(&self, source: SourceRef, span: Span) -> Result<&'source str, SyntaxViewError> {
        if source != self.source_ref() {
            return Err(SyntaxViewError::SourceMismatch);
        }
        self.text().get(span.range()).ok_or(SyntaxViewError::InvalidSpan)
    }
}

/// One decoded portion of a literal string segment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DecodedStringChunk<'source> {
    /// Source text requiring no decoding.
    Borrowed(&'source str),
    /// One decoded escape or Unicode scalar.
    Scalar(char),
}

/// Allocation-free iterator over decoded literal chunks.
///
/// [`decode_literal_chunks`] is the only constructor and validates the whole segment first, so no escape this walks
/// should be undecodable. The walk is still total: an escape it cannot decode ends the iteration with an error rather
/// than aborting the process. That matters because the validation is two scans, not one — [`literal_errors`]
/// deliberately ACCEPTS `\(` (it validates interpolated literals too), and only `active_interpolation` keeps an
/// interpolation marker away from this walk.
#[derive(Clone, Debug)]
struct DecodedStringChunks<'source> {
    text: &'source str,
    /// Source offset of `text`, for reporting an undecodable escape in place.
    base: usize,
    cursor: usize,
}

impl<'source> Iterator for DecodedStringChunks<'source> {
    type Item = Result<DecodedStringChunk<'source>, StringDecodeError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.cursor >= self.text.len() {
            return None;
        }
        let bytes = self.text.as_bytes();
        if bytes[self.cursor] != b'\\' {
            let start = self.cursor;
            self.cursor += 1;
            while self.cursor < bytes.len() && bytes[self.cursor] != b'\\' {
                self.cursor += 1;
            }
            return Some(Ok(DecodedStringChunk::Borrowed(&self.text[start..self.cursor])));
        }
        let start = self.cursor;
        let Some(marker) = bytes.get(start + 1).copied() else {
            return Some(self.refuse(start, 1));
        };
        let scalar = if marker == b'u' {
            match self.decode_unicode_escape(bytes, start) {
                Ok(scalar) => scalar,
                Err(error) => {
                    self.cursor = self.text.len();
                    return Some(Err(error));
                }
            }
        } else {
            // `\(` reaches here only if an interpolation marker escaped the active-interpolation scan: the escape
            // authority cannot decode it, so the walk refuses instead of guessing.
            match decode_escape(marker) {
                Some(scalar) => scalar,
                None => return Some(self.refuse(start, 2)),
            }
        };
        self.cursor += 2 + usize::from(marker == b'u') * 4;
        Some(Ok(DecodedStringChunk::Scalar(scalar)))
    }
}

impl<'source> DecodedStringChunks<'source> {
    /// Decodes the `\uXXXX` escape at `start`, consuming the trailing low surrogate when the escape opens a pair.
    ///
    /// A lone LOW surrogate decodes to U+FFFD; a lone HIGH surrogate is the one surrogate shape with no decoding, and
    /// is refused here.
    fn decode_unicode_escape(&mut self, bytes: &[u8], start: usize) -> Result<char, StringDecodeError> {
        let Some(first) = unicode_escape(bytes, start) else {
            return Err(self.invalid_escape(start, 2));
        };
        if (0xDC00..=0xDFFF).contains(&first) {
            return Ok('\u{FFFD}');
        }
        if !(0xD800..=0xDBFF).contains(&first) {
            return char::from_u32(u32::from(first)).ok_or_else(|| self.invalid_escape(start, 6));
        }
        let Some(second) = unicode_escape(bytes, start + 6).filter(|low| (0xDC00..=0xDFFF).contains(low)) else {
            return Err(self.invalid_escape(start, 6));
        };
        // The inner advance covers the second `\uXXXX`; the caller's advance covers the first, so a pair moves 12
        // bytes.
        self.cursor += 6;
        char::from_u32(0x1_0000 + ((u32::from(first - 0xD800) << 10) | u32::from(second - 0xDC00)))
            .ok_or_else(|| self.invalid_escape(start, 12))
    }

    /// Ends the walk at the undecodable escape starting at `start`.
    fn refuse(&mut self, start: usize, len: usize) -> Result<DecodedStringChunk<'source>, StringDecodeError> {
        let error = self.invalid_escape(start, len);
        self.cursor = self.text.len();
        Err(error)
    }

    /// The source span of the `len`-byte escape at `start`. Out-of-range offsets fall back the way
    /// [`LiteralErrors::span`] does, under the same law.
    fn invalid_escape(&self, start: usize, len: usize) -> StringDecodeError {
        StringDecodeError::InvalidEscape(
            Span::try_from_usize(self.base + start, self.base + start + len)
                .unwrap_or_else(|_| Span::new(u32::MAX, u32::MAX)),
        )
    }
}

/// The fixed one-character escape set: marker to decoded scalar.
///
/// `(` (interpolation) and `u` (Unicode escape) are handled separately by both validation and decoding; every other
/// marker routes through this single authority.
const fn decode_escape(marker: u8) -> Option<char> {
    match marker {
        b'"' => Some('"'),
        b'\\' => Some('\\'),
        b'/' => Some('/'),
        b'b' => Some('\u{0008}'),
        b'f' => Some('\u{000c}'),
        b'n' => Some('\n'),
        b'r' => Some('\r'),
        b't' => Some('\t'),
        _ => None,
    }
}

/// Failure to decode one checked literal segment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StringDecodeError {
    /// Source identity or span validation failed.
    View(SyntaxViewError),
    /// The literal contains an invalid escape.
    InvalidEscape(Span),
    /// The span crosses an active interpolation boundary.
    Interpolation(Span),
    /// The literal's source offsets exceed the representable span range.
    OffsetOverflow,
    /// Reserving caller-owned output capacity failed.
    AllocationFailure,
}

impl fmt::Display for StringDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::View(error) => error.fmt(formatter),
            Self::InvalidEscape(_) => formatter.write_str("invalid string escape"),
            Self::Interpolation(_) => formatter.write_str("literal decode span contains interpolation syntax"),
            Self::OffsetOverflow => formatter.write_str("literal source offsets exceed the representable span range"),
            Self::AllocationFailure => formatter.write_str("string decode allocation failed"),
        }
    }
}

impl core::error::Error for StringDecodeError {}

/// Validate and iterate decoded chunks from one literal template segment.
fn decode_literal_chunks<'source>(
    source: &SyntaxSource<'source>,
    source_ref: SourceRef,
    span: Span,
) -> Result<DecodedStringChunks<'source>, StringDecodeError> {
    let text = source.view_text(source_ref, span).map_err(StringDecodeError::View)?;
    // The literal's own span bounds every error offset the scans below can produce (`text` is exactly `span`'s slice,
    // so `span.start() + offset` never exceeds `span.end()`, itself a `u32`). The fallible path refuses cleanly rather
    // than panicking if a future caller breaks that invariant over a >4 GiB source — the CLI sets no input ceiling by
    // law.
    if u64::from(span.start()) + text.len() as u64 > u64::from(u32::MAX) {
        return Err(StringDecodeError::OffsetOverflow);
    }
    if let Some((_, error_span)) = literal_errors(text, span).next() {
        return Err(StringDecodeError::InvalidEscape(error_span));
    }
    if let Some(interpolation_span) = active_interpolation(text, span) {
        return Err(StringDecodeError::Interpolation(interpolation_span));
    }
    Ok(DecodedStringChunks {
        text,
        base: span.start() as usize,
        cursor: 0,
    })
}

/// Atomically append one decoded literal segment to caller-owned output.
///
/// # Errors
///
/// Returns an error without changing `output` when source validation, escape validation, active-interpolation checking,
/// or capacity reservation fails.
pub fn decode_literal_into(
    source: &SyntaxSource<'_>,
    source_ref: SourceRef,
    span: Span,
    output: &mut String,
) -> Result<(), StringDecodeError> {
    let chunks = decode_literal_chunks(source, source_ref, span)?;
    // The sizing pass runs the whole walk before a single byte is appended, so an undecodable escape is reported with
    // `output` untouched.
    let additional = chunks.clone().try_fold(0_usize, |total, chunk| match chunk? {
        DecodedStringChunk::Borrowed(text) => Ok(total + text.len()),
        DecodedStringChunk::Scalar(scalar) => Ok(total + scalar.len_utf8()),
    })?;
    output
        .try_reserve(additional)
        .map_err(|_| StringDecodeError::AllocationFailure)?;
    for chunk in chunks {
        match chunk? {
            DecodedStringChunk::Borrowed(text) => output.push_str(text),
            DecodedStringChunk::Scalar(scalar) => output.push(scalar),
        }
    }
    Ok(())
}

pub(crate) fn literal_errors(text: &str, span: Span) -> LiteralErrors<'_> {
    LiteralErrors {
        text,
        base: span.start() as usize,
        cursor: 0,
    }
}

pub(crate) struct LiteralErrors<'source> {
    text: &'source str,
    base: usize,
    cursor: usize,
}

impl Iterator for LiteralErrors<'_> {
    type Item = (SyntaxErrorKind, Span);

    fn next(&mut self) -> Option<Self::Item> {
        let bytes = self.text.as_bytes();
        while self.cursor < bytes.len() {
            if bytes[self.cursor] != b'\\' {
                self.cursor += 1;
                continue;
            }
            let start = self.cursor;
            let Some(marker) = bytes.get(start + 1).copied() else {
                self.cursor = bytes.len();
                return Some((SyntaxErrorKind::InvalidStringEscape, self.span(start, bytes.len())));
            };
            if marker == b'(' {
                self.cursor += 2;
                continue;
            }
            if decode_escape(marker).is_some() {
                self.cursor += 2;
                continue;
            }
            if marker != b'u' {
                self.cursor += 1 + self.text[start + 1..].chars().next().map_or(1, char::len_utf8);
                return Some((SyntaxErrorKind::InvalidStringEscape, self.span(start, self.cursor)));
            }
            let Some(first) = unicode_escape(bytes, start) else {
                self.cursor = (start + 6).min(bytes.len());
                while self.cursor < bytes.len() && !self.text.is_char_boundary(self.cursor) {
                    self.cursor += 1;
                }
                return Some((SyntaxErrorKind::InvalidStringEscape, self.span(start, self.cursor)));
            };
            if (0xD800..=0xDBFF).contains(&first) {
                let next = start + 6;
                if unicode_escape(bytes, next).is_some_and(|low| (0xDC00..=0xDFFF).contains(&low)) {
                    self.cursor += 12;
                    continue;
                }
                self.cursor += 6;
                return Some((SyntaxErrorKind::InvalidUnicodeEscape, self.span(start, self.cursor)));
            }
            // A lone LOW surrogate (`\uDC00..=\uDFFF`) decodes to U+FFFD; only a lone HIGH surrogate (handled above) is
            // an InvalidUnicodeEscape.
            self.cursor += 6;
        }
        None
    }
}

impl LiteralErrors<'_> {
    fn span(&self, start: usize, end: usize) -> Span {
        // `base` is the literal's source offset and `start`/`end` are bounded by the literal's own text, so `base +
        // end` never exceeds the literal's `u32` span end (the `decode_literal_chunks` extent guard is the refusal for
        // a caller that breaks that invariant). The fallible builder keeps the process alive either way.
        Span::try_from_usize(self.base + start, self.base + end).unwrap_or_else(|_| Span::new(u32::MAX, u32::MAX))
    }
}

fn unicode_escape(bytes: &[u8], start: usize) -> Option<u16> {
    if bytes.get(start..start + 2)? != b"\\u" {
        return None;
    }
    let mut value = 0_u32;
    for digit in bytes.get(start + 2..start + 6)? {
        value = (value << 4) | hex_value(*digit)?;
    }
    u16::try_from(value).ok()
}

fn active_interpolation(text: &str, span: Span) -> Option<Span> {
    let bytes = text.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if bytes[cursor] != b'\\' {
            cursor += 1;
            continue;
        }
        if bytes.get(cursor + 1) == Some(&b'(') {
            // Same law as [`LiteralErrors::span`]: the offsets are bounded by the literal's own span, so the fallback
            // is unreachable; the fallible builder keeps the process alive if that invariant ever breaks on a >4 GiB
            // source.
            return Some(
                Span::try_from_usize(span.start() as usize + cursor, span.start() as usize + cursor + 2)
                    .unwrap_or_else(|_| Span::new(u32::MAX, u32::MAX)),
            );
        }
        // This scan may overshoot past a real `\(` on an invalid `\u`; unreachable in practice because `literal_errors`
        // rejects that `\u` in the same segment before decoding runs. The two scans share the same marker walk.
        cursor += match bytes.get(cursor + 1) {
            Some(b'u') => 6,
            Some(_) => 2,
            None => 1,
        };
    }
    None
}

const fn hex_value(byte: u8) -> Option<u32> {
    // `u8 as char` is the byte's Latin-1 scalar, so every non-ASCII byte lands outside the hex alphabet and `to_digit`
    // rejects it.
    (byte as char).to_digit(16)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunks(text: &str) -> DecodedStringChunks<'_> {
        DecodedStringChunks {
            text,
            base: 0,
            cursor: 0,
        }
    }

    fn decoded(text: &str) -> String {
        let mut output = String::new();
        for chunk in chunks(text) {
            match chunk.expect("fixture is a decodable literal") {
                DecodedStringChunk::Borrowed(part) => output.push_str(part),
                DecodedStringChunk::Scalar(scalar) => output.push(scalar),
            }
        }
        output
    }

    #[test]
    fn borrowed_runs_split_at_every_escape_marker() {
        let text = r"ab";
        let mut iterator = chunks(text);
        assert_eq!(iterator.next(), Some(Ok(DecodedStringChunk::Borrowed("ab"))));
        assert_eq!(iterator.next(), None);
        assert_eq!(iterator.next(), None, "fused iterator stays exhausted");
    }

    #[test]
    fn simple_escapes_decode_to_one_scalar_each() {
        let text = r"a\nb\tc";
        let mut iterator = chunks(text);
        assert_eq!(iterator.next(), Some(Ok(DecodedStringChunk::Borrowed("a"))));
        assert_eq!(iterator.next(), Some(Ok(DecodedStringChunk::Scalar('\n'))));
        assert_eq!(iterator.next(), Some(Ok(DecodedStringChunk::Borrowed("b"))));
        assert_eq!(iterator.next(), Some(Ok(DecodedStringChunk::Scalar('\t'))));
        assert_eq!(iterator.next(), Some(Ok(DecodedStringChunk::Borrowed("c"))));
        assert_eq!(iterator.next(), None);
    }

    #[test]
    fn full_escape_surface_decodes_to_expected_scalars() {
        assert_eq!(decoded(r#""\\\/\b\f\r"#), "\"\\/\u{0008}\u{000c}\r");
    }

    #[test]
    fn bare_unicode_escape_decodes_to_one_scalar() {
        let text = r"\u03bb";
        let mut iterator = chunks(text);
        assert_eq!(iterator.next(), Some(Ok(DecodedStringChunk::Scalar('λ'))));
        assert_eq!(iterator.next(), None);
    }

    #[test]
    fn surrogate_pair_decodes_to_one_scalar() {
        let text = r"\uD83D\uDE00";
        let mut iterator = chunks(text);
        assert_eq!(iterator.next(), Some(Ok(DecodedStringChunk::Scalar('😀'))));
        assert_eq!(iterator.next(), None);
        assert_eq!(decoded(text), "😀");
    }

    #[test]
    fn adjacent_surrogate_pair_is_one_chunk_not_two_scalars() {
        let text = r"x\uD834\uDD1Ex";
        assert_eq!(decoded(text), "x𝄞x");
    }

    #[test]
    fn empty_and_plain_text_round_trip_unchanged() {
        assert_eq!(decoded(""), "");
        assert_eq!(decoded("plain text"), "plain text");
    }

    #[test]
    fn unicode_escape_parses_exactly_four_hex_digits() {
        assert_eq!(unicode_escape(b"\\u0041", 0), Some(0x41));
        assert_eq!(unicode_escape(b"\\u12G4", 0), None);
        assert_eq!(unicode_escape(b"\\u123", 0), None);
        assert_eq!(unicode_escape(b"\\u", 0), None);
        assert_eq!(unicode_escape(b"\\x0041", 0), None, "wrong marker");
    }

    #[test]
    fn hex_value_accepts_upper_and_lower_case_only() {
        assert_eq!(hex_value(b'0'), Some(0));
        assert_eq!(hex_value(b'9'), Some(9));
        assert_eq!(hex_value(b'a'), Some(10));
        assert_eq!(hex_value(b'f'), Some(15));
        assert_eq!(hex_value(b'A'), Some(10));
        assert_eq!(hex_value(b'F'), Some(15));
        assert_eq!(hex_value(b'g'), None);
        assert_eq!(hex_value(b'G'), None);
    }

    #[test]
    fn literal_errors_accepts_escapes_and_escaped_interpolation() {
        let cases = [
            r"plain",
            r#"\n\r\t\"\\"#,
            r"a\nb",
            r"literal=\\(not interpolation)",
            r"\u03bb",
            r"\uD83D\uDE00",
            r"\uDC00",
        ];
        for text in cases {
            assert_eq!(
                literal_errors(text, Span::from_usize(0, text.len())).next(),
                None,
                "{text:?}"
            );
        }
    }

    #[test]
    fn literal_errors_flags_every_invalid_escape_shape() {
        for (text, kind) in [
            (r"\q", SyntaxErrorKind::InvalidStringEscape),
            (r"trailing\", SyntaxErrorKind::InvalidStringEscape),
            (r"\u12G4", SyntaxErrorKind::InvalidStringEscape),
            (r"\u", SyntaxErrorKind::InvalidStringEscape),
            (r"\uD800", SyntaxErrorKind::InvalidUnicodeEscape),
            (r"\uD800\u0041", SyntaxErrorKind::InvalidUnicodeEscape),
            // A lone low surrogate is now valid; `\uDC00\uD800` still fails because the SECOND escape is a lone high.
            (r"\uDC00\uD800", SyntaxErrorKind::InvalidUnicodeEscape),
        ] {
            let error = literal_errors(text, Span::from_usize(0, text.len()))
                .next()
                .map(|(kind, _)| kind);
            assert_eq!(error, Some(kind), "{text:?}");
        }
    }

    #[test]
    fn lone_low_surrogate_escape_decodes_to_replacement_character() {
        // The lone-low-surrogate law: the escape decodes to U+FFFD; a lone high surrogate stays a parse error; a valid
        // pair is unchanged.
        assert_eq!(decoded(r"\uDC00"), "\u{FFFD}");
        assert_eq!(literal_errors(r"\uDC00", Span::from_usize(0, 6)).next(), None);
        assert_eq!(
            literal_errors(r"\uD800", Span::from_usize(0, 6))
                .next()
                .map(|(kind, _)| kind),
            Some(SyntaxErrorKind::InvalidUnicodeEscape)
        );
        assert_eq!(decoded(r"\uD83D\uDE00"), "😀");
    }

    /// The decode walk is total on any input, including the shapes its two validating scans are supposed to have
    /// removed first.
    ///
    /// `\(` is the load-bearing row: [`literal_errors`] ACCEPTS it, because it also validates the literal halves of an
    /// interpolated string, so the escape authority is the only thing standing between an interpolation marker and the
    /// walk. A lone HIGH surrogate is the second row — the one surrogate shape with no decoding — and a trailing
    /// lone backslash the third.
    #[test]
    fn an_escape_the_authority_cannot_decode_ends_the_walk_with_an_error() {
        for (text, start, end) in [(r"a\(b", 1, 3), (r"\uD800", 0, 6), ("trailing\\", 8, 9)] {
            let mut iterator = chunks(text);
            let last = iterator.by_ref().find(Result::is_err);
            assert_eq!(
                last,
                Some(Err(StringDecodeError::InvalidEscape(Span::from_usize(start, end)))),
                "{text:?}"
            );
            assert_eq!(iterator.next(), None, "{text:?} stops at the refusal");
        }
    }

    #[test]
    fn active_interpolation_flags_only_unescaped_introducers() {
        for text in [r"a\(b", r"\("] {
            assert!(
                active_interpolation(text, Span::from_usize(10, 10 + text.len())).is_some(),
                "{text:?}"
            );
        }

        for text in [r"a\\(b", r"a\u0028b", r"plain"] {
            assert!(
                active_interpolation(text, Span::from_usize(0, text.len())).is_none(),
                "{text:?}"
            );
        }
    }
}
