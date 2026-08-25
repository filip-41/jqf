//! The headered dialect's header row: its names, and the duplicate policy.
//!
//! Under `csv.rfc4180-header@1` the FIRST physical record is a header, not data. The framer consumes it (it is never
//! published and never receives an ordinal) and the payload provider reads it from the same retained source, so the two
//! halves derive one identical name vector from one identical rule rather than passing state across the record ABI.
//!
//! ## The three delegated sub-decisions
//!
//! * **Duplicate names.** An object cannot hold two members with one key, so a repeated header name is RENAMED, never
//!   dropped: the second `a` becomes `a_2`, and a literal `a_2` that then collides becomes `a_2_2`. The result is
//!   unique, order-preserving, and total (byte-identical on `a,a` and on `a,a,a_2`).
//! * **Empty names.** Kept verbatim as the `""` key, matching the portfolio's "header names retain ordered ordinal
//!   identity, including empty and duplicate names" law.
//! * **A headerless file.** Row 1 is consumed as the header REGARDLESS — the user opted into the dialect, and no
//!   content test could tell a header from data without inference this codec has forbidden itself. It is why the array
//!   dialect stays the default. The same law names the blank-first- row shape: a BLANK first row splits to ZERO header
//!   names, and every real row after it then disagrees with that zero-column header — the strict profile rejects the
//!   stream at the first data row, pointing at data whose only defect is the blank line above it. That is the
//!   documented cost of consuming row 1 without inference; a file whose first line may be blank belongs to the array
//!   dialect. The ragged-row diagnostic names this header's physical extent so the blank line is findable.
//!
//! Ragged rows are the payload provider's decision, not the header's; see [`crate::decode`].

use alloc::string::String;
use alloc::vec::Vec;

use jqf_codec_core::CodecError;

/// Reads the header row from the START of one delimited source.
///
/// Returns `Ok(None)` for an empty source — a headerless empty input is zero records, so there is no header to name.
/// The header's own physical extent is framed by the same quote-aware scanner the record stream uses (under the TSV
/// grammar a `"` is data, exactly as in the framer), so a header containing a quoted line feed is one header row
/// exactly as it would be one record.
/// Exclusive physical end of the first record — the header unit, terminator included.
///
/// A blank first row occupies `0..physical_end` (one LF is `0..1`); that extent is what the ragged-row diagnostic
/// names so the defect is findable from the data-row rejection.
#[must_use]
pub(crate) fn physical_end(bytes: &[u8], delimiter: u8, quote: Option<u8>) -> u64 {
    let end = match crate::boundary::frame_at(bytes, 0, delimiter, quote) {
        crate::boundary::Frame::Lf { lf } => lf.saturating_add(1),
        crate::boundary::Frame::CrLf { cr } => cr.saturating_add(2),
        crate::boundary::Frame::BareCr { cr } => cr.saturating_add(1),
        crate::boundary::Frame::Unterminated => bytes.len(),
    };
    end as u64
}

pub(crate) fn parse(
    bytes: &[u8],
    delimiter: u8,
    quote: Option<u8>,
    textdata: bool,
) -> Result<Option<Vec<String>>, CodecError> {
    if bytes.is_empty() {
        return Ok(None);
    }
    let payload_end = match crate::boundary::frame_at(bytes, 0, delimiter, quote) {
        crate::boundary::Frame::Lf { lf } => lf,
        // A bare CR is a framing fault the record stream reports; like CRLF the header names are read up to it, so the
        // fault — not a panic — is what surfaces.
        crate::boundary::Frame::CrLf { cr } | crate::boundary::Frame::BareCr { cr } => cr,
        crate::boundary::Frame::Unterminated => bytes.len(),
    };
    let names = crate::fields::split_fields(&bytes[..payload_end], delimiter, quote, textdata)?;
    Ok(Some(disambiguate(names)))
}

/// Applies the duplicate-name rule to one header row's raw names.
///
/// Each name that repeats an ALREADY-ASSIGNED name takes a `_N` suffix, `N` counting that assigned name's occurrences;
/// a generated name that collides in turn is suffixed again, so the result is unique, order-preserving, and total.
fn disambiguate(names: Vec<String>) -> Vec<String> {
    // The assigned names live in BOTH shapes: the map answers "have I seen this name, and how many times" in one probe
    // (a k-column header stays O(k log k), not the quadratic scan a vector find costs per column), and insertion order
    // is carried by `resolved` itself.
    let mut assigned: alloc::collections::BTreeMap<alloc::string::String, u32> = alloc::collections::BTreeMap::new();
    let mut resolved = Vec::with_capacity(names.len());
    for name in names {
        let mut candidate = name;
        loop {
            let Some(seen) = assigned.get_mut(&candidate) else {
                assigned.insert(candidate.clone(), 1);
                break;
            };
            *seen = seen.saturating_add(1);
            let suffix = *seen;
            candidate = alloc::format!("{candidate}_{suffix}");
        }
        resolved.push(candidate);
    }
    resolved
}

#[cfg(test)]
mod tests {
    use super::parse;
    use alloc::string::{String, ToString};

    const QUOTE: Option<u8> = Some(b'"');

    #[test]
    fn a_blank_first_row_occupies_its_terminator() {
        assert_eq!(super::physical_end(b"\nada,b\n", b',', QUOTE), 1);
        assert_eq!(super::physical_end(b"\r\nada,b\r\n", b',', QUOTE), 2);
        assert!(
            parse(b"\nada,b\n", b',', QUOTE, false)
                .expect("parses")
                .expect("header")
                .is_empty()
        );
    }

    #[test]
    fn an_empty_source_has_no_header() {
        assert!(parse(b"", b',', QUOTE, false).expect("parses").is_none());
    }

    #[test]
    fn the_first_row_is_the_header_whatever_it_contains() {
        // The headerless-file law: row 1 is the header because the user asked for the headered dialect; no content test
        // could tell a header from data.
        assert_eq!(
            parse(b"1,2\n3,4\n", b',', QUOTE, false)
                .expect("parses")
                .expect("header"),
            ["1".to_string(), "2".to_string()]
        );
    }

    #[test]
    fn duplicate_names_take_a_numeric_suffix() {
        assert_eq!(
            parse(b"a,a\n", b',', QUOTE, false).expect("parses").expect("header"),
            ["a".to_string(), "a_2".to_string()]
        );
        // The literal `a_2` collides with the generated one and is suffixed again.
        assert_eq!(
            parse(b"a,a,a_2\n", b',', QUOTE, false)
                .expect("parses")
                .expect("header"),
            ["a".to_string(), "a_2".to_string(), "a_2_2".to_string()]
        );
    }

    #[test]
    fn an_empty_name_is_kept_verbatim() {
        assert_eq!(
            parse(b"a,,b\n", b',', QUOTE, false).expect("parses").expect("header"),
            ["a".to_string(), String::new(), "b".to_string()]
        );
    }

    #[test]
    fn a_quoted_header_is_unquoted_and_may_span_a_line_feed() {
        assert_eq!(
            parse(b"\"a,x\",\"b\nc\"\nrow\n", b',', QUOTE, false)
                .expect("parses")
                .expect("header"),
            ["a,x".to_string(), "b\nc".to_string()]
        );
    }

    #[test]
    fn a_crlf_terminator_is_not_part_of_the_last_name() {
        assert_eq!(
            parse(b"a,b\r\n1,2\r\n", b',', QUOTE, false)
                .expect("parses")
                .expect("header"),
            ["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn a_tsv_header_splits_on_tabs_and_keeps_quotes_as_data() {
        // Header mode over TSV, where `"` is field data.
        assert_eq!(
            parse(b"a\tb\n1\t2\n", b'\t', None, false)
                .expect("parses")
                .expect("header"),
            ["a".to_string(), "b".to_string()]
        );
        assert_eq!(
            parse(b"\"x\"\ty\n", b'\t', None, false)
                .expect("parses")
                .expect("header"),
            ["\"x\"".to_string(), "y".to_string()]
        );
    }
}
