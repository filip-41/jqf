//! Physical json-seq (RFC 7464) boundary discovery and §2.4 classification.
//!
//! A json-seq record is delimited by the raw RS byte (0x1E) ALONE: a raw RS always establishes the next boundary, even
//! mid-string or mid-container, which is what makes the boundary scan a single `memchr`-free byte probe. Unlike NDJSON
//! (where LF and CR are PHYSICAL framing bytes), a json-seq possible-JSON may contain any JSON whitespace — space,
//! tab, LF, CR — because the record's physical boundary is the RS and nothing else.

use crate::byte_scan::is_json_ws;

/// Whether a significant span's FIRST byte opens a self-delimiting value.
///
/// An object, array, or string is self-delimiting per RFC 7464 §2.4: a following RS or EOF cannot be confused with
/// content. A top-level number, `true`, `false`, or `null` is NOT, which is why the possible-JSON must end in JSON
/// whitespace before the RS/EOF.
#[must_use]
const fn is_self_delimiting_first(byte: u8) -> bool {
    matches!(byte, b'{' | b'[' | b'"')
}

/// What one unit's significant content proves, before any grammar is consulted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UnitClass {
    /// The unit holds no significant bytes: an empty possible-JSON. Consecutive RS bytes coalesce as the `1*RS` prefix
    /// of the next text and never create an empty semantic document. Only the FINAL unit is ever `FinalEmpty` (a clean
    /// end): an empty unit followed by an RS coalesces, and that RS then leaves the unterminated zero-byte tail strict
    /// rejects.
    Empty,
    /// The unit's significant span starts with a self-delimiting value or is followed by at least one JSON-whitespace
    /// byte before the RS/EOF.
    Complete,
    /// The unit's significant span is a non-self-delimiting scalar with no JSON whitespace before the RS/EOF: RFC 7464
    /// §2.4's possibly-truncated top-level scalar (`<RS>true<RS>` and `<RS>123<RS>` are canaries).
    TruncatedScalar,
}

/// Classifies one framed unit `[start, end)` against the §2.4 law.
///
/// The significant span trims leading AND trailing JSON whitespace; a span whose first significant byte is not `{`,
/// `[`, or `"` is a non-self- delimiting scalar candidate, and it is truncated exactly when it reaches the unit's own
/// end with no whitespace in between.
#[must_use]
pub(crate) fn classify(unit: &[u8]) -> UnitClass {
    let mut start = 0;
    while start < unit.len() && is_json_ws(unit[start]) {
        start += 1;
    }
    let mut end = unit.len();
    while end > start && is_json_ws(unit[end - 1]) {
        end -= 1;
    }
    if start == end {
        return UnitClass::Empty;
    }
    if is_self_delimiting_first(unit[start]) {
        return UnitClass::Complete;
    }
    if end < unit.len() {
        // A non-self-delimiting scalar with at least one trailing JSON whitespace byte is complete.
        return UnitClass::Complete;
    }
    UnitClass::TruncatedScalar
}

#[cfg(test)]
mod tests {
    use super::{UnitClass, classify};

    #[test]
    fn empty_and_whitespace_units_are_empty() {
        assert_eq!(classify(b""), UnitClass::Empty);
        assert_eq!(classify(b" \t\n\r "), UnitClass::Empty);
    }

    #[test]
    fn self_delimiting_values_are_always_complete() {
        assert_eq!(classify(b"{\"a\":1}"), UnitClass::Complete);
        assert_eq!(classify(b"[1,2]"), UnitClass::Complete);
        assert_eq!(classify(b"\"str\""), UnitClass::Complete);
        assert_eq!(classify(b"{\"a\":1} "), UnitClass::Complete);
        assert_eq!(classify(b" [1] \n"), UnitClass::Complete);
    }

    #[test]
    fn scalars_need_trailing_whitespace() {
        assert_eq!(classify(b"123"), UnitClass::TruncatedScalar);
        assert_eq!(classify(b"true"), UnitClass::TruncatedScalar);
        assert_eq!(classify(b"null"), UnitClass::TruncatedScalar);
        assert_eq!(classify(b"-1"), UnitClass::TruncatedScalar);
        assert_eq!(classify(b"123 "), UnitClass::Complete);
        assert_eq!(classify(b"123\t"), UnitClass::Complete);
        assert_eq!(classify(b"123\n"), UnitClass::Complete);
        assert_eq!(classify(b"123\r"), UnitClass::Complete);
        assert_eq!(classify(b" 123  "), UnitClass::Complete);
    }

    #[test]
    fn leading_whitespace_does_not_affect_the_self_delimiting_check() {
        assert_eq!(classify(b" \n{\"a\":1} "), UnitClass::Complete);
        assert_eq!(classify(b" 123 "), UnitClass::Complete);
        assert_eq!(classify(b"\t123"), UnitClass::TruncatedScalar);
    }
}
