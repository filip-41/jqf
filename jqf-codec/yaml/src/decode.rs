//! Non-parsing YAML provider factories.

use jqf_codec_core::{CodecError, CodecFailureKind, DecodeRequest, ErasedProvider};
use jqf_resource::ResourceContext;
use jqf_source::ResolvedSource;

use crate::provider::{DialectKind, YamlProvider};

/// The ONE decoder entry point: the registration carries every dialect, and the factory dispatches on the request's
/// dialect. An output-profile dialect (a decode can never target one) is a requirement mismatch.
pub(crate) fn create_provider<'source>(
    source: ResolvedSource<'source>,
    request: DecodeRequest<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedProvider<'source>, CodecError> {
    request.expect_strict_defaults()?;
    let kind = match request.dialect.as_str() {
        crate::YAML_FAILSAFE_DIALECT_ID => DialectKind::Failsafe,
        crate::YAML_JSON_DIALECT_ID => DialectKind::Json,
        crate::YAML_CORE_DIALECT_ID => DialectKind::Core,
        _ => return Err(CodecError::new(CodecFailureKind::RequirementMismatch)),
    };
    // A YAML source is a STREAM of documents (§4.8), not one document: every decode publishes one document and reports
    // its `consumed_offset`, and the SDK's reopen-at-offset sequence drive walks the rest — one SDK item per `---`
    // unit, in source order.
    let provider = YamlProvider::try_new(request.diagnostics, kind, resources)?;
    ErasedProvider::try_new_provider(source, resources, || Ok(provider))
}
