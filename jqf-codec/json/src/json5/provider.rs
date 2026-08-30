//! The JSON5 complete-document provider route: the commented-JSON provider with the whole JSON5 grammar armed.

use jqf_codec_core::{CodecError, CodecFailureKind, DecodeRequest, ErasedProvider};
use jqf_data::{DataError, DocumentSchemaRecipe};
use jqf_resource::ResourceContext;
use jqf_source::ResolvedSource;

use crate::jsonc::provider::{CommentedSpec, create_commented_provider};
use crate::parse::{JSON_NODE_KINDS, JSON_OCCURRENCE_ROLES, JSON5_COMMENT_FACT};

use super::{DOCUMENT_DIALECT_ID, FORMAT_ID};

/// The JSON5 comment-fact role: the JSONC shape, swapped namespace. The codec-agnostic matcher strips the prefix and
/// serves `.@comment` with zero SDK change.
pub(crate) const COMMENT_FACT_ROLE: &str = JSON5_COMMENT_FACT;

/// Stable physical identity of JSON5 whole-document decode.
pub(crate) const WHOLE_PHYSICAL_ROUTE_ID: jqf_codec_core::PhysicalRouteId =
    jqf_codec_core::PhysicalRouteId::derive_or_panic(FORMAT_ID, 1, 1);

/// Stable physical identity of JSON5 scoped exact-path decode.
pub(crate) const SCOPED_PHYSICAL_ROUTE_ID: jqf_codec_core::PhysicalRouteId =
    jqf_codec_core::PhysicalRouteId::derive_or_panic(FORMAT_ID, 2, 1);

/// The JSON5 schema recipe: strict JSON's value model, plus the `json5.comment@1` fact role so the decode's comment
/// attachment has a declared home.
fn json5_schema_recipe(dialect: &'static str) -> Result<DocumentSchemaRecipe<'static>, DataError> {
    DocumentSchemaRecipe::try_new(
        FORMAT_ID,
        Some(dialect),
        JSON_NODE_KINDS,
        JSON_OCCURRENCE_ROLES,
        &[COMMENT_FACT_ROLE],
        &[COMMENT_FACT_ROLE],
    )
}

/// The allocation-free JSON5 decoder factory: the grammar is the one json5 dialect's, so the request's dialect must
/// name it.
pub(crate) fn create_provider<'source>(
    source: ResolvedSource<'source>,
    request: DecodeRequest<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedProvider<'source>, CodecError> {
    if request.dialect.as_str() != DOCUMENT_DIALECT_ID {
        return Err(CodecError::new(CodecFailureKind::RequirementMismatch));
    }
    create_commented_provider(
        source,
        CommentedSpec {
            dialect: DOCUMENT_DIALECT_ID,
            recipe: json5_schema_recipe,
            comment_role: COMMENT_FACT_ROLE,
            // Comments AND trailing commas are both JSON5 grammar.
            trailing_commas: true,
            json5: true,
            // U+FEFF is JSON5 whitespace, so the grammar accepts a marked document without a source-prefix rule.
            consume_bom: false,
            route: WHOLE_PHYSICAL_ROUTE_ID,
            scoped_route: SCOPED_PHYSICAL_ROUTE_ID,
        },
        request.diagnostics,
        resources,
    )
}
