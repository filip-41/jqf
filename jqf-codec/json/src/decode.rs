//! Non-parsing strict-JSON provider factory.

use jqf_codec_core::{CodecError, DecodeRequest, ErasedProvider};
use jqf_resource::ResourceContext;
use jqf_source::ResolvedSource;

pub(crate) fn create_provider<'source>(
    source: ResolvedSource<'source>,
    request: DecodeRequest<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedProvider<'source>, CodecError> {
    request.expect_strict_defaults()?;
    let provider =
        crate::provider::JsonProvider::try_new(request.diagnostics, request.allow_adjacent_values, resources)?;
    ErasedProvider::try_new_provider(source, resources, || Ok(provider))
}
