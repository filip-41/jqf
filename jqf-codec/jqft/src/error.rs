//! Structured source-aware jqft diagnostics.

use jqf_codec_core::{CodecError, CodecFailureKind};
use jqf_source::{Namespace, ResolvedSource};

const JQFT: Namespace = Namespace::new("jqft");

/// Constructs an `InvalidInput` reject diagnostic in the `jqft` namespace.
pub(crate) fn invalid(
    source: ResolvedSource<'_>,
    offset: usize,
    code: &'static str,
    message: &'static str,
) -> CodecError {
    jqf_codec_core::diagnosed(
        CodecFailureKind::InvalidInput,
        JQFT,
        source,
        offset,
        offset.saturating_add(usize::from(offset < source.bytes().len())),
        code,
        message,
    )
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
        JQFT,
        source,
        start,
        end,
        code,
        message,
    )
}

pub(crate) fn data_contract() -> CodecError {
    jqf_codec_core::data_contract("jqft authoritative document construction")
}
