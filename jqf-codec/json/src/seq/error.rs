//! Structured source-aware json-seq framing diagnostics.

use jqf_codec_core::{CodecError, CodecFailureKind, RecordIssueCode};
use jqf_source::{Namespace, ResolvedSource, SourceRef};

const JSON_SEQ: Namespace = Namespace::new("json-seq");

/// The stable diagnostic code and message for one json-seq framing fault.
///
/// The recovering profile's OWN faults are reported here too (an issue's `code` is the same vocabulary), but their
/// severity and exit-law are the flag-scoped profile's: advisory, and never fatal.
#[must_use]
pub const fn framing_text(code: RecordIssueCode) -> (&'static str, &'static str) {
    match code {
        RecordIssueCode::TruncatedTopLevelScalar => (
            "truncated-top-level-scalar",
            // The framer is grammar-free: it knows only that the unit's non-delimiting head reached its end with no
            // separating whitespace, so the message names the CLASS of unit (the RFC 7464 section 2.4 scalars) without
            // claiming to have read the bytes. Arbitrary invalid text classifies here too.
            "json-seq unit ends in a non-self-delimiting scalar with no JSON whitespace \
             before the RS or end of input (RFC 7464 section 2.4)",
        ),
        RecordIssueCode::UnframedInput => (
            "unframed-input",
            "json-seq input never contained an RS, so no possible-JSON was ever begun",
        ),
        RecordIssueCode::MalformedPayload => (
            "malformed-unit-payload",
            "json-seq possible-JSON is not one complete strict-JSON text",
        ),
        // The oversize ceiling is LIVE here: `JsonSeqRecordSession` faults a unit whose payload exceeds it, so this
        // class owns its own text in this framer's vocabulary (a unit), not the cannot-raise placeholder.
        RecordIssueCode::OversizeRecord => ("unit-too-large", "json-seq unit exceeds the per-unit byte ceiling"),
        // json-seq never raises these NDJSON/CSV framing faults; the arm exists so the shared vocabulary stays total
        // over the codec that raised them.
        RecordIssueCode::BlankRecord | RecordIssueCode::InitialByteOrderMark | RecordIssueCode::BareCarriageReturn => (
            "unexpected-framing-fault",
            "a framing fault this json-seq stream cannot raise was reported",
        ),
        // The strict profile's unterminated zero-byte tail. The recovering profile discards it silently (reference
        // parity). As an ISSUE text this arm is dead by law — no poll ever pushes this class into the ordered-issue
        // stream — but the shared code vocabulary must stay total over every codec's table, so the arm exists for ABI
        // symmetry alone and MUST stay unfireable: if a future publisher bug ever emits one, its plausible-looking text
        // must not let it pass as intended behavior.
        RecordIssueCode::MissingFinalTerminator => (
            "unterminated-zero-byte-item",
            "json-seq input ends in RS after its last complete item: an unterminated \
             zero-byte possible-JSON",
        ),
    }
}

/// Builds one terminal framing failure carrying its absolute source position.
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
        JSON_SEQ,
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

/// The strict profile's unterminated zero-byte tail: an input ending in RS after its last complete item (or an RS-only
/// input) leaves a possible-JSON that began but never terminated. The recovering profile discards it silently
/// (reference parity), so this raise site is STRICT-only. It carries [`RecordIssueCode::MissingFinalTerminator`]
/// because the terminal-failure path needs a code from the shared vocabulary; no ordered issue ever does.
pub(crate) fn trailing_rs_in(source: ResolvedSource<'_>, offset: u64) -> CodecError {
    framing_in(source, offset, offset, RecordIssueCode::MissingFinalTerminator)
}

/// One internal-contract violation raised by the framer.
pub(crate) const fn contract(name: &'static str) -> CodecError {
    CodecError::new(CodecFailureKind::InternalContractViolation { contract: name })
}

#[cfg(test)]
mod tests {
    use super::{framing_in, trailing_rs_in};
    use jqf_codec_core::RecordIssueCode;
    use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

    fn resolved(bytes: &'static [u8]) -> ResolvedSource<'static> {
        ResolvedSource::new(
            SourceRef::new(SourceId::new(1), SourceKind::Input),
            "test.json-seq",
            bytes,
            0,
        )
    }

    fn label_span(error: &jqf_codec_core::CodecError) -> (u32, u32) {
        let labels = error
            .diagnostic()
            .expect("a framing fault carries a diagnostic")
            .labels();
        assert_eq!(labels.len(), 1);
        (labels[0].span().start(), labels[0].span().end())
    }

    #[test]
    fn a_trailing_rs_at_end_of_input_labels_the_final_byte() {
        // The live strict path: an input ending in RS raises AT the source length, so an unclamped span would start
        // past EOF (3..4 here).
        let error = trailing_rs_in(resolved(b"1\n\x1e"), 3);
        assert_eq!(label_span(&error), (2, 3));
    }

    #[test]
    fn an_in_range_fault_keeps_its_exact_one_byte_span() {
        // The clamp must not move a fault that is already inside the source.
        let error = framing_in(resolved(b"\x1e1\n\x1e2\n"), 3, 4, RecordIssueCode::MalformedPayload);
        assert_eq!(label_span(&error), (4, 5));
    }

    #[test]
    fn an_empty_source_still_produces_a_span_inside_itself() {
        // The degenerate end: a zero-length source has no byte to point at, and the clamp must answer 0..1 rather than
        // underflow.
        let error = trailing_rs_in(resolved(b""), 0);
        assert_eq!(label_span(&error), (0, 1));
    }
}
