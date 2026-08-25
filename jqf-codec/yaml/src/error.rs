//! Structured source-aware YAML diagnostics.

use jqf_codec_core::{CodecError, CodecFailureKind};
use jqf_source::{Namespace, ResolvedSource};

const YAML: Namespace = Namespace::new("yaml");

/// Constructs an `InvalidInput` reject diagnostic in the `yaml` namespace.
pub(crate) fn invalid(
    source: ResolvedSource<'_>,
    offset: usize,
    code: &'static str,
    message: &'static str,
) -> CodecError {
    jqf_codec_core::diagnosed(
        CodecFailureKind::InvalidInput,
        YAML,
        source,
        offset,
        offset.saturating_add(usize::from(offset < source.bytes().len())),
        code,
        message,
    )
}

/// Constructs an `InvalidInput` diagnostic over an exact byte range.
pub(crate) fn invalid_range(
    source: ResolvedSource<'_>,
    start: usize,
    end: usize,
    code: &'static str,
    message: &'static str,
) -> CodecError {
    jqf_codec_core::diagnosed(CodecFailureKind::InvalidInput, YAML, source, start, end, code, message)
}

/// Constructs an `UnsupportedRepresentation` diagnostic (semantic range loss).
pub(crate) fn unsupported(
    source: ResolvedSource<'_>,
    start: usize,
    end: usize,
    code: &'static str,
    message: &'static str,
) -> CodecError {
    jqf_codec_core::diagnosed(
        CodecFailureKind::UnsupportedRepresentation,
        YAML,
        source,
        start,
        end,
        code,
        message,
    )
}
