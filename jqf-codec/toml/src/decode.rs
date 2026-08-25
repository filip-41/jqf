//! Decoder factories that build a [`crate::provider::TomlProvider`].
//!
//! One factory per input dialect. Sibling: [`crate::provider`].

use jqf_codec_core::{CodecError, CodecFailureKind, DecodeRequest, ErasedProvider};
use jqf_resource::ResourceContext;
use jqf_source::ResolvedSource;

use crate::provider::{DialectKind, TomlProvider};

/// Shared body of the two dialect-specific decoder entry points.
fn create_provider_for<'source>(
    source: ResolvedSource<'source>,
    request: DecodeRequest<'_>,
    resources: &mut ResourceContext<'_>,
    dialect: DialectKind,
) -> Result<ErasedProvider<'source>, CodecError> {
    request.expect_strict_defaults()?;
    // TOML is a single-document format: a request must not treat the source as a stream of adjacent texts. The decoder
    // factory is only ever reached through a catalog for a `toml/*` request; the allow-adjacent flag must be off, and a
    // caller that sets it has named the wrong drive.
    if request.allow_adjacent_values {
        return Err(CodecError::new(CodecFailureKind::RequirementMismatch));
    }
    let provider = TomlProvider::try_new(request.diagnostics, dialect, resources)?;
    ErasedProvider::try_new_provider(source, resources, || Ok(provider))
}

pub(crate) fn create_provider_1_0<'source>(
    source: ResolvedSource<'source>,
    request: DecodeRequest<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedProvider<'source>, CodecError> {
    create_provider_for(source, request, resources, DialectKind::Toml10)
}

pub(crate) fn create_provider_1_1<'source>(
    source: ResolvedSource<'source>,
    request: DecodeRequest<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedProvider<'source>, CodecError> {
    create_provider_for(source, request, resources, DialectKind::Toml11)
}
