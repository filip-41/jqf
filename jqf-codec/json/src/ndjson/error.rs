//! Structured source-aware NDJSON framing diagnostics.

use jqf_codec_core::{CodecError, CodecFailureKind, RecordIssueCode};
use jqf_source::{Namespace, ResolvedSource, SourceRef};

const NDJSON: Namespace = Namespace::new("ndjson");

/// The stable diagnostic code and message for one framing fault.
///
/// A format's own spelling is the format's to own, so the wording lives with the framer that raises it, exactly as the
/// json-seq and CSV framers own theirs. Codec core keeps the code vocabulary and no format's prose.
#[must_use]
pub const fn framing_text(code: RecordIssueCode) -> (&'static str, &'static str) {
    match code {
        RecordIssueCode::BlankRecord => (
            "blank-record",
            "NDJSON record holds no value; the strict profile forbids blank records",
        ),
        RecordIssueCode::InitialByteOrderMark => (
            "initial-byte-order-mark",
            "NDJSON stream starts with a byte-order mark; the strict profile forbids it",
        ),
        RecordIssueCode::BareCarriageReturn => (
            "bare-carriage-return",
            "NDJSON payload holds a carriage return that does not terminate the record",
        ),
        RecordIssueCode::MalformedPayload => (
            "malformed-record-payload",
            "NDJSON record payload is not one complete strict-JSON text",
        ),
        RecordIssueCode::OversizeRecord => ("record-too-large", "NDJSON record exceeds the per-record byte ceiling"),
        // Codes an NDJSON stream never reports. Both profiles accept a complete final record with no terminator, so the
        // missing terminator never becomes an issue; the other two belong to json-seq. The arm exists so this table
        // stays total over the shared code vocabulary.
        RecordIssueCode::MissingFinalTerminator
        | RecordIssueCode::TruncatedTopLevelScalar
        | RecordIssueCode::UnframedInput => (
            "unexpected-framing-fault",
            "a framing fault this NDJSON stream cannot raise was reported",
        ),
    }
}

/// Builds one terminal framing failure carrying its absolute source position.
///
/// `source_end` is the absolute end of the source (base offset plus byte length). An end-of-input fault's `offset` IS
/// that end, so the label span is clamped onto the final byte rather than starting past EOF.
pub(crate) fn framing(
    source: SourceRef,
    label: &str,
    record_start: u64,
    offset: u64,
    source_end: u64,
    code: RecordIssueCode,
    source_bytes: &[u8],
) -> CodecError {
    let (diagnostic_code, message) = framing_text(code);
    crate::record_diag::framing(
        NDJSON,
        source,
        label,
        record_start,
        offset,
        source_end,
        diagnostic_code,
        message,
        source_bytes,
    )
}

/// Convenience wrapper for a framing failure over a resolved source view.
pub(crate) fn framing_in(
    source: ResolvedSource<'_>,
    record_start: u64,
    offset: u64,
    code: RecordIssueCode,
) -> CodecError {
    let bytes = source.bytes();
    framing(
        source.source(),
        source.label(),
        record_start,
        offset,
        source.base_offset().saturating_add(bytes.len() as u64),
        code,
        bytes,
    )
}

/// One internal-contract violation raised by the framer.
pub(crate) const fn contract(name: &'static str) -> CodecError {
    CodecError::new(CodecFailureKind::InternalContractViolation { contract: name })
}

#[cfg(test)]
mod tests {
    use super::framing_in;
    use jqf_codec_core::RecordIssueCode;
    use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

    #[test]
    fn an_end_of_input_faults_label_span_is_clamped_inside_the_source() {
        // A fault raised AT the source length must clamp its label onto the final byte (1..2 for a two-byte input), not
        // start past EOF (2..3).
        let source = ResolvedSource::new(
            SourceRef::new(SourceId::new(1), SourceKind::Input),
            "test.ndjson",
            b"{}",
            0,
        );
        let error = framing_in(source, 0, 2, RecordIssueCode::MalformedPayload);
        let labels = error.diagnostic().expect("framing fault carries a diagnostic").labels();
        assert_eq!(labels.len(), 1);
        let span = labels[0].span();
        assert_eq!((span.start(), span.end()), (1, 2));
    }

    #[test]
    fn an_in_range_fault_keeps_its_exact_one_byte_span() {
        // The clamp must not move in-range faults: a blank record's label stays at its own byte.
        let source = ResolvedSource::new(
            SourceRef::new(SourceId::new(1), SourceKind::Input),
            "test.ndjson",
            b"{\"v\":1}\n\n{\"v\":2}\n",
            0,
        );
        let error = framing_in(source, 8, 8, RecordIssueCode::BlankRecord);
        let labels = error.diagnostic().expect("framing fault carries a diagnostic").labels();
        assert_eq!(labels.len(), 1);
        let span = labels[0].span();
        assert_eq!((span.start(), span.end()), (8, 9));
    }
}
