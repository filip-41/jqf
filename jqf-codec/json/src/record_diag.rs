//! The shared record-framing diagnostic builder.
//!
//! A format's own spelling is the format's to own — the code/message table (`framing_text`) lives with the framer
//! that raises it. The MECHANICAL builder around that wording (diagnostic construction, source record, the end-of-input
//! label clamp, the primary label) is one law shared by every framer in this crate, so it lives here once; a one-sided
//! edit to the clamp or the severity used to be able to drift one format's diagnostics away from its siblings'.

use jqf_codec_core::{CodecError, CodecFailureKind};
use jqf_source::{Label, Namespace, Severity, SourceRef, Span};

/// Builds one terminal framing failure carrying its absolute source position.
///
/// `source_end` is the absolute end of the source (base offset plus byte length). An end-of-input fault's `offset` IS
/// that end, so the label span is clamped onto the final byte rather than starting past EOF. The clamp also floors onto
/// a UTF-8 character boundary using `source_bytes`: a multibyte character straddling the clamp point must not yield a
/// label span that starts mid-codepoint (labels carry spans, and a mid-codepoint start makes the rendered caret slice
/// invalid).
#[allow(
    clippy::too_many_arguments,
    reason = "one builder law: the namespace, the position triple, and the format-owned wording travel together"
)]
pub(crate) fn framing(
    namespace: Namespace,
    source: SourceRef,
    label: &str,
    record_start: u64,
    offset: u64,
    source_end: u64,
    diagnostic_code: &'static str,
    message: &'static str,
    source_bytes: &[u8],
) -> CodecError {
    let base = CodecError::new(CodecFailureKind::InvalidInput);
    let Some(mut diagnostic) =
        jqf_source::Diagnostic::try_new(namespace.code(diagnostic_code), Severity::Error, message)
    else {
        return base;
    };
    let Some(source_record) = jqf_source::DiagnosticSource::try_new(source, label, record_start) else {
        return base;
    };
    let Some(extended) = diagnostic.try_with_source(source_record) else {
        return base;
    };
    diagnostic = extended;
    let raw_start = usize::try_from(offset).unwrap_or(usize::MAX);
    let raw_end = usize::try_from(source_end).unwrap_or(usize::MAX);
    // A fault raised AT the source length (a missing terminator, an unterminated tail) would otherwise label a span
    // STARTING past EOF (`Primary input#0 2..3` for a two-byte input). Clamp onto the final byte. The degenerate
    // ZERO-length source has no final byte; the clamp deliberately answers 0..1 there — the saturating arithmetic's
    // stable, never-underflowing answer, pinned by `seq::error`'s
    // `an_empty_source_still_produces_a_span_inside_itself`.
    let mut start = raw_start.min(raw_end.saturating_sub(1));
    // A multibyte character straddling the clamp point must not yield a label span that starts mid-codepoint, so the
    // clamp floors onto a UTF-8 character boundary (the walk is a no-op for a zero-length or ASCII-final source).
    while start > 0 && source_bytes.get(start).is_some_and(|b| b & 0xC0 == 0x80) {
        start -= 1;
    }
    let Some(label_record) = Label::try_primary(source, Span::from_usize(start, start.saturating_add(1)), message)
    else {
        return base;
    };
    let Some(extended) = diagnostic.try_with_label(label_record) else {
        return base;
    };
    base.with_diagnostic(extended)
}
