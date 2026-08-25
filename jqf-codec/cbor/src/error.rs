//! Structured source-aware CBOR diagnostics.
//!
//! CBOR is byte-oriented, so diagnostics carry byte offsets (the source has no line structure to speak of). Every
//! failure kind follows the codec crate vocabulary: `InvalidInput` for structural/generic-validity rejects,
//! `UnsupportedRepresentation` for semantic range loss, and `InternalContractViolation` for builder rejects.
//!
//! The whole-route decoder reports bare codec errors; these source-aware helpers are wired into the decoder's error
//! paths.

use jqf_codec_core::{CodecError, CodecFailureKind};
use jqf_source::{Namespace, ResolvedSource};

const CBOR: Namespace = Namespace::new("cbor");

/// Constructs an `InvalidInput` reject diagnostic in the `cbor` namespace.
pub(crate) fn invalid(
    source: ResolvedSource<'_>,
    offset: usize,
    code: &'static str,
    message: &'static str,
) -> CodecError {
    jqf_codec_core::diagnosed(
        CodecFailureKind::InvalidInput,
        CBOR,
        source,
        offset,
        offset.saturating_add(usize::from(offset < source.bytes().len())),
        code,
        message,
    )
}
