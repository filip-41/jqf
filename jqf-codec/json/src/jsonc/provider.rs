//! The commented-JSON provider route, shared by JSONC and JSON5: both advertise Whole and Exact, and differ only in the
//! values [`CommentedSpec`] names.
//!
//! Slot 0 is Whole/`CompleteDocument`. Slot 1 is Exact/`Located`: the strict-JSON [`crate::scoped::ScopedSession`] with
//! this dialect's grammar (comments, trailing commas, JSON5 extras). Validation still walks every byte. Count and
//! element Exact publish the last-wins span as a lazy container root. Print rematerialize of the located span still
//! goes through [`crate::parse::JsonParseState`] with comment facts when
//! [`jqf_data::BuilderCoverage::attached_facts`] is on; leading comments that sit before a selected member's key are
//! collected during validate and seeded onto the materializer so Preserve / `.@comment` keep them. Identity Exact still
//! validates unread bytes (trailing trivia is skipped; trailing values are not).
//!
//! Lazy Whole walks re-parse a deferred span through [`crate::lazy::CommentedSpanMaterializer`]. When
//! [`jqf_data::BuilderCoverage::attached_facts`] is on (Preserve / `.@comment`), that re-parse attaches leading comments
//! as `<fmt>.comment@1` list-of-texts, matching [`crate::parse::JsonParseState`].

use alloc::vec::Vec;

use jqf_codec_core::{
    AccessGuarantees, AccessRequirement, AccessResultKind, CodecError, CodecFailureKind, DecodeRequest,
    DiagnosticPolicy, ErasedAccessSession, ErasedProvider, FactIntent, InputProvider, ProviderInput,
    RecycledSessionState, RouteDescription, RouteSlot, required_builder_coverage,
};
use jqf_data::{BuilderCoverage, DataError, DiagnosticCoverage, DocumentSchemaRecipe};
use jqf_resource::ResourceContext;
use jqf_source::ResolvedSource;

use crate::error::data_contract;
use crate::parse::{JSON_NODE_KINDS, JSON_OCCURRENCE_ROLES};
use crate::scoped::CommentedScope;
use crate::storage::{JsonGrammar, ParseMode};

use super::{DEFAULT_DIALECT_ID, FORMAT_ID, TRAILING_DIALECT_ID};

/// Stable physical identity of JSONC whole-document decode.
pub(crate) const WHOLE_PHYSICAL_ROUTE_ID: jqf_codec_core::PhysicalRouteId =
    jqf_codec_core::PhysicalRouteId::derive_or_panic(FORMAT_ID, 1, 1);

/// Stable physical identity of JSONC scoped exact-path decode.
pub(crate) const SCOPED_PHYSICAL_ROUTE_ID: jqf_codec_core::PhysicalRouteId =
    jqf_codec_core::PhysicalRouteId::derive_or_panic(FORMAT_ID, 2, 1);

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
    /// Stable physical identity of this dialect's scoped exact-path decode.
    pub(crate) scoped_route: jqf_codec_core::PhysicalRouteId,
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
        let routes = RouteDescription::try_standard_document_table(guarantees, resources)?;
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

    #[allow(
        clippy::too_many_lines,
        reason = "one dispatch per advertised slot: the route table is read as a single unit"
    )]
    fn open_route<'source>(
        &mut self,
        input: ProviderInput<'source>,
        slot: RouteSlot,
        requirement: &AccessRequirement,
        resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedAccessSession<'source>, CodecError> {
        let (diagnostics, coverage) = decode_shape(requirement);
        let spec = &self.spec;
        if slot == RouteSlot::new(0) {
            requirement.expect_whole(AccessResultKind::CompleteDocument)?;
            let recipe = (spec.recipe)(spec.dialect).map_err(|_| data_contract())?;
            let frontier = requirement.lazy_frontier();
            let prune = requirement.prune();
            let canonicality = requirement.canonicality_probe();
            let authored_spans = requirement.authored_spans();
            let materializer = commented_span_materializer(spec.trailing_commas, spec.json5, coverage.attached_facts());
            let grammar = commented_grammar(spec, resources);
            return ErasedAccessSession::try_new_source_with_route(input.source(), spec.route, || {
                let mut state = crate::parse::JsonParseState::new(diagnostics, coverage, ParseMode::Document);
                // The DYNAMIC-schema builder: the prototype path builds a shared prepared schema that refuses `add_fact`,
                // and comment facts are the whole point of these formats when Preserve.
                state.with_dynamic_recipe(recipe);
                state.set_comment_fact_role(spec.comment_role);
                state.set_span_materializer(materializer);
                state.set_grammar(grammar);
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
            });
        }
        if slot != RouteSlot::new(1) {
            return Err(CodecError::new(CodecFailureKind::ProviderRouteMismatch));
        }
        let (path, origin) = requirement.expect_exact(AccessResultKind::Located)?;
        let scope = CommentedScope {
            grammar: commented_grammar(spec, resources),
            comment_role: spec.comment_role,
            recipe: spec.recipe,
            dialect: spec.dialect,
            span_materializer: commented_span_materializer(spec.trailing_commas, spec.json5, coverage.attached_facts()),
            consume_bom: spec.consume_bom,
        };
        let mut session = crate::scoped::ScopedSession::try_new_commented(
            path.steps(),
            origin,
            diagnostics,
            coverage,
            scope,
            resources,
        )?;
        session.arm_requirement(requirement, resources)?;
        ErasedAccessSession::try_new_source_with_route(input.source(), spec.scoped_route, || Ok(session))
    }

    fn try_reopen_route(
        &mut self,
        state: &mut RecycledSessionState<'_>,
        slot: RouteSlot,
        requirement: &AccessRequirement,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        let (diagnostics, coverage) = decode_shape(requirement);
        if slot == RouteSlot::new(0) {
            if !requirement.footprint().is_whole()
                || !requirement.schedule().is_empty_complete()
                || requirement.result() != AccessResultKind::CompleteDocument
            {
                return Ok(false);
            }
            let Some(parse) = state.downcast_mut::<crate::parse::JsonParseState>() else {
                return Ok(false);
            };
            parse.reset(diagnostics, coverage, ParseMode::Document);
            parse.set_span_materializer(commented_span_materializer(
                self.spec.trailing_commas,
                self.spec.json5,
                coverage.attached_facts(),
            ));
            if requirement.lazy_frontier() == 0
                && let Some(tree) = requirement.prune()
            {
                parse.enable_prune(tree);
            }
            parse.set_canonicality_probe(requirement.canonicality_probe());
            parse.set_authored_spans(requirement.authored_spans());
            return Ok(true);
        }
        if slot != RouteSlot::new(1)
            || requirement.footprint().is_whole()
            || requirement.schedule().is_empty_complete()
            || requirement.result() != AccessResultKind::Located
        {
            return Ok(false);
        }
        let (Some(path), Some(origin)) = (
            requirement.footprint().exact_path(),
            requirement.schedule().singleton_origin(),
        ) else {
            return Ok(false);
        };
        let Some(scoped) = state.downcast_mut::<crate::scoped::ScopedSession>() else {
            return Ok(false);
        };
        if !scoped.try_reset(path.steps(), origin, diagnostics, coverage, false) {
            return Ok(false);
        }
        scoped.arm_requirement(requirement, resources)?;
        Ok(true)
    }
}

fn commented_grammar(spec: &CommentedSpec, resources: &ResourceContext<'_>) -> JsonGrammar {
    JsonGrammar {
        comments: true,
        trailing_commas: spec.trailing_commas,
        lenient: resources.decode_lenient(),
        json5: spec.json5,
    }
}

fn commented_span_materializer(
    trailing_commas: bool,
    json5: bool,
    attach_facts: bool,
) -> &'static dyn jqf_data::LazySpanMaterializer {
    match (json5, trailing_commas, attach_facts) {
        (true, _, true) => &crate::lazy::JSON5_SPAN_MATERIALIZER_FACTS,
        (true, _, false) => &crate::lazy::JSON5_SPAN_MATERIALIZER,
        (false, true, true) => &crate::lazy::JSONC_TRAILING_SPAN_MATERIALIZER_FACTS,
        (false, true, false) => &crate::lazy::JSONC_TRAILING_SPAN_MATERIALIZER,
        (false, false, true) => &crate::lazy::JSONC_DEFAULT_SPAN_MATERIALIZER_FACTS,
        (false, false, false) => &crate::lazy::JSONC_DEFAULT_SPAN_MATERIALIZER,
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
            scoped_route: SCOPED_PHYSICAL_ROUTE_ID,
        },
        request.diagnostics,
        resources,
    )
}
