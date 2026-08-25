//! TOML `InvalidInput` diagnostics in the `toml` namespace.
//!
//! Sibling: [`jqf_codec_core::CodecError`].

use jqf_codec_core::{CodecError, CodecFailureKind};
use jqf_source::{Namespace, ResolvedSource};

const TOML: Namespace = Namespace::new("toml");

/// Constructs an `InvalidInput` reject diagnostic in the `toml` namespace.
pub(crate) fn invalid(
    source: ResolvedSource<'_>,
    offset: usize,
    code: &'static str,
    message: &'static str,
) -> CodecError {
    jqf_codec_core::diagnosed(
        CodecFailureKind::InvalidInput,
        TOML,
        source,
        offset,
        offset.saturating_add(usize::from(offset < source.bytes().len())),
        code,
        message,
    )
}
