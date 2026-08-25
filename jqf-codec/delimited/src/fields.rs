//! RFC 4180 field DECODING for one record payload.
//!
//! [`crate::scan`] LOCATES fields without allocating; this module turns located bytes into the field's semantic string.
//! The whole-document route, the header row, and the scoped single-column lane all decode through it, which is what
//! makes the scoped lane's byte-identity law — "identical to whole-decode-then-navigate" — TRUE rather than merely
//! documented: one unquoting rule, one UTF-8 rule, one copy. A second unquoting rule beside it is exactly how the lane
//! published `"\"a,b\""` where the floor published `"a,b"`.
//!
//! ## The TEXTDATA freeze
//!
//! `textdata` gates ONE predicate, [`is_textdata`], over the field bytes — it is never a second parser. Under the RFC
//! 4180 grammar an unquoted field may carry only `TEXTDATA = %x20-21 / %x23-2B / %x2D-7E`, and a quoted field's content
//! only TEXTDATA, comma, CR, LF, or the structural `""` escape; TAB, NUL, other C0/DEL, and every non-ASCII scalar are
//! `InvalidInput` even when their byte sequence is valid UTF-8. The UTF-8-admitting family (`csv.utf8@1`) and the TSV
//! grammar run with the gate off and are untouched.

use alloc::string::String;
use alloc::vec::Vec;

use crate::byte_scan::{self, Csv, CsvTab, StopSet};
use jqf_codec_core::{CodecError, CodecFailureKind};

/// RFC 4180's `TEXTDATA = %x20-21 / %x23-2B / %x2D-7E`: printable ASCII minus the quote (structural) and the comma (the
/// delimiter). The one field-byte predicate the RFC-named dialects enforce; TAB, NUL, other C0/DEL, and every non-ASCII
/// scalar fail it even when their bytes are valid UTF-8.
pub(crate) const fn is_textdata(byte: u8) -> bool {
    matches!(byte, 0x20..=0x21 | 0x23..=0x2B | 0x2D..=0x7E)
}

/// Whether one CONTENT byte is admitted inside a quoted field under the TEXTDATA freeze: TEXTDATA, the comma, or CR/LF
/// as quoted data (RFC 4180 §2.3 admits a line break inside a quoted field). The quote byte itself is structural —
/// opener, closer, or the `""` escape — and never reaches here.
const fn is_admitted_quoted(byte: u8) -> bool {
    is_textdata(byte) || byte == b',' || byte == b'\r' || byte == b'\n'
}

/// Splits one record's payload into its decoded field strings.
///
/// Quotes are structural under the CSV grammar (`quote` is `Some('"')`): a quote OPENS quoted state only at a field
/// start (payload start, or immediately after a delimiter outside quotes); inside quotes a doubled quote (`""`)
/// contributes one literal quote and any other quote closes. A lone quote in a non-empty unquoted field is malformed
/// (`InvalidInput`). Under the TSV grammar (`quote` is `None`) `"` is ordinary field data and the quote-free walk is
/// authoritative — it never bails to the per-byte machine. Every field must be valid UTF-8; a field with invalid
/// UTF-8 is `InvalidInput`. Field values are core `String`s, never type-inferred.
///
/// `textdata` is the RFC 4180 alphabet freeze: when it is set, an unquoted field carries only TEXTDATA and a quoted
/// field's content only TEXTDATA/comma/CR/LF, and any other byte — TAB, NUL, C0/DEL, every non-ASCII scalar — is
/// `InvalidInput` even though the payload is valid UTF-8.
///
/// An empty payload is a zero-field record (`[]`), not a single empty field. A payload that ends with the delimiter
/// still has that trailing empty field: `a,` is two fields, `,` is two empty fields, and an empty payload is none.
pub(crate) fn split_fields(
    payload: &[u8],
    delimiter: u8,
    quote: Option<u8>,
    textdata: bool,
) -> Result<Vec<String>, CodecError> {
    let mut fields = Vec::new();
    let mut scratch = Vec::new();
    let count = split_fields_into(
        payload,
        delimiter,
        quote,
        textdata,
        &mut fields,
        &mut scratch,
        None,
        None,
    )?;
    fields.truncate(count);
    Ok(fields)
}

/// Splits one record's payload into reused field string buffers.
///
/// The semantics are exactly [`split_fields`]'s, but `fields` and `scratch` are caller-owned and their capacities
/// survive the call, so a record drive that decodes many rows allocates once per field shape instead of once per field.
/// `fields` grows to the widest row seen; the caller reads `fields[..returned_count]` and may keep the buffers for the
/// next row. `scratch` is a per-field byte buffer restored after every field, so the quote/UTF-8 walk itself allocates
/// nothing once warmed. `quote` is the grammar axis: `Some('"')` runs the RFC 4180 quote law, `None` makes the
/// quote-free walk authoritative (`"` is field data, never a fallback).
///
/// The stop-set delimiters (`Csv` for `,`, `CsvTab` for `\t`) take the wide walk: content runs are copied whole through
/// the architecture-wide scan, and a quote — the only shape the wide walk cannot serve under the CSV grammar —
/// sends the walk back to the per-byte state machine, which remains the byte-identity authority for every quoted
/// payload. The two walks answer byte-identically; the module's differential test proves it.
///
/// `keep` is the headered prune mask: a `false` column still walks its bytes (quote / UTF-8 / structural law) and still
/// counts toward the ragged-row check, but writes nothing into the field slot. `None` keeps every column. The span
/// ledger stays index-aligned with the field vector whenever it is present — skipped columns still occupy their slot.
#[expect(
    clippy::too_many_arguments,
    reason = "the split takes the payload, the three-axis grammar, and the caller's reused buffers; bundling the buffers would hide the reuse contract"
)]
pub(crate) fn split_fields_into(
    payload: &[u8],
    delimiter: u8,
    quote: Option<u8>,
    textdata: bool,
    fields: &mut Vec<String>,
    scratch: &mut Vec<u8>,
    spans: Option<&mut Vec<jqf_source::Span>>,
    keep: Option<&[bool]>,
) -> Result<usize, CodecError> {
    // One whole-payload UTF-8 check replaces the per-field revalidation: a delimiter and a quote are ASCII, so every
    // field boundary is a char boundary, and unquoting only removes ASCII quotes. Validate-everything- first is
    // preserved and fires earlier than today's per-field check.
    if core::str::from_utf8(payload).is_err() {
        return Err(CodecError::new(CodecFailureKind::InvalidInput));
    }
    // The TEXTDATA freeze is a CSV-grammar law: the RFC names it for comma- separated fields with quoting. Under the
    // no-quote TSV grammar the gate is off by construction (the TSV constructor never sets it); this line makes the
    // walk total, so a stray flag cannot half-apply the freeze.
    let textdata = textdata && quote.is_some();
    // Scratch is only the quoted-unescape buffer (the quote-free wide walk copies field runs straight from the
    // payload). Amortized reserve so a stream of widening rows does not reallocate exactly once per row.
    scratch
        .try_reserve(payload.len())
        .map_err(jqf_resource::ResourceError::from)?;
    match delimiter {
        b',' => split_fields_wide::<Csv>(payload, delimiter, quote, textdata, fields, scratch, spans, keep),
        b'\t' => split_fields_wide::<CsvTab>(payload, delimiter, quote, textdata, fields, scratch, spans, keep),
        _ => split_fields_validated(payload, delimiter, quote, textdata, fields, scratch, spans, keep),
    }
}

/// The quote-free split: walk stop-to-stop with the wide scanner. A stop at the delimiter finishes the field from the
/// payload slice (no scratch copy); a stop at CR/LF is the bare newline rejection (with no quote seen before it, it is
/// outside quotes by construction); under the CSV grammar a stop at a quote means the record is not quote-free, and the
/// per-byte split — the byte-identity authority for every quoted shape — re-runs from the start (`fields` is
/// rebuilt by index exactly as a fresh call would). Under the TSV grammar a `"` is field data and the walk continues.
#[expect(
    clippy::too_many_arguments,
    reason = "the wide walk threads the same grammar and buffer set as the scalar walk it falls back to"
)]
fn split_fields_wide<S: StopSet>(
    payload: &[u8],
    delimiter: u8,
    quote: Option<u8>,
    textdata: bool,
    fields: &mut Vec<String>,
    scratch: &mut Vec<u8>,
    mut spans: Option<&mut Vec<jqf_source::Span>>,
    keep: Option<&[bool]>,
) -> Result<usize, CodecError> {
    if payload.len() < crate::scan::WIDE_SCAN_MIN {
        return split_fields_validated(payload, delimiter, quote, textdata, fields, scratch, spans, keep);
    }
    let mut count = 0usize;
    let mut index = 0;
    let mut field_start = 0usize;
    while index < payload.len() {
        let clean = byte_scan::prefix_len::<S>(&payload[index..]);
        if clean == payload.len() - index {
            break;
        }
        let pos = index + clean;
        match payload[pos] {
            b'"' if quote.is_some() => {
                // The payload was already validated by the entry; the fallback re-walk must not pay the whole-payload
                // UTF-8 scan a second time.
                return split_fields_validated(payload, delimiter, quote, textdata, fields, scratch, spans, keep);
            }
            b'"' => index = pos + 1,
            b'\n' | b'\r' => return Err(CodecError::new(CodecFailureKind::InvalidInput)),
            _ => {
                let span = field_span(field_start, pos)?;
                finish_clean_field(
                    payload,
                    field_start,
                    pos,
                    textdata,
                    fields,
                    count,
                    span,
                    spans.as_deref_mut(),
                    keep,
                )?;
                count += 1;
                index = pos + 1;
                field_start = index;
            }
        }
    }
    // Any non-empty payload ends with a field, exactly as the scalar tail.
    if !payload.is_empty() {
        let span = field_span(field_start, payload.len())?;
        finish_clean_field(
            payload,
            field_start,
            payload.len(),
            textdata,
            fields,
            count,
            span,
            spans,
            keep,
        )?;
        count += 1;
    }
    Ok(count)
}

/// The per-byte split — every quote shape, byte for byte — over a payload that has ALREADY been validated as UTF-8
/// by the caller (`split_fields_into` at the entry, or the wide walk's fallback). The scoped lane's field decoder
/// shares its unquoting rule. Under the TSV grammar (`quote == None`) `"` is an ordinary byte: no quote state, no
/// lone-quote rejection, and the walk is authoritative.
#[expect(
    clippy::too_many_arguments,
    reason = "the per-byte walk threads the same grammar and buffer set as the wide walk that falls back into it"
)]
fn split_fields_validated(
    payload: &[u8],
    delimiter: u8,
    quote: Option<u8>,
    textdata: bool,
    fields: &mut Vec<String>,
    scratch: &mut Vec<u8>,
    mut spans: Option<&mut Vec<jqf_source::Span>>,
    keep: Option<&[bool]>,
) -> Result<usize, CodecError> {
    // The same CSV-grammar gate normalization `split_fields_into` applies, so a standalone call agrees with the routed
    // one under every flag combination (the wide/scalar differential runs them side by side).
    let textdata = textdata && quote.is_some();
    scratch.clear();
    let mut count = 0usize;
    let mut in_quotes = false;
    let mut at_field_start = true;
    let mut after_close_quote = false;
    let mut quoted = false;
    let mut field_start = 0usize;
    let mut index = 0;
    while index < payload.len() {
        let byte = payload[index];
        match byte {
            b'"' if quote.is_none() => {
                at_field_start = false;
                index += 1;
            }
            b'"' if in_quotes => {
                if payload.get(index + 1) == Some(&b'"') {
                    scratch.push(b'"');
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
                    quoted = true;
                    at_field_start = false;
                    index += 1;
                } else {
                    // A lone quote in a non-empty unquoted field is malformed: the field never opened quoted.
                    return Err(CodecError::new(CodecFailureKind::InvalidInput));
                }
            }
            b if b == delimiter && !in_quotes => {
                let span = field_span(field_start, index)?;
                finish_scalar_field(
                    payload,
                    field_start,
                    index,
                    textdata,
                    quoted,
                    scratch,
                    fields,
                    count,
                    span,
                    spans.as_deref_mut(),
                    keep,
                )?;
                count += 1;
                at_field_start = true;
                after_close_quote = false;
                quoted = false;
                index += 1;
                field_start = index;
            }
            b'\n' | b'\r' if !in_quotes => {
                // The framer already stripped the terminator, so any CR/LF here is inside the payload and malformed.
                return Err(CodecError::new(CodecFailureKind::InvalidInput));
            }
            _ => {
                if after_close_quote {
                    // The quote law at [`crate::scan::count_and_locate`].
                    return Err(CodecError::new(CodecFailureKind::InvalidInput));
                }
                // The TEXTDATA freeze on QUOTED content: a quoted field admits TEXTDATA, comma, CR, LF, and the `""`
                // escape only. Unquoted content is checked once per field at `finish_clean_field`, so this arm gates
                // the in-quotes case.
                if textdata && in_quotes && !is_admitted_quoted(byte) {
                    return Err(CodecError::new(CodecFailureKind::InvalidInput));
                }
                if quoted {
                    scratch.push(byte);
                }
                at_field_start = false;
                index += 1;
            }
        }
    }
    if in_quotes {
        return Err(CodecError::new(CodecFailureKind::InvalidInput));
    }
    // Any non-empty payload ends with a field. `scratch` and `fields` can both be empty here: a payload of exactly `""`
    // consumed its only content closing the quote, yet counts as one empty field.
    if !payload.is_empty() {
        let span = field_span(field_start, payload.len())?;
        finish_scalar_field(
            payload,
            field_start,
            payload.len(),
            textdata,
            quoted,
            scratch,
            fields,
            count,
            span,
            spans,
            keep,
        )?;
        count += 1;
    }
    Ok(count)
}

/// Builds one field's authored span from its raw byte offsets, refusing a record larger than the compact `Span`
/// representation can address.
fn field_span(start: usize, end: usize) -> Result<jqf_source::Span, CodecError> {
    jqf_source::Span::try_from_usize(start, end).map_err(|_| CodecError::new(CodecFailureKind::InvalidInput))
}

/// Whether column `index` should materialize its decoded text. A mask shorter than the row (a ragged extra field) skips
/// the copy — the headered build will reject the row on the full count.
fn keep_column(keep: Option<&[bool]>, index: usize) -> bool {
    keep.is_none_or(|mask| mask.get(index).copied().unwrap_or(false))
}

/// The payload was validated as UTF-8 at the split entry; delimiters and quotes are ASCII so every field boundary is a
/// char boundary.
#[allow(
    unsafe_code,
    reason = "the split entry validated the whole payload as UTF-8 and field cuts land on ASCII delimiters or quotes"
)]
fn field_text(payload: &[u8], start: usize, end: usize) -> &str {
    // SAFETY: `split_fields_into` rejected a non-UTF-8 payload before any
    // walk ran, and every cut is an ASCII delimiter or the payload end.
    unsafe { core::str::from_utf8_unchecked(&payload[start..end]) }
}

#[allow(
    clippy::too_many_arguments,
    reason = "one field finish: payload window, TEXTDATA freeze, and the output/spans/keep plumbing the two walks already hold"
)]
fn finish_clean_field(
    payload: &[u8],
    start: usize,
    end: usize,
    textdata: bool,
    fields: &mut Vec<String>,
    index: usize,
    span: jqf_source::Span,
    spans: Option<&mut Vec<jqf_source::Span>>,
    keep: Option<&[bool]>,
) -> Result<(), CodecError> {
    // The TEXTDATA freeze on UNQUOTED fields, shared by the wide and per-byte walks so both reject identically. A
    // delimiter, a quote, CR, and LF can never appear here (the walks bound or reject them), so the predicate is the
    // whole remaining law: TAB, NUL, other C0/DEL, and every non-ASCII scalar fail it even as valid UTF-8.
    if textdata && !payload[start..end].iter().all(|&byte| is_textdata(byte)) {
        return Err(CodecError::new(CodecFailureKind::InvalidInput));
    }
    finish_text(field_text(payload, start, end), fields, index, span, spans, keep)
}

#[allow(
    clippy::too_many_arguments,
    reason = "one field finish: payload window, TEXTDATA freeze, quote state, and the output/spans/keep plumbing the two walks already hold"
)]
fn finish_scalar_field(
    payload: &[u8],
    start: usize,
    end: usize,
    textdata: bool,
    quoted: bool,
    scratch: &mut Vec<u8>,
    fields: &mut Vec<String>,
    index: usize,
    span: jqf_source::Span,
    spans: Option<&mut Vec<jqf_source::Span>>,
    keep: Option<&[bool]>,
) -> Result<(), CodecError> {
    let result = if quoted {
        // Unquoting only removes ASCII quotes from already-valid UTF-8. Quoted CONTENT was validated byte-by-byte
        // during the walk (the `""` escape is structural there, not a content byte).
        finish_text(field_text(scratch, 0, scratch.len()), fields, index, span, spans, keep)
    } else {
        finish_clean_field(payload, start, end, textdata, fields, index, span, spans, keep)
    };
    scratch.clear();
    result
}

/// Copies one decoded field into the reused buffer slot.
///
/// The String slot is reused by index, so the field vector's per-slot allocations happen once per row width. A pruned
/// column still occupies its slot (ragged-row law, span-ledger alignment) but writes no text. The authored span — the
/// field's RAW bytes, quotes included — rides the same walk into the caller's span ledger by index, so a re-walk (the
/// wide split's fallback to the per-byte walk at the first quote) rebuilds the ledger exactly as it rebuilds the field
/// strings.
fn finish_text(
    text: &str,
    fields: &mut Vec<String>,
    index: usize,
    span: jqf_source::Span,
    spans: Option<&mut Vec<jqf_source::Span>>,
    keep: Option<&[bool]>,
) -> Result<(), CodecError> {
    if index == fields.len() {
        // Amortized, grant-accounted growth: a refused reserve (a row wider than the caller's grant) fails the morsel
        // instead of aborting the process through the counted allocator's null return.
        fields.try_reserve(1).map_err(jqf_resource::ResourceError::from)?;
        fields.push(String::new());
    }
    if keep_column(keep, index) {
        fields[index].clear();
        // The reused String slot grows to hold this field's text; reserve the exact bound fallibly so a field larger
        // than the caller's grant fails the morsel instead of aborting through the counted allocator.
        fields[index]
            .try_reserve(text.len())
            .map_err(jqf_resource::ResourceError::from)?;
        fields[index].push_str(text);
    }
    if let Some(spans) = spans {
        if index == spans.len() {
            // Amortized, grant-accounted growth, exactly as the field slots.
            spans.try_reserve(1).map_err(jqf_resource::ResourceError::from)?;
            spans.push(span);
        } else {
            spans[index] = span;
        }
    }
    Ok(())
}

/// Decodes ONE field's raw bytes — the range [`crate::scan::field_bytes`] returns, quotes included — into the
/// identical string [`split_fields`] would have produced for that field.
///
/// This is the same unquoting walk, minus the delimiter arm the caller has already resolved. It is the scoped lane's
/// only field decoder. Under the TSV grammar (`quote == None`) the raw bytes ARE the field: copied verbatim (UTF-8
/// validated), `"` included. `textdata` is the RFC 4180 alphabet freeze, enforced on exactly the bytes this walk
/// consumes: TEXTDATA only outside quotes; TEXTDATA, comma, CR, LF, and the structural `""` escape inside.
pub(crate) fn decode_field_into(
    raw: &[u8],
    quote: Option<u8>,
    textdata: bool,
    current: &mut Vec<u8>,
) -> Result<String, CodecError> {
    current.clear();
    // One fallible reserve bounds every push below (a doubled quote consumes two raw bytes per output byte), exactly as
    // `split_fields_into` bounds its scratch: a giant field a morsel worker's grant cannot hold is the morsel's
    // terminal, never an allocator abort.
    current
        .try_reserve(raw.len())
        .map_err(jqf_resource::ResourceError::from)?;
    if quote.is_none() {
        // The quote-free grammar's authoritative walk: the field is its raw bytes, verbatim — `"` is data, there is
        // no unquoting to do. The TEXTDATA freeze never applies (TSV admits UTF-8).
        current.extend_from_slice(raw);
        return take_utf8(current);
    }
    let mut in_quotes = false;
    let mut after_close_quote = false;
    let mut index = 0;
    while index < raw.len() {
        let byte = raw[index];
        if byte == b'"' {
            if in_quotes {
                if raw.get(index + 1) == Some(&b'"') {
                    current.push(b'"');
                    index += 2;
                } else {
                    in_quotes = false;
                    after_close_quote = true;
                    index += 1;
                }
            } else if index == 0 {
                // A field is quoted only when it STARTS with a quote.
                in_quotes = true;
                index += 1;
            } else {
                // A lone quote after unquoted content is malformed.
                return Err(CodecError::new(CodecFailureKind::InvalidInput));
            }
        } else {
            if after_close_quote {
                // The quote law at [`crate::scan::count_and_locate`].
                return Err(CodecError::new(CodecFailureKind::InvalidInput));
            }
            // The TEXTDATA freeze: the same per-context law the whole split enforces, so the scoped lane rejects
            // exactly the fields the floor rejects.
            if textdata
                && !(if in_quotes {
                    is_admitted_quoted(byte)
                } else {
                    is_textdata(byte)
                })
            {
                return Err(CodecError::new(CodecFailureKind::InvalidInput));
            }
            current.push(byte);
            index += 1;
        }
    }
    if in_quotes {
        return Err(CodecError::new(CodecFailureKind::InvalidInput));
    }
    take_utf8(current)
}

/// Validates one field's accumulated bytes as UTF-8, consuming the scratch.
fn take_utf8(current: &mut Vec<u8>) -> Result<String, CodecError> {
    let bytes = core::mem::take(current);
    String::from_utf8(bytes).map_err(|_| CodecError::new(CodecFailureKind::InvalidInput))
}

#[cfg(test)]
mod tests {
    use super::{decode_field_into, split_fields};
    use jqf_codec_core::{CodecError, CodecFailureKind};

    const QUOTE: Option<u8> = Some(b'"');

    /// The tests' standalone shape of the per-byte split: validate-then-walk, standing in for a caller that holds an
    /// unvalidated payload. Production paths enter through [`split_fields_into`] or the wide walk's fallback, which
    /// have already paid the whole-payload UTF-8 check exactly once.
    #[expect(
        clippy::too_many_arguments,
        reason = "the test stands in for the production entry's exact signature"
    )]
    fn split_fields_scalar(
        payload: &[u8],
        delimiter: u8,
        quote: Option<u8>,
        textdata: bool,
        fields: &mut alloc::vec::Vec<alloc::string::String>,
        scratch: &mut alloc::vec::Vec<u8>,
        spans: Option<&mut alloc::vec::Vec<jqf_source::Span>>,
        keep: Option<&[bool]>,
    ) -> Result<usize, CodecError> {
        if core::str::from_utf8(payload).is_err() {
            return Err(CodecError::new(CodecFailureKind::InvalidInput));
        }
        super::split_fields_validated(payload, delimiter, quote, textdata, fields, scratch, spans, keep)
    }

    /// The tests' whole-buffer shape of [`decode_field_into`]: a fresh scratch per call, returning the decoded string.
    fn decode_field(
        raw: &[u8],
        quote: Option<u8>,
        textdata: bool,
    ) -> Result<alloc::string::String, jqf_codec_core::CodecError> {
        let mut scratch = alloc::vec::Vec::new();
        decode_field_into(raw, quote, textdata, &mut scratch)
    }

    #[test]
    fn an_empty_payload_is_a_zero_field_record() {
        assert!(split_fields(b"", b',', QUOTE, false).expect("splits").is_empty());
    }

    #[test]
    fn a_trailing_delimiter_contributes_an_empty_final_field() {
        assert_eq!(split_fields(b"a,", b',', QUOTE, false).expect("splits"), ["a", ""]);
        assert_eq!(split_fields(b",", b',', QUOTE, false).expect("splits"), ["", ""]);
    }

    #[test]
    fn a_lone_quoted_empty_field_is_one_field() {
        // The closing quote leaves `current` and `fields` both empty, but the non-empty payload still ends with one
        // empty field.
        assert_eq!(split_fields(b"\"\"", b',', QUOTE, false).expect("splits"), [""]);
    }

    #[test]
    fn the_four_field_shapes_agree_with_the_scoped_lane() {
        // `""` -> [""], `a,` -> ["a", ""], `,` -> ["", ""], `` -> [].
        assert_eq!(split_fields(b"\"\"", b',', QUOTE, false).expect("splits"), [""]);
        assert_eq!(split_fields(b"a,", b',', QUOTE, false).expect("splits"), ["a", ""]);
        assert_eq!(split_fields(b",", b',', QUOTE, false).expect("splits"), ["", ""]);
        assert!(split_fields(b"", b',', QUOTE, false).expect("splits").is_empty());
    }

    #[test]
    fn quotes_are_structural_and_doubled_quotes_are_literal() {
        assert_eq!(
            split_fields(b"x,\"a,b\",\"he said \"\"hi\"\"\"", b',', QUOTE, false).expect("splits"),
            ["x", "a,b", "he said \"hi\""]
        );
    }

    #[test]
    fn decoding_one_field_matches_the_whole_split_of_that_field() {
        // The scoped lane's byte-identity law, as an executable statement: every located field range decodes to the
        // split's own string.
        for payload in [
            &b"x,\"a,b\""[..],
            &b"\"he said \"\"hi\"\"\",tail"[..],
            &b"a,,c"[..],
            &b"\"\",\"x\""[..],
        ] {
            let split = split_fields(payload, b',', QUOTE, false).expect("splits");
            for (column, expected) in split.iter().enumerate() {
                let range = crate::scan::field_bytes(payload, b',', column, QUOTE)
                    .expect("scans")
                    .expect("column exists");
                assert_eq!(
                    &decode_field(&payload[range.0..range.1], QUOTE, false).expect("decodes"),
                    expected
                );
            }
        }
    }

    #[test]
    fn an_unterminated_quote_is_invalid() {
        assert!(split_fields(b"\"a", b',', QUOTE, false).is_err());
        assert!(decode_field(b"\"a", QUOTE, false).is_err());
    }

    #[test]
    fn a_lone_quote_mid_field_is_invalid() {
        // The quote law: a quote opens quoted state only at a field start. A lone quote after unquoted content is
        // malformed on both decoders — the whole payload AND the one-field decode.
        assert!(split_fields(b"a\"b", b',', QUOTE, false).is_err());
        assert!(split_fields(b"x,a\"b", b',', QUOTE, false).is_err());
        assert!(split_fields(b"a\"b,x", b',', QUOTE, false).is_err());
        assert!(decode_field(b"a\"b", QUOTE, false).is_err());
        // The quote still opens at a field start.
        assert_eq!(split_fields(b"\"a,b\"", b',', QUOTE, false).expect("splits"), ["a,b"]);
        assert_eq!(decode_field(b"\"a\"\"b\"", QUOTE, false).expect("decodes"), "a\"b");
    }

    #[test]
    fn content_after_a_closing_quote_is_invalid() {
        // The quote law at [`crate::scan::count_and_locate`]: `"a"b` is the load-bearing row. Sibling:
        // `scan::content_after_a_closing_quote_is_invalid`.
        assert!(split_fields(b"\"a\"b", b',', QUOTE, false).is_err());
        assert!(split_fields(b"x,\"a\"b", b',', QUOTE, false).is_err());
        assert!(split_fields(b"\"a\"b,x", b',', QUOTE, false).is_err());
        assert!(decode_field(b"\"a\"b", QUOTE, false).is_err());
        // The legal shapes still decode: delimiter after the quote, field end after the quote, and a doubled quote as a
        // literal quote.
        assert_eq!(
            split_fields(b"\"a\",\"b\"", b',', QUOTE, false).expect("splits"),
            ["a", "b"]
        );
        assert_eq!(split_fields(b"\"a\"", b',', QUOTE, false).expect("splits"), ["a"]);
        assert_eq!(
            split_fields(b"\"a\"\"b\"", b',', QUOTE, false).expect("splits"),
            ["a\"b"]
        );
        assert_eq!(decode_field(b"\"a\"", QUOTE, false).expect("decodes"), "a");
        assert_eq!(decode_field(b"\"a\"\"b\"", QUOTE, false).expect("decodes"), "a\"b");
    }

    #[test]
    fn under_the_tsv_grammar_a_quote_is_plain_field_data() {
        // Acceptance case under the TSV grammar: `a\t"b"\tc` is THREE fields, the middle one the quoted text `"b"` with
        // its quotes intact.
        assert_eq!(
            split_fields(b"a\t\"b\"\tc", b'\t', None, false).expect("splits"),
            ["a", "\"b\"", "c"]
        );
        // A lone quote, an unterminated quote, and doubled quotes are all ordinary data under the no-quote grammar.
        assert_eq!(split_fields(b"a\"b", b'\t', None, false).expect("splits"), ["a\"b"]);
        assert_eq!(split_fields(b"\"a", b'\t', None, false).expect("splits"), ["\"a"]);
        assert_eq!(
            split_fields(b"x\t\"a\"\"b\"", b'\t', None, false).expect("splits"),
            ["x", "\"a\"\"b\""]
        );
        // The scoped lane's decoder agrees: the raw field IS the field.
        assert_eq!(decode_field(b"\"b\"", None, false).expect("decodes"), "\"b\"");
        assert_eq!(decode_field(b"a\"b", None, false).expect("decodes"), "a\"b");
        // A CR/LF inside a payload is still malformed under either grammar (the framer stripped the terminator, so a
        // raw newline is a fault).
        assert!(split_fields(b"a\nb", b'\t', None, false).is_err());
    }

    /// The TEXTDATA freeze: under the RFC 4180 grammar with the gate on, an unquoted field admits TEXTDATA only and a
    /// quoted field's content TEXTDATA/comma/CR/LF plus the structural `""` escape; TAB, NUL, other C0, DEL, and every
    /// non-ASCII scalar are rejected even though their bytes are valid UTF-8. The UTF-8-admitting family (the gate off)
    /// accepts every one of the same payloads unchanged, and the scoped lane's decoder rejects exactly what the whole
    /// split rejects.
    #[test]
    fn the_textdata_freeze_rejects_non_textdata_bytes_only_under_the_gate() {
        let utf8_tail: &[u8] = &[0xc3, 0xa9]; // é
        let cases: &[(&[u8], bool)] = &[
            // Unquoted non-TEXTDATA: TAB, NUL, DEL, non-ASCII.
            (b"a\tb", false),
            (b"a\x00b", false),
            (b"a\x7fb", false),
            (&[b"c".as_slice(), utf8_tail, b"f"].concat(), false),
            // Quoted content: the same alphabet plus comma/CR/LF/"".
            (&[b"\"".as_slice(), utf8_tail, b"\""].concat(), false),
            (b"\"a\tb\"", false),
            (b"\"a\x00b\"", false),
            // Legal under BOTH: TEXTDATA fields, quoted commas, quoted newlines, the doubled-quote escape, spaces, and
            // the excluded structural bytes as themselves.
            (b"cafe", true),
            (b"\"a,b\"", true),
            (b"\"a\nb\"", true),
            (b"\"a\r\nb\"", true),
            (b"\"a\"\"b\"", true),
            (b"\" spaced \"", true),
        ];
        for (payload, rfc_ok) in cases {
            let rfc = split_fields(payload, b',', QUOTE, true);
            let utf8 = split_fields(payload, b',', QUOTE, false);
            assert_eq!(rfc.is_ok(), *rfc_ok, "rfc4180 verdict wrong for {payload:?}: {rfc:?}");
            assert!(utf8.is_ok(), "utf8 family must admit {payload:?}");
            if *rfc_ok {
                assert_eq!(
                    rfc.expect("splits"),
                    utf8.expect("splits"),
                    "the families agree on admitted payloads"
                );
            }
            // The scoped lane's decoder enforces the same law per field.
            let scoped = decode_field(payload, QUOTE, true);
            assert_eq!(scoped.is_ok(), *rfc_ok, "scoped verdict wrong for {payload:?}");
        }
        // A quoted newline stays legal under the freeze (RFC 4180 §2.3); a quoted TAB does not (TAB is C0, not
        // TEXTDATA).
        assert!(split_fields(b"\"a\nb\"", b',', QUOTE, true).is_ok());
        assert!(split_fields(b"\"a\tb\"", b',', QUOTE, true).is_err());
        // The gate never touches the TSV grammar: `"` is data and every UTF-8 scalar is admitted.
        assert!(split_fields(utf8_tail, b'\t', None, true).is_ok());
    }

    /// The wide split is a byte-identity claim: for every delimiter the dispatch serves and every payload shape, the
    /// fast split must equal the per-byte split's fields exactly — same count, same strings, same rejections. The
    /// corpus is the module's own adversarial shapes (quoted fields take the wide walk's fallback), plus delimiter and
    /// UTF-8 edges, asserted under both grammars AND both sides of the TEXTDATA freeze — the gate must reject
    /// identically whichever walk lands. The AUTHORED SPANS join the claim: the wide walk's per-field raw byte ranges
    /// must equal the per-byte walk's, because the edit lane splices at exactly those offsets.
    #[test]
    fn the_wide_split_agrees_with_the_per_byte_split() {
        let corpus: &[&[u8]] = &[
            b"",
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
            b"a\nb",
            b"\"a\"\"b\"",
            b"\"\"",
            b"\"\",\"x\"",
            b"v0_0,v1_0,v2_0,v3_0",
            b"caf\xc3\xa9,na\xc3\xafve",
            b"\xff",
            // TEXTDATA edges: TAB/NUL/DEL in unquoted and quoted fields, long enough to take the wide walk (>=
            // WIDE_SCAN_MIN).
            b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\taaa,bbb",
            b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa,\"a\tb\",ccc",
            b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa,\x7f,zzz",
            b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa,\"x\x00y\",zzz",
            b"caf\xc3\xa9,na\xc3\xafve,wider,wider,wider,wider",
        ];
        for payload in corpus {
            for quote in [Some(b'"'), None] {
                for delimiter in [b',', b'\t', b';'] {
                    for textdata in [false, true] {
                        let mut wide_fields = alloc::vec::Vec::new();
                        let mut wide_scratch = alloc::vec::Vec::new();
                        let mut wide_spans = alloc::vec::Vec::new();
                        let wide = super::split_fields_into(
                            payload,
                            delimiter,
                            quote,
                            textdata,
                            &mut wide_fields,
                            &mut wide_scratch,
                            Some(&mut wide_spans),
                            None,
                        );
                        let mut scalar_fields = alloc::vec::Vec::new();
                        let mut scalar_scratch = alloc::vec::Vec::new();
                        let mut scalar_spans = alloc::vec::Vec::new();
                        let scalar = split_fields_scalar(
                            payload,
                            delimiter,
                            quote,
                            textdata,
                            &mut scalar_fields,
                            &mut scalar_scratch,
                            Some(&mut scalar_spans),
                            None,
                        );
                        assert_eq!(
                            wide.as_ref().ok(),
                            scalar.as_ref().ok(),
                            "split count diverged for {payload:?} with delimiter {delimiter:?} quote {quote:?} textdata {textdata:?}"
                        );
                        if let (Ok(wide_count), Ok(scalar_count)) = (wide, scalar) {
                            assert_eq!(
                                &wide_fields[..wide_count],
                                &scalar_fields[..scalar_count],
                                "split fields diverged for {payload:?} with delimiter {delimiter:?} quote {quote:?} textdata {textdata:?}"
                            );
                            assert_eq!(
                                &wide_spans[..wide_count],
                                &scalar_spans[..scalar_count],
                                "split spans diverged for {payload:?} with delimiter {delimiter:?} quote {quote:?} textdata {textdata:?}"
                            );
                            // Every spanned range slices the payload to the raw bytes of that field — the span
                            // contract the edit lane's splice reads. The locate walk names the same raw range
                            // independently.
                            for (column, span) in wide_spans[..wide_count].iter().enumerate() {
                                let located = crate::scan::field_bytes(payload, delimiter, column, quote)
                                    .expect("locates")
                                    .expect("column exists");
                                assert_eq!(
                                    &payload[span.range()],
                                    &payload[located.0..located.1],
                                    "span does not name the field's raw bytes"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}
