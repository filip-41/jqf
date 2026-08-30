//! Strict-JSON complete-document provider: bind, open, recycle.
//!
//! Hands the requirement to [`crate::parse::JsonParseState`]. JSONC/JSON5 have their own provider modules.

use alloc::vec::Vec;

use jqf_codec_core::{
    AccessFootprintKind, AccessGuarantees, AccessRequirement, AccessResultKind, CodecError, CodecFailureKind,
    DiagnosticPolicy, ErasedAccessSession, InputProvider, ProviderInput, RecycledSessionState, RouteDescription,
    RouteSlot, required_builder_coverage,
};
use jqf_data::{BuilderCoverage, DiagnosticCoverage, DocumentSchemaPrototype};
use jqf_resource::ResourceContext;

use crate::storage::ParseMode;

pub(crate) struct JsonProvider {
    routes: Vec<RouteDescription>,
    allow_adjacent_values: bool,
    schema_prototype: Option<DocumentSchemaPrototype>,
}

impl JsonProvider {
    pub(crate) fn try_new(
        diagnostics: DiagnosticPolicy,
        allow_adjacent_values: bool,
        resources: &ResourceContext<'_>,
    ) -> Result<Self, CodecError> {
        let guarantees = AccessGuarantees::strict(diagnostics);
        let rows: alloc::vec::Vec<_> = alloc::vec![
            // Slot 0: the whole-document route.
            (
                RouteSlot::new(0),
                AccessFootprintKind::Whole,
                AccessResultKind::CompleteDocument,
            ),
            // Slot 1: the scoped exact-path route. Its demand names exactly the minimal semantic capabilities it can
            // materialize from a subtree; the core binder Direct-binds it only for Located requirements whose demand
            // fits within this ceiling, and routes anything richer (topology/source/provenance) to the whole route +
            // generic exact adapter exactly as before.
            (RouteSlot::new(1), AccessFootprintKind::Exact, AccessResultKind::Located,),
        ];
        let routes = RouteDescription::try_table(&rows, guarantees, resources)?;
        Ok(Self {
            routes,
            allow_adjacent_values,
            schema_prototype: None,
        })
    }

    /// What the whole-document route's parser is decoding: one value of a caller-framed adjacent stream, or one
    /// document occupying its source.
    const fn parse_mode(&self) -> ParseMode {
        if self.allow_adjacent_values {
            ParseMode::AdjacentValue
        } else {
            ParseMode::Document
        }
    }

    fn try_schema_prototype(&mut self, resources: &ResourceContext<'_>) -> Result<DocumentSchemaPrototype, CodecError> {
        if self.schema_prototype.is_none() {
            self.schema_prototype = Some(crate::parse::try_schema_prototype(resources)?);
        }
        self.schema_prototype
            .as_ref()
            .ok_or_else(crate::error::data_contract)
            .cloned()
    }
}

impl InputProvider for JsonProvider {
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
        // Decode only the coverage the bound requirement demanded: the identity root vertical asks for semantic
        // root+shape and so decodes minimal (topology/facts/provenance omitted), while richer requirements keep
        // requesting the complete document.
        let (diagnostics, coverage) = decode_shape(requirement);
        if slot == RouteSlot::new(0) {
            requirement.expect_whole(AccessResultKind::CompleteDocument)?;
            let schema_prototype = self.try_schema_prototype(resources)?;
            let mode = self.parse_mode();
            // The lazy frontier travels WITH the requirement (the engine's root lowering set it from the program class
            // or the policy override), and is offered ONLY here: the whole-document route is the one that builds the
            // entire document up front, so it is the only route a deferral can save anything on. The scoped route
            // already materializes a fraction of the input and stays exactly as it was — and carries 0 by
            // construction.
            let frontier = requirement.lazy_frontier();
            // The prune HINT also rides the requirement and is only honored here, on the eager whole-document build (a
            // deferral already skips materialization wholesale, so the two never mix).
            let prune = requirement.prune();
            return ErasedAccessSession::try_new_source_with_route(
                input.source(),
                crate::FULL_PHYSICAL_ROUTE_ID,
                || {
                    let mut state = crate::parse::JsonParseState::with_schema_prototype(
                        schema_prototype,
                        diagnostics,
                        coverage,
                        mode,
                    );
                    // The strict grammar's leniency comes from the resource dial, read HERE at each session build: this
                    // closure runs per bind, so it consults the request's LIVE context and nothing is frozen into the
                    // provider at construction. JSONC's own provider arms comments and trailing commas instead.
                    state.set_grammar(crate::storage::JsonGrammar {
                        lenient: resources.decode_lenient(),
                        ..crate::storage::JsonGrammar::STRICT
                    });
                    if frontier > 0 {
                        state.enable_lazy_frontier(frontier as usize);
                    } else if let Some(tree) = prune {
                        state.enable_prune(tree);
                    }
                    if requirement.canonicality_probe() {
                        state.set_canonicality_probe(true);
                    }
                    state.set_authored_spans(requirement.authored_spans());
                    // The adjacent-value BOM law: a UTF-8 byte-order mark is consumed before the FIRST value, and a
                    // whole-document route keeps strict JSON's rejection of one. The NDJSON record codec keeps its own
                    // opposite profile law.
                    if mode == ParseMode::AdjacentValue {
                        state.consume_source_bom(input.source());
                    }
                    Ok(state)
                },
            );
        }
        // Validation is served by the ordinary whole-document slot: a record-independent program binds the lazy whole
        // document, which validates to the full parser's strictness and never materializes when nothing reads it.
        if slot != RouteSlot::new(1) {
            return Err(CodecError::new(CodecFailureKind::ProviderRouteMismatch));
        }
        let (path, origin) = requirement.expect_exact(AccessResultKind::Located)?;
        let steps = path.steps();
        let schema_prototype = self.try_schema_prototype(resources)?;
        let mut session = crate::scoped::ScopedSession::try_new_with_schema_prototype(
            steps,
            origin,
            diagnostics,
            coverage,
            self.allow_adjacent_values,
            schema_prototype,
            resources,
        )?;
        if let Some(tree) = requirement.prune() {
            session.arm_prune(
                tree.try_clone_in(resources)
                    .map_err(|_| crate::error::data_contract())?,
            );
        }
        session.set_type_demand(requirement.type_demand());
        ErasedAccessSession::try_new_source_with_route(input.source(), crate::SCOPED_PHYSICAL_ROUTE_ID, || Ok(session))
    }

    fn try_reopen_route(
        &mut self,
        state: &mut RecycledSessionState<'_>,
        slot: RouteSlot,
        requirement: &AccessRequirement,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        let (diagnostics, coverage) = decode_shape(requirement);
        // Slot 0 decodes a whole document; slot 1 locates one exact path. Both reset in place. Those two slots ARE the
        // codec's entire access inventory; record framing is not an access observation and opens through the
        // record-stream provider, never through this binder, so there is no other slot to reopen.
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
            parse.reset(diagnostics, coverage, self.parse_mode());
            // Reset cleared the previous requirement's prune hint; re-arm EVERY requirement-scoped hint from the NEW
            // requirement, under the same conditions as the bind. A recycled session reopened under a different-shaped
            // requirement must not keep the old one's flags: a stale `authored_spans = false` silently drops the span
            // table an edit lane needs, and a stale canonicality probe answers a verdict its new caller never asked
            // for.
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
        if !scoped.try_reset(path.steps(), origin, diagnostics, coverage, self.allow_adjacent_values) {
            return Ok(false);
        }
        scoped.set_type_demand(requirement.type_demand());
        Ok(true)
    }
}

/// The diagnostic and builder coverage one bound requirement decodes under.
fn decode_shape(requirement: &AccessRequirement) -> (DiagnosticCoverage, BuilderCoverage) {
    let diagnostics = match requirement.diagnostics() {
        DiagnosticPolicy::Off | DiagnosticPolicy::ErrorsOnly => DiagnosticCoverage::NotRequested,
        DiagnosticPolicy::All => DiagnosticCoverage::AuthoritativeEmpty,
    };
    (diagnostics, required_builder_coverage(requirement))
}
