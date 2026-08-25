//! Delimited-record option payloads.
//!
//! CSV/TSV delimiter, quote, header, and terminator spellings live with the delimited grammar. Codec-core keeps only
//! the opaque format/dialect id strings.

use jqf_codec_core::{
    CSV_FORMAT_ID, CSV_JQF_RFC4180_HEADER_DIALECT_ID, CSV_JQF_UTF8_HEADER_DIALECT_ID, CSV_RFC4180_DIALECT_ID,
    CSV_RFC4180_HEADER_DIALECT_ID, CSV_UTF8_DIALECT_ID, CSV_UTF8_HEADER_DIALECT_ID, CodecError, CodecFailureKind,
    TSV_FORMAT_ID, TSV_JQF_LF_HEADER_DIALECT_ID, TSV_UTF8_DIALECT_ID, TSV_UTF8_HEADER_DIALECT_ID,
};

/// The record terminator one delimited OUTPUT dialect appends after every row, decided per dialect: RFC 4180 output
/// appends CRLF, TSV output appends LF.
///
/// Distinct from [`jqf_codec_core::RecordTerminator`], which is the OBSERVED framing terminator of a delivered record;
/// this is the terminator a delimited ENCODER writes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DelimitedTerminator {
    /// RFC 4180's `\r\n`.
    CrLf,
    /// `\n`.
    Lf,
}

impl DelimitedTerminator {
    /// The terminator's exact bytes.
    #[must_use]
    pub const fn bytes(self) -> &'static [u8] {
        match self {
            Self::CrLf => b"\r\n",
            Self::Lf => b"\n",
        }
    }
}

/// Whether one byte may serve as a field delimiter.
#[must_use]
pub const fn is_valid_delimiter(byte: u8) -> bool {
    matches!(
        byte,
        b',' | b'\t' | b';' | b'|' | b':' | b' ' | b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z'
    )
}

/// Normalized CSV/TSV read-side options.
///
/// The field delimiter, the quoting character, HEADER MODE, and the TEXTDATA freeze are part of the EXECUTABLE grammar
/// (the first two change how records split; the third changes which records are data and what kind each published value
/// has; the fourth changes which bytes a field may carry), so they are normalized into the options fingerprint per the
/// portfolio's explicit-grammar law. `quote` is the grammar axis: `Some('"')` is RFC 4180 quoting, `None` is the TSV
/// no-quote grammar in which `"` is ordinary field data. The delimiter is configurable under the CSV grammar (a
/// caller-supplied delimiter with the TSV grammar is a `RequirementMismatch` — the TSV constructor binds tab itself).
///
/// The `textdata` axis freezes the field alphabet: `true` pins it to the RFC's ASCII `TEXTDATA` (the `csv.rfc4180@1`
/// family), `false` admits every valid-UTF-8 scalar (the `csv.utf8@1` family). TSV is always UTF-8-admitting; its
/// grammar has no quote to hide a newline behind and its dialect ids name utf8 already.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CsvDecodeOptions {
    delimiter: u8,
    max_record_bytes: u64,
    header: bool,
    quote: Option<u8>,
    textdata: bool,
}

impl CsvDecodeOptions {
    /// Normalizes read-side options against the request's input ceiling.
    ///
    /// The per-record ceiling defaults to the request's own effective input ceiling, so an omitted option never makes a
    /// legal stream illegal. An explicit value may only make the ceiling SMALLER.
    ///
    /// `header` is REQUIRED rather than defaulted: it selects between the two sealed input dialects of the UTF-8 family
    /// ([`CSV_UTF8_DIALECT_ID`] and [`CSV_UTF8_HEADER_DIALECT_ID`]), and a caller that could omit it could silently
    /// open a stream under the other dialect's row shape.
    ///
    /// This constructor builds the UTF-8-ADMITTING family — the dialects the short `--input-format csv` selects. The
    /// frozen RFC alphabet family is [`Self::try_new_rfc4180`].
    pub fn try_new(
        delimiter: Option<u8>,
        max_record_bytes: Option<u64>,
        input_ceiling: u64,
        header: bool,
    ) -> Result<Self, CodecError> {
        Self::try_new_with_grammar(delimiter, max_record_bytes, input_ceiling, header, Some(b'"'), false)
    }

    /// The RFC 4180 constructor: the same quoting grammar and ceiling law as [`Self::try_new`], with the field alphabet
    /// FROZEN to the RFC's ASCII `TEXTDATA = %x20-21 / %x23-2B / %x2D-7E`. An unquoted field carries TEXTDATA only; a
    /// quoted field TEXTDATA, comma, CR, LF, or the `""` escape; TAB, NUL, other C0/DEL, and every non-ASCII scalar are
    /// `InvalidInput` even when their bytes are valid UTF-8. These are the explicit `--input-dialect csv.rfc4180@1` /
    /// `csv.rfc4180-header@1` opt-ins.
    pub fn try_new_rfc4180(
        delimiter: Option<u8>,
        max_record_bytes: Option<u64>,
        input_ceiling: u64,
        header: bool,
    ) -> Result<Self, CodecError> {
        Self::try_new_with_grammar(delimiter, max_record_bytes, input_ceiling, header, Some(b'"'), true)
    }

    /// The TSV constructor: tab delimiter, no quote, the same record ceiling and header law as the CSV constructor. The
    /// delimiter is BOUND here (tab) — a caller-supplied delimiter with the TSV grammar is `RequirementMismatch`,
    /// which is why this constructor takes none. TSV admits every Unicode scalar except TAB/CR/LF regardless of this
    /// crate's CSV families, so the textdata axis is off.
    pub fn try_new_tsv(max_record_bytes: Option<u64>, input_ceiling: u64, header: bool) -> Result<Self, CodecError> {
        Self::try_new_with_grammar(Some(b'\t'), max_record_bytes, input_ceiling, header, None, false)
    }

    /// Reconstructs options from the codec-neutral open envelope.
    ///
    /// The envelope names only the FRAMING axes (delimiter, header, quote), which is all the framer reads; the
    /// field-alphabet freeze travels with the typed options the payload decode receives, so this constructor builds the
    /// UTF-8-admitting family and the framer's behavior is identical under both.
    pub(crate) fn try_from_open(
        delimiter: u8,
        header: bool,
        quote: Option<u8>,
        max_record_bytes: u64,
    ) -> Result<Self, CodecError> {
        Self::try_new_with_grammar(
            Some(delimiter),
            Some(max_record_bytes),
            max_record_bytes,
            header,
            quote,
            false,
        )
    }

    fn try_new_with_grammar(
        delimiter: Option<u8>,
        max_record_bytes: Option<u64>,
        input_ceiling: u64,
        header: bool,
        quote: Option<u8>,
        textdata: bool,
    ) -> Result<Self, CodecError> {
        let delimiter = delimiter.unwrap_or(b',');
        if !is_valid_delimiter(delimiter) {
            return Err(CodecError::new(CodecFailureKind::RequirementMismatch));
        }
        let requested = max_record_bytes.unwrap_or(input_ceiling);
        if requested == 0 || requested > input_ceiling {
            return Err(CodecError::new(CodecFailureKind::RequirementMismatch));
        }
        Ok(Self {
            delimiter,
            max_record_bytes: requested,
            header,
            quote,
            textdata,
        })
    }

    /// Field delimiter byte.
    #[must_use]
    pub const fn delimiter(self) -> u8 {
        self.delimiter
    }

    /// Effective per-record payload ceiling in bytes.
    #[must_use]
    pub const fn max_record_bytes(self) -> u64 {
        self.max_record_bytes
    }

    /// Whether the first record is consumed as a header rather than published.
    #[must_use]
    pub const fn header(self) -> bool {
        self.header
    }

    /// The quoting character: `Some('"')` under the RFC 4180 grammar, `None` under the TSV no-quote grammar.
    #[must_use]
    pub const fn quote(self) -> Option<u8> {
        self.quote
    }

    /// Whether the field alphabet is frozen to the RFC's ASCII TEXTDATA (the `csv.rfc4180@1` family). `false` admits
    /// every valid-UTF-8 scalar (the `csv.utf8@1` family); always `false` under the TSV grammar.
    #[must_use]
    pub const fn textdata(self) -> bool {
        self.textdata
    }

    /// The format identity these options normalize to: `csv` under the quoted grammar, `tsv` under the no-quote
    /// grammar. One options struct serves both registrations; the grammar axis picks the format.
    #[must_use]
    pub const fn format_id(self) -> &'static str {
        match self.quote {
            Some(_) => CSV_FORMAT_ID,
            None => TSV_FORMAT_ID,
        }
    }

    /// The sealed input dialect identity these options normalize to — the 2×2×2 grammar-by-header-by-alphabet match
    /// (TSV doubles the header pair; its alphabet axis is fixed).
    #[must_use]
    pub const fn dialect_id(self) -> &'static str {
        match (self.quote, self.header, self.textdata) {
            (Some(_), false, true) => CSV_RFC4180_DIALECT_ID,
            (Some(_), true, true) => CSV_RFC4180_HEADER_DIALECT_ID,
            (Some(_), false, false) => CSV_UTF8_DIALECT_ID,
            (Some(_), true, false) => CSV_UTF8_HEADER_DIALECT_ID,
            (None, false, _) => TSV_UTF8_DIALECT_ID,
            (None, true, _) => TSV_UTF8_HEADER_DIALECT_ID,
        }
    }
}

/// Normalized CSV/TSV write-side options.
///
/// The delimiter, the quoting character, and the RECORD TERMINATOR are part of the executable output grammar. v1 is
/// otherwise deterministic: under the CSV grammar every field is quoted exactly when RFC 4180 requires it and the
/// terminator is CRLF (`csv.jqf-rfc4180@1` advertises an RFC number, so it must append the RFC's CRLF); under the TSV
/// grammar fields join with TAB, a field containing TAB/CR/LF is unrepresentable (no quote to protect it), and the
/// terminator is LF. The terminator is appended inside the encoder's staging buffer (the record-stream seam), and it is
/// per-dialect rather than a global constant because `tsv.jqf-lf@1` correctly wants LF.
///
/// Header mode is NOT carried here: the encoder reads it from the request's own
/// [`dialect`](jqf_codec_core::EncodeRequest::dialect), which the encode side — unlike the decode side — already
/// sees.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CsvEncodeOptions {
    delimiter: u8,
    quote: Option<u8>,
    terminator: DelimitedTerminator,
}

impl CsvEncodeOptions {
    /// Selects the canonical output delimiter under the RFC 4180 grammar (quote `"`, CRLF terminator).
    pub fn try_new(delimiter: Option<u8>) -> Result<Self, CodecError> {
        let delimiter = delimiter.unwrap_or(b',');
        if !is_valid_delimiter(delimiter) {
            return Err(CodecError::new(CodecFailureKind::RequirementMismatch));
        }
        Ok(Self {
            delimiter,
            quote: Some(b'"'),
            terminator: DelimitedTerminator::CrLf,
        })
    }

    /// The TSV output options: TAB joins, no quote, LF terminator.
    pub fn try_new_tsv() -> Result<Self, CodecError> {
        Ok(Self {
            delimiter: b'\t',
            quote: None,
            terminator: DelimitedTerminator::Lf,
        })
    }

    /// The canonical output delimiter.
    #[must_use]
    pub const fn delimiter(self) -> u8 {
        self.delimiter
    }

    /// The quoting character, `None` under the TSV no-quote grammar.
    #[must_use]
    pub const fn quote(self) -> Option<u8> {
        self.quote
    }

    /// The per-dialect record terminator.
    #[must_use]
    pub const fn terminator(self) -> DelimitedTerminator {
        self.terminator
    }
}

impl Default for CsvEncodeOptions {
    fn default() -> Self {
        Self {
            delimiter: b',',
            quote: Some(b'"'),
            terminator: DelimitedTerminator::CrLf,
        }
    }
}

/// True when `id` is a headered CSV or TSV dialect.
///
/// The edit drive uses this to decide whether to publish the authored header prefix before splicing data records.
#[must_use]
pub fn is_headered_delimited_dialect(id: &str) -> bool {
    matches!(
        id,
        CSV_RFC4180_HEADER_DIALECT_ID
            | CSV_JQF_RFC4180_HEADER_DIALECT_ID
            | CSV_UTF8_HEADER_DIALECT_ID
            | CSV_JQF_UTF8_HEADER_DIALECT_ID
            | TSV_UTF8_HEADER_DIALECT_ID
            | TSV_JQF_LF_HEADER_DIALECT_ID
    )
}

/// The headered dialect's grow/shrink `--edit` refusal.
///
/// Identity and cell assignment are served: the edit drive publishes the authored header prefix and splices data-record
/// fields. A reshape that would disagree with that prefix (grow or shrink a row) still refuses.
#[must_use]
pub(crate) fn headered_delimited_edit_refusal() -> CodecError {
    let base = CodecError::new(CodecFailureKind::UnsupportedRepresentation);
    let Some(diagnostic) = jqf_source::Diagnostic::try_new(
        jqf_source::Namespace::new("csv").code("headered-edit"),
        jqf_source::Severity::Error,
        "the headered dialect cannot grow or shrink a row: the header is \
         stream-prefix schema, not a record",
    ) else {
        return base;
    };
    base.with_diagnostic(diagnostic)
}

#[cfg(test)]
mod tests {
    use super::{CsvDecodeOptions, CsvEncodeOptions, DelimitedTerminator};
    use jqf_codec_core::{
        CSV_FORMAT_ID, CSV_RFC4180_DIALECT_ID, CSV_RFC4180_HEADER_DIALECT_ID, CSV_UTF8_DIALECT_ID,
        CSV_UTF8_HEADER_DIALECT_ID, TSV_FORMAT_ID, TSV_UTF8_DIALECT_ID, TSV_UTF8_HEADER_DIALECT_ID,
    };

    #[test]
    fn headered_dialect_ids_are_exactly_the_four_headered_spellings() {
        assert!(super::is_headered_delimited_dialect(CSV_RFC4180_HEADER_DIALECT_ID));
        assert!(super::is_headered_delimited_dialect(TSV_UTF8_HEADER_DIALECT_ID));
        assert!(!super::is_headered_delimited_dialect(CSV_RFC4180_DIALECT_ID));
        assert!(!super::is_headered_delimited_dialect("json"));
    }

    #[test]
    fn an_omitted_record_ceiling_defaults_to_the_input_ceiling() {
        assert_eq!(
            CsvDecodeOptions::try_new(None, None, 1024, false)
                .expect("default")
                .max_record_bytes(),
            1024
        );
    }

    #[test]
    fn an_explicit_ceiling_may_shrink_but_never_grow() {
        assert!(CsvDecodeOptions::try_new(None, Some(2048), 1024, false).is_err());
        assert!(CsvDecodeOptions::try_new(None, Some(0), 1024, false).is_err());
    }

    #[test]
    fn the_delimiter_defaults_to_comma_and_rejects_grammar_bytes() {
        assert_eq!(
            CsvDecodeOptions::try_new(None, None, 16, false)
                .expect("comma")
                .delimiter(),
            b','
        );
        assert_eq!(
            CsvDecodeOptions::try_new(Some(b'\t'), None, 16, false)
                .expect("tab")
                .delimiter(),
            b'\t'
        );
        assert!(CsvDecodeOptions::try_new(Some(b'"'), None, 16, false).is_err());
        assert!(CsvDecodeOptions::try_new(Some(b'\n'), None, 16, false).is_err());
    }

    #[test]
    fn encode_options_default_to_comma_and_validate_the_delimiter() {
        assert_eq!(CsvEncodeOptions::default().delimiter(), b',');
        assert_eq!(
            CsvEncodeOptions::try_new(Some(b';')).expect("semicolon").delimiter(),
            b';'
        );
        assert!(CsvEncodeOptions::try_new(Some(b'\n')).is_err());
    }

    #[test]
    fn the_tsv_constructor_binds_tab_no_quote_and_the_tsv_identity() {
        let options = CsvDecodeOptions::try_new_tsv(None, 1024, false).expect("tsv");
        assert_eq!(options.delimiter(), b'\t');
        assert_eq!(options.quote(), None);
        assert_eq!(options.format_id(), TSV_FORMAT_ID);
        assert_eq!(options.dialect_id(), TSV_UTF8_DIALECT_ID);
        assert!(!options.textdata());
        let headered = CsvDecodeOptions::try_new_tsv(None, 1024, true).expect("tsv header");
        assert_eq!(headered.dialect_id(), TSV_UTF8_HEADER_DIALECT_ID);
        // The plain CSV constructor is the UTF-8-ADMITTING family: same grammar, no TEXTDATA freeze.
        let tabbed = CsvDecodeOptions::try_new(Some(b'\t'), None, 1024, false).expect("tab");
        assert_eq!(tabbed.quote(), Some(b'"'));
        assert_eq!(tabbed.format_id(), CSV_FORMAT_ID);
        assert_eq!(tabbed.dialect_id(), CSV_UTF8_DIALECT_ID);
        assert!(!tabbed.textdata());
        let encode = CsvEncodeOptions::try_new_tsv().expect("tsv encode");
        assert_eq!(encode.delimiter(), b'\t');
        assert_eq!(encode.quote(), None);
        assert_eq!(encode.terminator(), DelimitedTerminator::Lf);
        assert_eq!(
            CsvEncodeOptions::try_new(None).expect("csv").terminator(),
            DelimitedTerminator::CrLf
        );
        assert_eq!(
            CsvDecodeOptions::try_new(None, None, 16, true)
                .expect("header")
                .dialect_id(),
            CSV_UTF8_HEADER_DIALECT_ID
        );
    }

    /// The 2×2×2 dialect map: the RFC constructor freezes TEXTDATA and names the rfc4180 ids; the plain constructor
    /// admits UTF-8 and names the utf8 ids; TSV ignores the axis.
    #[test]
    fn the_rfc4180_constructor_freezes_textdata_and_names_the_rfc_ids() {
        let array = CsvDecodeOptions::try_new_rfc4180(None, None, 1024, false).expect("rfc");
        assert!(array.textdata());
        assert_eq!(array.dialect_id(), CSV_RFC4180_DIALECT_ID);
        assert_eq!(array.quote(), Some(b'"'));
        let headered = CsvDecodeOptions::try_new_rfc4180(None, None, 1024, true).expect("rfc h");
        assert!(headered.textdata());
        assert_eq!(headered.dialect_id(), CSV_RFC4180_HEADER_DIALECT_ID);
        // The ceiling law is shared with the other constructors.
        assert!(CsvDecodeOptions::try_new_rfc4180(None, Some(2048), 1024, false).is_err());
        assert!(CsvDecodeOptions::try_new_rfc4180(Some(b'\n'), None, 16, false).is_err());
    }
}
