//! Non-parsing CBOR provider factories.

use jqf_codec_core::{CodecError, DecodeRequest, ErasedProvider};
use jqf_resource::ResourceContext;
use jqf_source::ResolvedSource;

use crate::provider::CborProvider;

/// Builds the generic-dialect CBOR provider for a decode request.
pub(crate) fn create_provider<'source>(
    source: ResolvedSource<'source>,
    request: DecodeRequest<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedProvider<'source>, CodecError> {
    request.expect_strict_defaults()?;
    // The adjacent-value opt-in: a cbor-seq request decodes ONE item and reports its real consumed offset, so the drive
    // advances by exactly the item and decodes the remainder as the next item. The single-document path (the ordinary
    // `cbor` format) keeps today's trailing-byte rejection: the flag is simply never set for it.
    let provider = CborProvider::try_new(request.diagnostics, request.allow_adjacent_values, resources)?;
    ErasedProvider::try_new_provider(source, resources, || Ok(provider))
}
