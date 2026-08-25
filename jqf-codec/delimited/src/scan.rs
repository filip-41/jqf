//! Allocation-free RFC 4180 field scanning for the fast-path lanes.
//!
//! The scoped (`.[1]`) lane does not need every field string materialized — it needs ONE column, and the count-only
//! walk survives beside it as the multi-step residual (the column session's `extra_steps` path), not as a separate
//! keys-shaped session. The whole-document route's `split_fields` allocates a `String` per field, which is the dominant
//! per-record cost those shapes exist to avoid. This module shares one quote-aware byte walk that validates UTF-8 and
//! either counts fields or extracts one column, allocating nothing.
//!
//! ## The walk IS the validation
//!
//! `count_and_locate` runs the FULL RFC 4180 quote-structure walk with the UTF-8 check in ONE pass; when the demanded
//! column is known up front the same call also records that column's byte range, so the scoped lane is one structural
//! pass per record. A negative index still needs the count first and keeps the second locate walk. `field_bytes` no
//! longer re-validates: its caller has already run `count_and_locate`, and its walk keeps the same state machine, so a
//! standalone call stays safe while the record lane pays one validation instead of two.

use crate::byte_scan::{self, Csv, CsvTab, StopSet};
use jqf_codec_core::{CodecError, CodecFailureKind};

/// The minimum payload length at which a wide stop-set walk earns its call overhead. Below this the per-byte walk is at
/// least as fast (the wide machinery's lane check is pure overhead on a payload that cannot fill a lane), so a short
/// record — a real and common shape — keeps the scalar walk. Records at or above the threshold have room for a
/// 16-byte lane and the wide walk wins; both walks answer byte-identically either way, and the module's differential
/// test proves it.
pub(crate) const WIDE_SCAN_MIN: usize = 32;

/// One record's field count plus, when `column` was named, that column's raw byte range (quotes included). `range` is
/// `None` when the column is out of range or was not requested.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CountLocate {
    pub count: usize,
    pub range: Option<(usize, usize)>,
}

/// The RFC 4180 quote law every walk in this module enforces, stated once. A quote opens quoted state only at a field
/// start (payload start, or immediately after a delimiter outside quotes); inside quotes a doubled quote (`""`) is a
/// literal quote and any other quote closes. A lone quote in a non-empty unquoted field is `InvalidInput`. A field
/// contains nothing after its closing quote — `"a"b` is malformed, never `ab`.
///
/// A bare CR/LF outside quotes is `InvalidInput` (the framer already stripped terminators, so any CR/LF here is inside
/// a quoted field and the whole-document split rejects it identically). An unterminated quote is `InvalidInput`.
/// Invalid UTF-8 is `InvalidInput` — the split validates per field, and the fast lanes must reject identically
/// (validate-everything- first).
///
/// Counts the fields of one record, allocating nothing, validating the whole payload to the quote law AND UTF-8 in this
/// same single walk. An empty payload is zero fields; otherwise the final field counts (a trailing delimiter
/// contributes an empty final field, matching `split_fields`: `a,` is two fields). This is the fast lanes' ONE
/// validation point: the full structural walk with the whole-payload UTF-8 check, then the count — two passes total
/// per record, the same shape as the whole route's split. `field_bytes` (the column locate) deliberately does not
/// re-validate.
pub(crate) fn count_and_locate(
    payload: &[u8],
    delimiter: u8,
    quote: Option<u8>,
    column: Option<usize>,
) -> Result<CountLocate, CodecError> {
    if payload.is_empty() {
        return Ok(CountLocate { count: 0, range: None });
    }
    // The wide walk serves the two stop-set delimiters (`Csv` for `,`, `CsvTab` for `\t`); every other legal delimiter
    // keeps the per-byte walk. A walk using the wrong set would stop at the wrong byte and silently mis-split, so the
    // dispatch is exactly these two arms. Both walks answer byte-identically (the wide one falls back to the scalar at
    // the first quote under the CSV grammar; under the TSV grammar `"` is data and there is no fallback), and the
    // module's differential test proves it.
    match delimiter {
        b',' => count_fields_wide::<Csv>(payload, delimiter, quote, column),
        b'\t' => count_fields_wide::<CsvTab>(payload, delimiter, quote, column),
        _ => count_fields_scalar(payload, delimiter, quote, column),
    }
}

/// The quote-free fast count: walk stop-to-stop with the wide scanner. A stop at the delimiter counts a field; a stop
/// at CR/LF is the bare newline rejection (with no quote seen before it, it is outside quotes by construction); under
/// the CSV grammar a stop at a quote means the record is not quote-free, and the per-byte walk — the byte-identity
/// authority for every quoted shape — re-runs from the start. Under the TSV grammar a `"` is data and the walk
/// continues. The whole-payload UTF-8 check is the scalar walk's own, so the two paths reject invalid UTF-8
/// identically.
fn count_fields_wide<S: StopSet>(
    payload: &[u8],
    delimiter: u8,
    quote: Option<u8>,
    column: Option<usize>,
) -> Result<CountLocate, CodecError> {
    if payload.len() < WIDE_SCAN_MIN {
        return count_fields_scalar(payload, delimiter, quote, column);
    }
    let mut index = 0;
    let mut count = 0usize;
    let mut field_start = 0usize;
    let mut range = None;
    while index < payload.len() {
        let clean = byte_scan::prefix_len::<S>(&payload[index..]);
        if clean == payload.len() - index {
            break;
        }
        let pos = index + clean;
        match payload[pos] {
            b'"' if quote.is_some() => {
                return count_fields_scalar(payload, delimiter, quote, column);
            }
            b'"' => index = pos + 1,
            b'\n' | b'\r' => return Err(CodecError::new(CodecFailureKind::InvalidInput)),
            _ => {
                if column == Some(count) {
                    range = Some((field_start, pos));
                }
                count += 1;
                index = pos + 1;
                field_start = index;
            }
        }
    }
    if core::str::from_utf8(payload).is_err() {
        return Err(CodecError::new(CodecFailureKind::InvalidInput));
    }
    if column == Some(count) {
        range = Some((field_start, payload.len()));
    }
    Ok(CountLocate {
        count: count + 1,
        range,
    })
}

/// The per-byte count walk — every quote shape, byte for byte. The wide walk re-enters it at the first quote under
/// the CSV grammar, and the scoped lane's `count_and_locate` is the fast lanes' ONE validation point, so this walk and
/// the scalar split must reject exactly the same payloads. Under the TSV grammar (`quote == None`) `"` is an ordinary
/// byte.
fn count_fields_scalar(
    payload: &[u8],
    delimiter: u8,
    quote: Option<u8>,
    column: Option<usize>,
) -> Result<CountLocate, CodecError> {
    let mut in_quotes = false;
    let mut at_field_start = true;
    let mut after_close_quote = false;
    let mut index = 0;
    let mut count = 0usize;
    let mut field_start = 0usize;
    let mut range = None;
    while index < payload.len() {
        match payload[index] {
            b'"' if quote.is_none() => {
                at_field_start = false;
                index += 1;
            }
            b'"' if in_quotes => {
                if payload.get(index + 1) == Some(&b'"') {
                    index += 2;
                } else {
                    in_quotes = false;
                    at_field_start = false;
                    after_close_quote = true;
                    index += 1;
                }
            }
            b'"' => {
                if at_field_start {
                    in_quotes = true;
                    at_field_start = false;
                    index += 1;
                } else {
                    return Err(CodecError::new(CodecFailureKind::InvalidInput));
                }
            }
            b'\n' | b'\r' if !in_quotes => {
                return Err(CodecError::new(CodecFailureKind::InvalidInput));
            }
            b if b == delimiter && !in_quotes => {
                if column == Some(count) {
                    range = Some((field_start, index));
                }
                count += 1;
                at_field_start = true;
                after_close_quote = false;
                index += 1;
                field_start = index;
            }
            _ => {
                if after_close_quote {
                    // The quote law at [`count_and_locate`].
                    return Err(CodecError::new(CodecFailureKind::InvalidInput));
                }
                at_field_start = false;
                index += 1;
            }
        }
    }
    if in_quotes {
        return Err(CodecError::new(CodecFailureKind::InvalidInput));
    }
    if core::str::from_utf8(payload).is_err() {
        return Err(CodecError::new(CodecFailureKind::InvalidInput));
    }
    if column == Some(count) {
        range = Some((field_start, payload.len()));
    }
    Ok(CountLocate {
        count: count + 1,
        range,
    })
}

/// Extracts one column's byte range of one record, allocating nothing.
///
/// Returns `Ok(Some((start, end)))` when the column exists, `Ok(None)` when the index is out of range, and `Err` on
/// malformed input (the same rejects the whole-document split raises). The range is over the RAW payload bytes (quotes
/// included); the caller decodes the field.
///
/// The caller MUST have validated the payload first (the single-column session does, through [`count_and_locate`]) —
/// the whole-payload UTF-8 check is the validation walk's, not this locate's. The walk below still carries the full
/// quote-state machine (`after_close_quote`, bare CR/LF, and the unterminated-quote tail included), so a standalone
/// call rejects a structurally malformed payload whenever the malformed region governs the walk's path — the one
/// deference to its caller is that a column found before a LATER malformed region answers from the bytes it located,
/// exactly like the UTF-8 check it already defers.
pub(crate) fn field_bytes(
    payload: &[u8],
    delimiter: u8,
    column: usize,
    quote: Option<u8>,
) -> Result<Option<(usize, usize)>, CodecError> {
    match delimiter {
        b',' => field_bytes_wide::<Csv>(payload, delimiter, column, quote),
        b'\t' => field_bytes_wide::<CsvTab>(payload, delimiter, column, quote),
        _ => field_bytes_scalar(payload, delimiter, column, quote),
    }
}

/// The quote-free column locate: walk stop-to-stop with the wide scanner until the `column`-th delimiter, exactly as
/// the scalar walk counts fields. Under the CSV grammar a quote sends the walk back to the per-byte locate — the
/// caller has already validated the payload through [`count_and_locate`], so the fallback re-walk is the byte-identity
/// authority for quoted shapes; under the TSV grammar `"` is data and the walk continues.
fn field_bytes_wide<S: StopSet>(
    payload: &[u8],
    delimiter: u8,
    column: usize,
    quote: Option<u8>,
) -> Result<Option<(usize, usize)>, CodecError> {
    if payload.len() < WIDE_SCAN_MIN {
        return field_bytes_scalar(payload, delimiter, column, quote);
    }
    let mut index = 0;
    let mut count = 0usize;
    let mut field_start = 0usize;
    while index < payload.len() {
        let clean = byte_scan::prefix_len::<S>(&payload[index..]);
        if clean == payload.len() - index {
            break;
        }
        let pos = index + clean;
        match payload[pos] {
            b'"' if quote.is_some() => {
                return field_bytes_scalar(payload, delimiter, column, quote);
            }
            b'"' => index = pos + 1,
            b'\n' | b'\r' => return Err(CodecError::new(CodecFailureKind::InvalidInput)),
            _ => {
                if count == column {
                    return Ok(Some((field_start, pos)));
                }
                count += 1;
                index = pos + 1;
                field_start = index;
            }
        }
    }
    if count == column {
        Ok(Some((field_start, payload.len())))
    } else {
        Ok(None)
    }
}

/// The per-byte column locate — every quote shape, byte for byte. Under the TSV grammar (`quote == None`) `"` is an
/// ordinary byte.
fn field_bytes_scalar(
    payload: &[u8],
    delimiter: u8,
    column: usize,
    quote: Option<u8>,
) -> Result<Option<(usize, usize)>, CodecError> {
    let mut in_quotes = false;
    // The caller's `count_and_locate` already rejected a lone mid-field quote, so this walk only sees legal quote
    // transitions; the field-start bookkeeping mirrors the decoders so the quote law stays one law across every site.
    let mut at_field_start = true;
    let mut after_close_quote = false;
    let mut index = 0;
    let mut count = 0usize;
    let mut field_start = 0usize;
    while index < payload.len() {
        match payload[index] {
            b'"' if quote.is_none() => {
                at_field_start = false;
                index += 1;
            }
            b'"' if in_quotes => {
                if payload.get(index + 1) == Some(&b'"') {
                    index += 2;
                } else {
                    in_quotes = false;
                    at_field_start = false;
                    after_close_quote = true;
                    index += 1;
                }
            }
            b'"' => {
                if at_field_start {
                    in_quotes = true;
                    at_field_start = false;
                    index += 1;
                } else {
                    // Unreachable: `count_and_locate` already rejected the lone quote; retained for standalone safety.
                    return Err(CodecError::new(CodecFailureKind::InvalidInput));
                }
            }
            b'\n' | b'\r' if !in_quotes => {
                return Err(CodecError::new(CodecFailureKind::InvalidInput));
            }
            b if b == delimiter && !in_quotes => {
                if count == column {
                    return Ok(Some((field_start, index)));
                }
                count += 1;
                index += 1;
                field_start = index;
                at_field_start = true;
                after_close_quote = false;
            }
            _ => {
                if after_close_quote {
                    // The quote law at [`count_and_locate`].
                    return Err(CodecError::new(CodecFailureKind::InvalidInput));
                }
                at_field_start = false;
                index += 1;
            }
        }
    }
    if in_quotes {
        // The validation walk's reject, retained here so a standalone call keeps the structural law this module's doc
        // claims: an unterminated quote never yields a column range.
        return Err(CodecError::new(CodecFailureKind::InvalidInput));
    }
    if count == column {
        Ok(Some((field_start, payload.len())))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::count_and_locate;

    const QUOTE: Option<u8> = Some(b'"');

    /// The tests' count-only shape of [`count_and_locate`], standing in for the deleted production wrapper.
    fn count_fields(payload: &[u8], delimiter: u8, quote: Option<u8>) -> Result<usize, jqf_codec_core::CodecError> {
        count_and_locate(payload, delimiter, quote, None).map(|walk| walk.count)
    }

    #[test]
    fn an_unterminated_quote_is_invalid_in_the_locate_walk_too() {
        // The locate walk's doc claims standalone structural rejection; an unterminated quote is the one shape its twin
        // rejected at the tail. Production callers validate first, so this is unreachable there — the claim is what
        // the test keeps honest. A column located BEFORE a later malformed region still answers (the walk's one
        // deference to its caller), so these shapes are ones whose malformed region the walk actually walks through.
        for (payload, column) in [(&b"\"a"[..], 0), (&b"a,\"b"[..], 1), (&b"\"a\"\"b"[..], 0)] {
            assert!(
                super::field_bytes(payload, b',', column, QUOTE).is_err(),
                "unterminated quote {payload:?} must be structurally rejected"
            );
        }
        // The legal shape still locates: a closed quote is fine.
        let range = super::field_bytes(b"\"a\",b", b',', 0, QUOTE)
            .expect("scans")
            .expect("column exists");
        assert_eq!(range, (0, 3));
    }

    #[test]
    fn content_after_a_closing_quote_is_invalid() {
        // The quote law at [`count_and_locate`]: `"a"b` is the load-bearing row. Sibling:
        // `fields::content_after_a_closing_quote_is_invalid`.
        assert!(count_fields(b"\"a\"b", b',', QUOTE).is_err());
        assert!(count_fields(b"x,\"a\"b", b',', QUOTE).is_err());
        assert!(count_fields(b"\"a\"b,x", b',', QUOTE).is_err());
        // The legal shapes still count: a delimiter after the quote starts a new field, the field may end right after
        // the quote, and a doubled quote is a literal quote inside the field.
        assert_eq!(count_fields(b"\"a\",\"b\"", b',', QUOTE).expect("counts"), 2);
        assert_eq!(count_fields(b"\"a\"", b',', QUOTE).expect("counts"), 1);
        assert_eq!(count_fields(b"\"a\"\"b\"", b',', QUOTE).expect("counts"), 1);
    }

    #[test]
    fn under_the_tsv_grammar_quotes_are_data_in_the_fast_lanes() {
        // The scoped lane's validation walks agree with the whole split: a quoted cell counts as one field with its
        // quotes intact.
        assert_eq!(count_fields(b"a\t\"b\"\tc", b'\t', None).expect("counts"), 3);
        assert_eq!(count_fields(b"a\"b", b'\t', None).expect("counts"), 1);
        // The column locate returns the raw field INCLUDING the quotes.
        let range = super::field_bytes(b"a\t\"b\"\tc", b'\t', 1, None)
            .expect("scans")
            .expect("column exists");
        assert_eq!(&b"a\t\"b\"\tc"[range.0..range.1], b"\"b\"");
    }

    /// The wide walks are a byte-identity claim: for every delimiter the dispatch serves and every payload shape, the
    /// fast result must equal the per-byte walk's exactly — same count, same column ranges, same rejections. The
    /// corpus is the module's own adversarial shapes: quoted fields (which take the wide walk's fallback), bare
    /// newlines, doubled quotes, lone quotes, trailing delimiters, empty fields, invalid UTF-8 — asserted under both
    /// grammars.
    #[test]
    fn the_wide_walks_agree_with_the_per_byte_walk() {
        let corpus: &[&[u8]] = &[
            // The empty payload is deliberately absent here: its zero-field guard lives at the PUBLIC entry
            // (`count_and_locate` returns 0), while the scalar walk answers 1 for it — reachable only through that
            // guard or the wide fallback, both of which see non-empty payloads.
            b"a",
            b",",
            b",,",
            b"a,",
            b",a",
            b"a,b,c",
            b"a\tb\tc",
            b"a;b;c",
            b"\"a\"",
            b"\"a\",\"b\"",
            b"\"a,b\"",
            b"x,\"a,b\"",
            b"\"he said \"\"hi\"\"\",tail",
            b"a\"b",
            b"x,a\"b",
            b"\"a\"b",
            b"\"a\n\"",
            b"\"a\rb\"",
            b"a\nb",
            b"\"a\"\"b\"",
            b"\"\"",
            b"\"\",\"x\"",
            b"v0_0,v1_0,v2_0,v3_0",
            b"caf\xc3\xa9,na\xc3\xafve",
            b"\xff",
            b"\xc3",
        ];
        // The public entry's empty guard, asserted beside the walks.
        assert_eq!(count_fields(b"", b',', QUOTE).expect("zero fields"), 0);
        assert_eq!(count_fields(b"", b'\t', QUOTE).expect("zero fields"), 0);
        assert_eq!(count_fields(b"", b'\t', None).expect("zero fields"), 0);
        for payload in corpus {
            for quote in [Some(b'"'), None] {
                for delimiter in [b',', b'\t', b';'] {
                    let count = count_fields(payload, delimiter, quote);
                    let scalar = super::count_fields_scalar(payload, delimiter, quote, None);
                    assert_eq!(
                        count.as_ref().ok().copied(),
                        scalar.as_ref().ok().map(|walk| walk.count),
                        "count_fields diverged for {payload:?} with delimiter {delimiter:?} quote {quote:?}"
                    );
                    for column in 0..=4 {
                        let wide = super::field_bytes(payload, delimiter, column, quote);
                        let narrow = super::field_bytes_scalar(payload, delimiter, column, quote);
                        assert_eq!(
                            wide.ok(),
                            narrow.ok(),
                            "field_bytes diverged for {payload:?} delimiter {delimiter:?} quote {quote:?} column {column}"
                        );
                    }
                }
            }
        }
    }
}
