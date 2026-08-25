//! Physical RFC 4180 record boundary discovery over contiguous retained input.
//!
//! Unlike NDJSON, an RFC 4180 record may contain RAW line feeds inside a quoted field, so "first line feed at or after
//! the start" is not a boundary. This scanner walks the record with RFC 4180 quote state: a record ends at a line feed
//! ONLY when the scan is not inside a quoted field. A quote OPENS quoted state only at a field start (record start, or
//! the byte after a delimiter outside quotes); inside quotes a quote closes the field unless it is immediately doubled
//! (`""`), which is a literal quote. A lone quote mid-unquoted-field is NON-STRUCTURAL: it does not toggle state, so
//! the record frames at the next out-of-quote terminator and the payload decode rejects the malformed field.
//!
//! The scanner is a PROPOSER, not an oracle (the adjacent-value framing law). It never allocates and never validates
//! the field grammar; the per-record decode owns correctness. A record that ends at end of input is unterminated and is
//! reported as such by the session's framing law.

/// How one record unit's physical extent ends.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Frame {
    /// The record ended at a line feed at `lf`.
    Lf { lf: usize },
    /// The record ended at a carriage return immediately followed by a line feed; `cr` is the carriage return's index.
    CrLf { cr: usize },
    /// A carriage return that is NOT followed by a line feed: a framing fault at `cr`. RFC 4180 allows CRLF and LF but
    /// not a bare CR as a terminator.
    BareCr { cr: usize },
    /// No terminator before end of input: the record is unterminated.
    Unterminated,
}

use crate::byte_scan::{self, CsvFrame, NdjsonFrame, StopSet};

/// Walks one record starting at `start`; `delimiter` is the field delimiter, which is what names field starts, and
/// `quote` is the grammar axis: `Some('"')` runs RFC 4180 quote state, `None` treats `"` as ordinary field data so the
/// frame is the next LF/CRLF.
///
/// Returns the physical extent end (exclusive, terminator included) and the classification of how it ended. Quote state
/// is resolved by doubled-quote (`""`) escapes; a quote that opens a field but never closes leaves the record
/// unterminated at end of input (the decode owns the malformed-field diagnostic). A lone quote mid-unquoted-field is a
/// NON-STRUCTURAL byte: it never toggles state, so the record still ends at the next out-of-quote terminator and the
/// payload decode rejects the malformed field.
///
/// Framing scans for quote / CR / LF only. The field delimiter is ordinary payload to a record boundary, so CSV uses
/// [`CsvFrame`] (`"`, CR, LF) and TSV reuses [`NdjsonFrame`] (CR, LF — a `"` is field data). Content runs pass whole
/// through the architecture-wide scan; a CSV quote sends the walk back to the per-byte state machine — the
/// byte-identity authority for every quoted shape (the framer's quote law differs from the payload decode's: a lone
/// quote is NON-STRUCTURAL here, never an error). The two walks answer byte-identically; the module's differential test
/// proves it.
pub(crate) fn frame_at(bytes: &[u8], start: usize, delimiter: u8, quote: Option<u8>) -> Frame {
    match delimiter {
        b',' => frame_at_wide::<CsvFrame>(bytes, start, delimiter, quote),
        b'\t' => frame_at_wide::<NdjsonFrame>(bytes, start, delimiter, quote),
        _ => frame_at_scalar(bytes, start, delimiter, quote),
    }
}

/// The quote-free framing walk: stop-to-stop with the wide scanner. A stop at a line feed or carriage return frames the
/// record (with no quote seen before it, it is outside quotes by construction); under the CSV grammar a stop at a quote
/// means the record is not quote-free, and the per-byte walk re-runs from the record start (its lone-quote
/// non-structural law is the byte-identity authority for quoted shapes). TSV framing never stops on a quote, so that
/// arm is dead there.
fn frame_at_wide<S: StopSet>(bytes: &[u8], start: usize, delimiter: u8, quote: Option<u8>) -> Frame {
    if bytes.len() - start < crate::scan::WIDE_SCAN_MIN {
        return frame_at_scalar(bytes, start, delimiter, quote);
    }
    let mut index = start;
    while index < bytes.len() {
        let clean = byte_scan::prefix_len::<S>(&bytes[index..]);
        if clean == bytes.len() - index {
            break;
        }
        let pos = index + clean;
        match bytes[pos] {
            b'"' if quote.is_some() => return frame_at_scalar(bytes, start, delimiter, quote),
            b'\n' => return Frame::Lf { lf: pos },
            b'\r' => {
                if bytes.get(pos + 1) == Some(&b'\n') {
                    return Frame::CrLf { cr: pos };
                }
                return Frame::BareCr { cr: pos };
            }
            _ => index = pos + 1,
        }
    }
    Frame::Unterminated
}

/// The per-byte framing walk — every quote shape, byte for byte. Under the TSV grammar (`quote == None`) `"` is an
/// ordinary byte and quote state is never entered.
fn frame_at_scalar(bytes: &[u8], start: usize, delimiter: u8, quote: Option<u8>) -> Frame {
    let mut in_quotes = false;
    // A quote OPENS quoted state only at a field start (record start, or immediately after a delimiter outside quotes).
    // A lone quote mid-unquoted-field must not swallow the terminator the decode needs: the proposer frames at it as a
    // normal byte.
    let mut at_field_start = true;
    let mut index = start;
    while index < bytes.len() {
        match bytes[index] {
            b'"' if quote.is_none() => index += 1,
            b'"' if in_quotes => {
                // A doubled quote is a literal quote and leaves state alone; any other quote closes the field.
                if bytes.get(index + 1) == Some(&b'"') {
                    index += 2;
                } else {
                    in_quotes = false;
                    at_field_start = false;
                    index += 1;
                }
            }
            b'"' => {
                if at_field_start {
                    in_quotes = true;
                    at_field_start = false;
                    index += 1;
                } else {
                    // Non-structural byte; no toggle.
                    index += 1;
                }
            }
            b'\n' if !in_quotes => return Frame::Lf { lf: index },
            b'\r' if !in_quotes => {
                if bytes.get(index + 1) == Some(&b'\n') {
                    return Frame::CrLf { cr: index };
                }
                return Frame::BareCr { cr: index };
            }
            _ => {
                if !in_quotes {
                    at_field_start = bytes[index] == delimiter;
                }
                index += 1;
            }
        }
    }
    Frame::Unterminated
}

#[cfg(test)]
mod tests {
    use super::{Frame, frame_at};

    const QUOTE: Option<u8> = Some(b'"');

    #[test]
    fn a_line_feed_terminates_an_unquoted_record() {
        assert_eq!(frame_at(b"a,b\nc", 0, b',', QUOTE), Frame::Lf { lf: 3 });
    }

    #[test]
    fn a_carriage_return_before_the_line_feed_is_part_of_the_terminator() {
        assert_eq!(frame_at(b"a,b\r\nc", 0, b',', QUOTE), Frame::CrLf { cr: 3 });
    }

    #[test]
    fn any_other_carriage_return_is_a_framing_fault() {
        assert_eq!(frame_at(b"a,b\rc", 0, b',', QUOTE), Frame::BareCr { cr: 3 });
        assert_eq!(frame_at(b"\r", 0, b',', QUOTE), Frame::BareCr { cr: 0 });
    }

    #[test]
    fn a_line_feed_inside_a_quoted_field_does_not_terminate_the_record() {
        // "a\nb" is ONE record; the record ends at the LF after the closing quote.
        assert_eq!(frame_at(b"\"a\nb\"\nnext", 0, b',', QUOTE), Frame::Lf { lf: 5 });
    }

    #[test]
    fn a_carriage_return_inside_a_quoted_field_is_not_a_framing_fault() {
        assert_eq!(frame_at(b"\"a\rb\"\n", 0, b',', QUOTE), Frame::Lf { lf: 5 });
    }

    #[test]
    fn a_doubled_quote_is_a_literal_quote_and_does_not_toggle_state() {
        // "a""b" is one quoted field; the first LF after it terminates.
        assert_eq!(frame_at(b"\"a\"\"b\"\n", 0, b',', QUOTE), Frame::Lf { lf: 6 });
    }

    #[test]
    fn an_unterminated_quote_leaves_the_record_unterminated() {
        assert_eq!(frame_at(b"\"a\nb", 0, b',', QUOTE), Frame::Unterminated);
        assert_eq!(frame_at(b"\"unterminated", 0, b',', QUOTE), Frame::Unterminated);
    }

    #[test]
    fn no_terminator_leaves_the_record_unterminated() {
        assert_eq!(frame_at(b"a,b", 0, b',', QUOTE), Frame::Unterminated);
        assert_eq!(frame_at(b"", 0, b',', QUOTE), Frame::Unterminated);
    }

    #[test]
    fn a_lone_quote_mid_field_does_not_open_quoted_state() {
        // The lone-quote repro: `a"b\nc"d\ne\n` frames as THREE records whose first payload `a"b` the decode rejects
        // — never one merged record `a"b\nc"d` with the newline swallowed.
        assert_eq!(frame_at(b"a\"b\nc\"d\ne\n", 0, b',', QUOTE), Frame::Lf { lf: 3 });
        assert_eq!(frame_at(b"c\"d\n", 0, b',', QUOTE), Frame::Lf { lf: 3 });
        assert_eq!(frame_at(b"e\n", 0, b',', QUOTE), Frame::Lf { lf: 1 });
        // A quote after a delimiter still opens a field.
        assert_eq!(frame_at(b"x,\"a,b\"\n", 0, b',', QUOTE), Frame::Lf { lf: 7 });
        // A quote at the record start opens a field.
        assert_eq!(frame_at(b"\"a\nb\"\n", 0, b',', QUOTE), Frame::Lf { lf: 5 });
    }

    #[test]
    fn under_the_tsv_grammar_a_quote_is_field_data_and_a_lf_frames() {
        // No quote state, so `"` never swallows a terminator and the record ends at the next LF/CRLF whatever quotes it
        // contains.
        assert_eq!(frame_at(b"a\"b\nc\"d\n", 0, b'\t', None), Frame::Lf { lf: 3 });
        assert_eq!(frame_at(b"x\t\"a\tb\"\n", 0, b'\t', None), Frame::Lf { lf: 7 });
        // CRLF still frames; a bare CR stays a framing fault.
        assert_eq!(frame_at(b"a\tb\r\nc", 0, b'\t', None), Frame::CrLf { cr: 3 });
        assert_eq!(frame_at(b"a\tb\rc", 0, b'\t', None), Frame::BareCr { cr: 3 });
    }

    /// The wide framing walk is a byte-identity claim: for every delimiter the dispatch serves, every payload shape,
    /// and every start offset, the fast walk must frame exactly as the per-byte walk does — same end, same
    /// classification. The corpus is the module's own adversarial shapes (quoted records take the wide walk's
    /// fallback), plus bare CR, doubled quotes, and non-standard delimiters. The same identity is asserted under the
    /// TSV grammar, where `"` is data and there is no fallback.
    #[test]
    fn the_wide_frame_agrees_with_the_per_byte_frame() {
        let corpus: &[&[u8]] = &[
            b"",
            b"a",
            b"a,b",
            b"a,b\n",
            b"a,b\nc",
            b"a,b\r\nc",
            b"a,b\rc",
            b"\r",
            b"a\tb\tc\n",
            b"a;b;c\n",
            b"\"a\nb\"\nnext",
            b"\"a\rb\"\n",
            b"\"a\"\"b\"\n",
            b"\"a\nb",
            b"a\"b\nc\"d\ne\n",
            b"x,\"a,b\"\n",
            b"\"a,b\"",
            b"v0_0,v1_0,v2_0,v3_0\n",
        ];
        for bytes in corpus {
            for quote in [Some(b'"'), None] {
                for delimiter in [b',', b'\t', b';'] {
                    for start in 0..bytes.len().min(4) {
                        let wide = super::frame_at(bytes, start, delimiter, quote);
                        let scalar = super::frame_at_scalar(bytes, start, delimiter, quote);
                        assert_eq!(
                            wide, scalar,
                            "frame_at diverged for {bytes:?} delimiter {delimiter:?} quote {quote:?} start {start}"
                        );
                    }
                }
            }
        }
    }
}
