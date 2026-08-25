//! Flat-config decoder provider factories.
//!
//! Three entries, one per sealed grammar. Each refuses adjacent values, requires the grammar's input dialect, and keeps
//! strict defaults.

use jqf_codec_core::{CodecError, CodecFailureKind, DecodeRequest, ErasedProvider};
use jqf_resource::ResourceContext;
use jqf_source::ResolvedSource;

use crate::options::Grammar;
use crate::provider::FlatProvider;

/// Shared body of the three grammar-specific decoder entry points: the request must name this grammar's input dialect,
/// keep strict defaults, and refuse the adjacent-value contract (flat config is one document per source).
fn create_provider_for<'source>(
    source: ResolvedSource<'source>,
    request: DecodeRequest<'_>,
    resources: &mut ResourceContext<'_>,
    grammar: Grammar,
) -> Result<ErasedProvider<'source>, CodecError> {
    request.expect_strict_defaults()?;
    if request.allow_adjacent_values {
        return Err(CodecError::new(CodecFailureKind::RequirementMismatch));
    }
    if request.dialect.as_str() != grammar.input_dialect_id() {
        return Err(CodecError::new(CodecFailureKind::RequirementMismatch));
    }
    let provider = FlatProvider::try_new(grammar, request.diagnostics, resources)?;
    ErasedProvider::try_new_provider(source, resources, || Ok(provider))
}

/// The `properties.jdk@1` decoder entry point.
pub(crate) fn create_properties_provider<'source>(
    source: ResolvedSource<'source>,
    request: DecodeRequest<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedProvider<'source>, CodecError> {
    create_provider_for(source, request, resources, Grammar::Properties)
}

/// The `ini.jqf-strict@1` decoder entry point.
pub(crate) fn create_ini_provider<'source>(
    source: ResolvedSource<'source>,
    request: DecodeRequest<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedProvider<'source>, CodecError> {
    create_provider_for(source, request, resources, Grammar::Ini)
}

/// The `dotenv.jqf-strict@1` decoder entry point.
pub(crate) fn create_dotenv_provider<'source>(
    source: ResolvedSource<'source>,
    request: DecodeRequest<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedProvider<'source>, CodecError> {
    create_provider_for(source, request, resources, Grammar::Dotenv)
}
