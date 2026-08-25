//! jqft-family decoder provider factories.

use jqf_codec_core::{CodecError, DecodeRequest, ErasedProvider};
use jqf_resource::ResourceContext;
use jqf_source::ResolvedSource;

use crate::provider::{JqftKind, JqftProvider};

pub(crate) fn create_jqft_provider<'source>(
    source: ResolvedSource<'source>,
    request: DecodeRequest<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedProvider<'source>, CodecError> {
    create_provider(source, request, JqftKind::Jqft, resources)
}

pub(crate) fn create_jqfjson_provider<'source>(
    source: ResolvedSource<'source>,
    request: DecodeRequest<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedProvider<'source>, CodecError> {
    create_provider(source, request, JqftKind::Jqfjson, resources)
}

fn create_provider<'source>(
    source: ResolvedSource<'source>,
    request: DecodeRequest<'_>,
    kind: JqftKind,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedProvider<'source>, CodecError> {
    request.expect_strict_defaults()?;
    let provider = JqftProvider::try_new(request.diagnostics, kind, request.allow_adjacent_values, resources)?;
    ErasedProvider::try_new_provider(source, resources, || Ok(provider))
}
