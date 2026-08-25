//! Non-parsing XML provider factory.

use jqf_codec_core::{CodecError, DecodeRequest, ErasedProvider};
use jqf_resource::ResourceContext;
use jqf_source::ResolvedSource;

use crate::provider::XmlProvider;

/// The decoder factory entry point (`decode::create_provider`). XML accepts
/// no decode options.
pub(crate) fn create_provider<'source>(
    source: ResolvedSource<'source>,
    request: DecodeRequest<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedProvider<'source>, CodecError> {
    request.expect_strict_defaults()?;
    let provider = XmlProvider::try_new(request.diagnostics, resources)?;
    ErasedProvider::try_new_provider(source, resources, || Ok(provider))
}
