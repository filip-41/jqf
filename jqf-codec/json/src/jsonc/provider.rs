//! The commented-JSON complete-document provider route, shared by JSONC and JSON5: both serve exactly one
//! whole-document route, and differ only in the values [`CommentedSpec`] names.
//!
//! Slot 0 is Whole/`CompleteDocument`. `Located` stays on core's exact fallback: a native Located slot would need a
//! comment-preserving scoped walk (TOML's `build_wrapped_value` rebuild), and the span materializer returns an owned
//! `Value` that cannot carry facts. Core's fallback decodes the whole document (with [`jqf_codec_core::FactIntent`])
//! then locates, so comments stay intact. Do not advertise slot 1 until that walk exists.

use alloc::vec::Vec;

use jqf_codec_core::{
    AccessFootprintKind, AccessGuarantees, AccessResultKind, CodecError, CodecFailureKind, DecodeRequest,
    DiagnosticPolicy, ErasedAccessSession, ErasedProvider, FactIntent, InputProvider, ProviderInput, RouteDescription,
    RouteSlot, required_builder_coverage,
};
use jqf_data::{BuilderCoverage, DataError, DiagnosticCoverage, DocumentSchemaRecipe};
use jqf_resource::ResourceContext;
use jqf_source::ResolvedSource;

use crate::error::data_contract;
use crate::parse::{JSON_NODE_KINDS, JSON_OCCURRENCE_ROLES};
use crate::storage::ParseMode;

use super::{DEFAULT_DIALECT_ID, FORMAT_ID, TRAILING_DIALECT_ID};

/// Stable physical identity of JSONC whole-document decode.
pub(crate) const WHOLE_PHYSICAL_ROUTE_ID: jqf_codec_core::PhysicalRouteId =
    jqf_codec_core::PhysicalRouteId::derive_or_panic(FORMAT_ID, 1, 1);

/// What separates one commented dialect's decode from the other's.
pub(crate) struct CommentedSpec {
    /// The dialect the decoded document names as its own — the request's own dialect, so the identity names what
    /// actually decoded it.
    pub(crate) dialect: &'static str,
    /// Builds this dialect's schema recipe for that dialect text. It stays a per-dialect function because the fact-role
    /// slice it declares must outlive the session it feeds.
    pub(crate) recipe: fn(&'static str) -> Result<DocumentSchemaRecipe<'static>, DataError>,
    /// The `<fmt>.comment@1` role the decode attaches comments under.
    pub(crate) comment_role: &'static str,
    /// Whether the grammar accepts a trailing comma: a JSON5 feature always, a JSONC dialect choice.
    pub(crate) trailing_commas: bool,
    /// Whether the whole JSON5 grammar rides.
    pub(crate) json5: bool,
    /// Whether a leading byte-order mark is consumed as a source prefix. JSONC needs it: its corpus is editor-written
    /// configuration (VS Code's settings.json, anything a Windows editor saved), which is exactly where marks live.
    /// JSON5 accepts U+FEFF as grammar whitespace instead, which leaves a marked document byte-identical to its source.
    pub(crate) consume_bom: bool,
    /// Stable physical identity of this dialect's whole-document decode.
    pub(crate) route: jqf_codec_core::PhysicalRouteId,
}

/// The JSONC schema recipe: strict JSON's value model, plus the `jsonc.comment@1` fact role so the decode's comment
/// attachment has a declared home.
fn jsonc_schema_recipe(dialect: &'static str) -> Result<DocumentSchemaRecipe<'static>, DataError> {
    DocumentSchemaRecipe::try_new(
        FORMAT_ID,
        Some(dialect),
        JSON_NODE_KINDS,
        JSON_OCCURRENCE_ROLES,
        &[crate::parse::COMMENT_FACT],
        &[crate::parse::COMMENT_FACT],
    )
}

/// The diagnostic and builder coverage one bound requirement decodes under. Comment facts arm only when the
/// requirement's [`FactIntent`] is `Preserve`: identity JSONC → JSONC and `--edit` set that at construction, identity
/// JSONC → JSON leaves it `None` so the JSON encoder is not handed facts it cannot emit. A comment-free decode still
/// attaches nothing (the trivia scanner allocates no comment text).
fn decode_shape(requirement: &jqf_codec_core::AccessRequirement) -> (DiagnosticCoverage, BuilderCoverage) {
    let diagnostics = match requirement.diagnostics() {
        DiagnosticPolicy::Off | DiagnosticPolicy::ErrorsOnly => DiagnosticCoverage::NotRequested,
        DiagnosticPolicy::All => DiagnosticCoverage::AuthoritativeEmpty,
    };
    let coverage = required_builder_coverage(requirement);
    let coverage = match requirement.fact_intent() {
        FactIntent::Preserve => coverage.with_attached_facts(true),
        FactIntent::None => coverage,
    };
    (diagnostics, coverage)
}

pub(crate) struct CommentedProvider {
    routes: Vec<RouteDescription>,
    spec: CommentedSpec,
}

impl CommentedProvider {
    fn try_new(
        spec: CommentedSpec,
        diagnostics: DiagnosticPolicy,
        resources: &ResourceContext<'_>,
    ) -> Result<Self, CodecError> {
        let guarantees = AccessGuarantees::strict(diagnostics);
        let rows: Vec<_> = alloc::vec![(
            // Slot 0: the whole-document route. Located stays on core's exact fallback — see the module docs.
            RouteSlot::new(0),
            AccessFootprintKind::Whole,
            AccessResultKind::CompleteDocument,
        )];
        let routes = RouteDescription::try_table(&rows, guarantees, resources)?;
        Ok(Self { routes, spec })
    }
}

impl InputProvider for CommentedProvider {
    fn route_descriptions(&self) -> &[RouteDescription] {
        self.routes.as_slice()
    }

    fn supports_attribute_absence(&self) -> bool {
        true
    }

    fn open_route<'source>(
        &mut self,
        input: ProviderInput<'source>,
        slot: RouteSlot,
        requirement: &jqf_codec_core::AccessRequirement,
        resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedAccessSession<'source>, CodecError> {
        // The sibling slot law (json provider.rs, record_route.rs): a slot this single-route provider does not declare
        // is a ROUTE mismatch, not a representation problem — the request asked the right codec for a route it never
        // advertised.
        if slot != RouteSlot::new(0) {
            return Err(CodecError::new(CodecFailureKind::ProviderRouteMismatch));
        }
        requirement.expect_whole(AccessResultKind::CompleteDocument)?;
        let (diagnostics, coverage) = decode_shape(requirement);
        let spec = &self.spec;
        let recipe = (spec.recipe)(spec.dialect).map_err(|_| data_contract())?;
        let frontier = requirement.lazy_frontier();
        let prune = requirement.prune();
        let canonicality = requirement.canonicality_probe();
        let authored_spans = requirement.authored_spans();
        let materializer = commented_span_materializer(spec.trailing_commas, spec.json5);
        ErasedAccessSession::try_new_source_with_route(input.source(), spec.route, || {
            let mut state = crate::parse::JsonParseState::new(diagnostics, coverage, ParseMode::Document);
            // The DYNAMIC-schema builder: the prototype path builds a shared prepared schema that refuses `add_fact`,
            // and comment facts are the whole point of these formats when Preserve.
            state.with_dynamic_recipe(recipe);
            state.set_comment_fact_role(spec.comment_role);
            state.set_span_materializer(materializer);
            // Comments are always armed — comments ARE the format — and the lenient bit rides the resource dial
            // exactly like strict JSON's.
            state.set_grammar(crate::storage::JsonGrammar {
                comments: true,
                trailing_commas: spec.trailing_commas,
                lenient: resources.decode_lenient(),
                json5: spec.json5,
            });
            if frontier > 0 {
                state.enable_lazy_frontier(frontier as usize);
            } else if let Some(tree) = prune {
                state.enable_prune(tree);
            }
            if canonicality {
                state.set_canonicality_probe(true);
            }
            state.set_authored_spans(authored_spans);
            if spec.consume_bom {
                state.consume_source_bom(input.source());
            }
            Ok(state)
        })
    }
}

fn commented_span_materializer(trailing_commas: bool, json5: bool) -> &'static dyn jqf_data::LazySpanMaterializer {
    match (json5, trailing_commas) {
        (true, _) => &crate::lazy::JSON5_SPAN_MATERIALIZER,
        (false, true) => &crate::lazy::JSONC_TRAILING_SPAN_MATERIALIZER,
        (false, false) => &crate::lazy::JSONC_DEFAULT_SPAN_MATERIALIZER,
    }
}

/// The allocation-free commented-dialect decoder factory.
pub(crate) fn create_commented_provider<'source>(
    source: ResolvedSource<'source>,
    spec: CommentedSpec,
    diagnostics: DiagnosticPolicy,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedProvider<'source>, CodecError> {
    let provider = CommentedProvider::try_new(spec, diagnostics, resources)?;
    ErasedProvider::try_new_provider(source, resources, || Ok(provider))
}

/// The JSONC decoder factory: picks the grammar's trailing-comma bit from the requested dialect.
pub(crate) fn create_provider<'source>(
    source: ResolvedSource<'source>,
    request: DecodeRequest<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedProvider<'source>, CodecError> {
    let (dialect, trailing_commas) = match request.dialect.as_str() {
        TRAILING_DIALECT_ID => (TRAILING_DIALECT_ID, true),
        DEFAULT_DIALECT_ID => (DEFAULT_DIALECT_ID, false),
        _ => return Err(CodecError::new(CodecFailureKind::RequirementMismatch)),
    };
    create_commented_provider(
        source,
        CommentedSpec {
            dialect,
            recipe: jsonc_schema_recipe,
            comment_role: crate::parse::COMMENT_FACT,
            trailing_commas,
            json5: false,
            consume_bom: true,
            route: WHOLE_PHYSICAL_ROUTE_ID,
        },
        request.diagnostics,
        resources,
    )
}
