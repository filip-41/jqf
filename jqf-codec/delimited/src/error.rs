//! Structured source-aware CSV framing diagnostics.
//!
//! The framer raises exactly the three fault codes mapped here — initial BOM, bare carriage return, oversize record;
//! a missing final terminator is filtered before it reaches this module. Each fault becomes a [`jqf_source`]-spanned
//! diagnostic over the offending byte range.

use jqf_codec_core::{CodecError, CodecFailureKind, RecordIssueCode};
use jqf_source::{Label, Namespace, ResolvedSource, Severity, Span};

const CSV: Namespace = Namespace::new("csv");

/// The stable diagnostic code and message for one framing fault this crate produces.
///
/// `None` for any other [`RecordIssueCode`]: those are an internal-contract break, not a user-facing framing fault.
#[must_use]
pub(crate) const fn framing_text(code: RecordIssueCode) -> Option<(&'static str, &'static str)> {
    match code {
        RecordIssueCode::InitialByteOrderMark => Some((
            "initial-byte-order-mark",
            "CSV stream starts with a byte-order mark; the strict profile forbids it",
        )),
        RecordIssueCode::BareCarriageReturn => Some((
            "bare-carriage-return",
            "CSV record holds a carriage return that does not terminate the record",
        )),
        RecordIssueCode::OversizeRecord => Some(("record-too-large", "CSV record exceeds the per-record byte ceiling")),
        _ => None,
    }
}

/// Builds one terminal framing failure carrying its absolute source position.
pub(crate) fn framing_in(
    source: ResolvedSource<'_>,
    record_start: u64,
    offset: u64,
    code: RecordIssueCode,
) -> CodecError {
    let Some((diagnostic_code, message)) = framing_text(code) else {
        return contract("CSV framing fault vocabulary");
    };
    let base = CodecError::new(CodecFailureKind::InvalidInput);
    let Some(mut diagnostic) = jqf_source::Diagnostic::try_new(CSV.code(diagnostic_code), Severity::Error, message)
    else {
        return base;
    };
    let Some(source_record) = jqf_source::DiagnosticSource::try_new(source.source(), source.label(), record_start)
    else {
        return base;
    };
    let Some(extended) = diagnostic.try_with_source(source_record) else {
        return base;
    };
    diagnostic = extended;
    let start = usize::try_from(offset).unwrap_or(usize::MAX);
    let Some(label) = Label::try_primary(
        source.source(),
        Span::from_usize(start, start.saturating_add(1)),
        message,
    ) else {
        return base;
    };
    let Some(extended) = diagnostic.try_with_label(label) else {
        return base;
    };
    base.with_diagnostic(extended)
}

/// One internal-contract violation raised by the framer.
pub(crate) const fn contract(name: &'static str) -> CodecError {
    CodecError::new(CodecFailureKind::InternalContractViolation { contract: name })
}

/// A headered row whose field count disagrees with the header.
///
/// The message names the header's physical extent so a zero-column header (a blank first row) is findable: the
/// rejection fires at the first data row, but the defect is the blank line occupying `0..header_end`.
pub(crate) fn ragged_row(header_columns: usize, row_fields: usize, header_end: u64) -> CodecError {
    let message = if header_columns == 0 {
        alloc::format!(
            "headered CSV row has {row_fields} fields against a zero-column header (blank first row occupying bytes 0..{header_end})"
        )
    } else {
        alloc::format!(
            "headered CSV row has {row_fields} fields, header has {header_columns} (header occupies bytes 0..{header_end})"
        )
    };
    let base = CodecError::new(CodecFailureKind::InvalidInput);
    let Some(diagnostic) = jqf_source::Diagnostic::try_new(CSV.code("ragged-row"), Severity::Error, &message) else {
        return base;
    };
    base.with_diagnostic(diagnostic)
}

#[cfg(test)]
mod tests {
    use super::{framing_in, framing_text, ragged_row};
    use jqf_codec_core::{CodecFailureKind, RecordIssueCode};
    use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

    #[test]
    fn an_unknown_framing_code_is_an_internal_contract_break() {
        assert!(framing_text(RecordIssueCode::BlankRecord).is_none());
        assert!(framing_text(RecordIssueCode::UnframedInput).is_none());
        let source = ResolvedSource::new(
            SourceRef::new(SourceId::new(1), SourceKind::Input),
            "t.csv",
            b"a,b\n",
            0,
        );
        let error = framing_in(source, 0, 0, RecordIssueCode::BlankRecord);
        assert!(matches!(
            error.kind(),
            CodecFailureKind::InternalContractViolation {
                contract: "CSV framing fault vocabulary"
            }
        ));
        // The three codes this crate produces stay user-facing InvalidInput.
        assert!(framing_text(RecordIssueCode::OversizeRecord).is_some());
    }

    #[test]
    fn a_zero_column_header_names_the_blank_first_row_extent() {
        let error = ragged_row(0, 2, 1);
        let message = error.diagnostic().expect("carries a diagnostic").message();
        assert!(
            message.contains("zero-column header") && message.contains("0..1"),
            "{message}"
        );
        let error = ragged_row(2, 3, 7);
        let message = error.diagnostic().expect("carries a diagnostic").message();
        assert!(
            message.contains("header has 2") && message.contains("0..7"),
            "{message}"
        );
    }
}
