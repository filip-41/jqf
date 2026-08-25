//! The `messagepack` namespace's diagnostic construction and decode failures.

use jqf_codec_core::{CodecError, CodecFailureKind};
use jqf_source::{Namespace, ResolvedSource};

/// The diagnostic namespace of the codec's rejects.
const fn namespace() -> Namespace {
    Namespace::new("messagepack")
}

/// A structural decode failure with a messagepack-namespace diagnostic naming the byte offset and the violated clause.
pub(crate) fn invalid(
    source: ResolvedSource<'_>,
    offset: usize,
    code: &'static str,
    message: &'static str,
) -> CodecError {
    let end = offset.saturating_add(usize::from(offset < source.bytes().len()));
    jqf_codec_core::diagnosed(
        CodecFailureKind::InvalidInput,
        namespace(),
        source,
        offset,
        end,
        code,
        message,
    )
}

/// An unrepresentable-semantic failure with a messagepack-namespace diagnostic. Used for the wire@1 invalid-UTF-8 span,
/// an invalid reserved `-1` timestamp payload, and a non-`str` map key set.
pub(crate) fn unrepresentable(source: ResolvedSource<'_>, offset: usize, message: &'static str) -> CodecError {
    let end = offset.saturating_add(usize::from(offset < source.bytes().len()));
    jqf_codec_core::diagnosed(
        CodecFailureKind::UnsupportedRepresentation,
        namespace(),
        source,
        offset,
        end,
        "unrepresentable",
        message,
    )
}

/// An encode-side unrepresentable failure with a messagepack-namespace diagnostic naming the value and its path (the
/// Decimal refusal and the out-of-range integer).
pub(crate) fn encode_unrepresentable(message: &str) -> CodecError {
    let base = CodecError::new(CodecFailureKind::UnsupportedRepresentation);
    let Some(diagnostic) = jqf_source::Diagnostic::try_new(
        namespace().code("unrepresentable"),
        jqf_source::Severity::Error,
        message,
    ) else {
        return base;
    };
    base.with_diagnostic(diagnostic)
}
