//! CBOR decoder provider and access session.
//!
//! Slot 0 is Whole/`CompleteDocument`; slot 1 is Exact/`Located`. The whole
//! route's plan (count-only vs lazy vs eager+prune) lives in
//! [`jqf_codec_core::whole_document_open_plan`].

use jqf_codec_core::{
    AccessGuarantees, AccessRequirement, AccessResultKind, CodecError, DiagnosticPolicy, ErasedAccessSession,
    InputProvider, ProviderInput, RecycledSessionState, RouteDescription, RouteSlot, whole_document_open_plan,
};
use jqf_resource::ResourceContext;

use alloc::vec::Vec;

pub(crate) struct CborProvider {
    routes: Vec<RouteDescription>,
    /// The adjacent-value opt-in: a request that names it decodes ONE item that reports its real consumed offset
    /// instead of rejecting every byte after the item as trailing content.
    adjacent: bool,
}

impl CborProvider {
    pub(crate) fn try_new(
        diagnostics: DiagnosticPolicy,
        allow_adjacent_values: bool,
        resources: &ResourceContext<'_>,
    ) -> Result<Self, CodecError> {
        // The standard two-slot document inventory (whole + located), mirroring jqf-codec-toml: slot 0 validates and
        // builds the whole document; slot 1 walks the wire to the target path and re-decodes only the located span.
        let guarantees = AccessGuarantees::strict(diagnostics);
        let routes = RouteDescription::try_standard_document_table(guarantees, resources)?;
        Ok(Self {
            routes,
            adjacent: allow_adjacent_values,
        })
    }
}

impl InputProvider for CborProvider {
    fn route_descriptions(&self) -> &[RouteDescription] {
        self.routes.as_slice()
    }

    fn open_route<'source>(
        &mut self,
        input: ProviderInput<'source>,
        slot: RouteSlot,
        requirement: &AccessRequirement,
        resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedAccessSession<'source>, CodecError> {
        let coverage = jqf_codec_core::required_builder_coverage_with(
            requirement,
            jqf_codec_core::CoveragePolicy::WholeRouteRichComplete,
        );
        let source = input.source();
        if slot == RouteSlot::new(0) {
            requirement.expect_whole(AccessResultKind::CompleteDocument)?;
            let (lazy_frontier, count_only, prune) = whole_document_open_plan(requirement);
            let mut state =
                crate::parse::CborParseState::try_new(source, coverage, lazy_frontier, self.adjacent, resources)?;
            if count_only {
                state.enable_count_only();
            } else if lazy_frontier.is_none() {
                state.set_prune(prune);
            }
            return ErasedAccessSession::try_new_source_with_route(source, crate::FULL_PHYSICAL_ROUTE_ID, || Ok(state));
        }
        if slot == RouteSlot::new(1) {
            let (path, origin) = requirement.expect_exact(AccessResultKind::Located)?;
            let prune = requirement.prune().and_then(crate::parse::PruneLookup::from_transport);
            let session = crate::routes::NativeLocatedSession::try_new(
                path.steps(),
                origin,
                self.adjacent,
                prune,
                requirement.type_demand(),
            )?;
            return ErasedAccessSession::try_new_source_with_route(source, crate::SCOPED_PHYSICAL_ROUTE_ID, || {
                Ok(session)
            });
        }
        Err(CodecError::new(jqf_codec_core::CodecFailureKind::ProviderRouteMismatch))
    }

    fn try_reopen_route(
        &mut self,
        state: &mut RecycledSessionState<'_>,
        slot: RouteSlot,
        requirement: &AccessRequirement,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        if slot == RouteSlot::new(0) {
            if !requirement.footprint().is_whole()
                || !requirement.schedule().is_empty_complete()
                || requirement.result() != AccessResultKind::CompleteDocument
            {
                return Ok(false);
            }
            let Some(parse) = state.downcast_mut::<crate::parse::CborParseState>() else {
                return Ok(false);
            };
            // The retention profile is derived from the frontier/count hints at open; a reopen whose hint derives the
            // OTHER profile must not recycle this session — answers would stay correct, but span-backed, count-only,
            // and eager decodes are different retention profiles, so decline and let the binder open fresh.
            let (lazy_frontier, count_only, prune) = whole_document_open_plan(requirement);
            if lazy_frontier.is_some() != parse.retain_source() || count_only != parse.count_only() {
                return Ok(false);
            }
            parse.set_prune(prune);
            parse.try_reset(resources)?;
            return Ok(true);
        }
        if slot != RouteSlot::new(1)
            || requirement.footprint().is_whole()
            || requirement.schedule().is_empty_complete()
            || requirement.result() != AccessResultKind::Located
        {
            return Ok(false);
        }
        let (path, origin) = requirement.expect_exact(AccessResultKind::Located)?;
        let Some(located) = state.downcast_mut::<crate::routes::NativeLocatedSession>() else {
            return Ok(false);
        };
        let prune = requirement.prune().and_then(crate::parse::PruneLookup::from_transport);
        located.try_reset(path.steps(), origin, self.adjacent, prune, requirement.type_demand())
    }
}
