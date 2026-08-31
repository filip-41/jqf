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

    fn supports_attribute_absence(&self) -> bool {
        true
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
                requirement.located_skeleton(),
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
        located.try_reset(
            path.steps(),
            origin,
            self.adjacent,
            prune,
            requirement.type_demand(),
            requirement.located_skeleton(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jqf_codec_core::{
        AccessAdapter, AccessFootprint, AccessGuarantees, AccessRequirement, CodecDemand, CodecRunContext,
        DemandClause, DiagnosticPolicy, ErasedProvider, ExactPath, ValidationMode,
    };
    use jqf_data::ExpandedName;
    use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
    use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

    fn resources() -> ResourceContext<'static> {
        ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                .expect("account"),
            &ContinueControl,
            WorkMeter::try_new_v1(4_096).expect("work"),
        )
        .expect("resources")
    }

    fn source(bytes: &[u8]) -> ResolvedSource<'_> {
        ResolvedSource::new(
            SourceRef::new(SourceId::new(1), SourceKind::Input),
            "test.cbor",
            bytes,
            0,
        )
    }

    fn decode_request() -> jqf_codec_core::DecodeRequest<'static> {
        let dialect: &'static jqf_data::DialectId = alloc::boxed::Box::leak(alloc::boxed::Box::new(
            jqf_data::DialectId::try_new(crate::CBOR_GENERIC_DIALECT_ID).expect("dialect"),
        ));
        jqf_codec_core::DecodeRequest {
            validation: ValidationMode::Strict,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            dialect,
            options: None,
            allow_adjacent_values: false,
            value_separator: &[],
        }
    }

    fn attribute_exact_requirement(resources: &ResourceContext<'_>, member: Option<&str>) -> AccessRequirement {
        let mut path = ExactPath::try_new(resources);
        if let Some(name) = member {
            path.try_push_semantic_member(name, resources).expect("member");
        }
        let footprint = AccessFootprint::try_exact(path, resources);
        let mut demand = CodecDemand::try_new(resources);
        demand
            .try_insert(&DemandClause::Attribute(
                ExpandedName::try_new("urn:test", "x").expect("expanded name"),
            ))
            .expect("attribute demand");
        AccessRequirement::try_exact(
            footprint,
            demand,
            AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            resources,
        )
        .expect("requirement")
    }

    #[test]
    fn supports_attribute_absence_and_binds_attribute_demand_on_exact() {
        let mut resources = resources();
        let provider = CborProvider::try_new(DiagnosticPolicy::ErrorsOnly, false, &resources).expect("provider");
        assert!(provider.supports_attribute_absence());

        let requirement = attribute_exact_requirement(&resources, Some("a"));
        let registration = crate::registration().expect("registration");
        let mut provider: ErasedProvider = registration
            .decoder()
            .expect("decoder")
            .create_provider(source(&[0xa1, 0x61, b'a', 0x01]), decode_request(), &mut resources)
            .expect("provider");
        assert!(provider.supports_attribute_absence());
        let handle = provider.bind(&requirement).expect("bind attribute on exact");
        let mut session = provider.open(&handle, &mut resources).expect("open");
        let mut context = CodecRunContext::new(&mut resources);
        context.set_cooperative_credits(4_096);
        let result = session.decode(&mut context).expect("decode");
        assert!(matches!(
            result.report().adapter(),
            AccessAdapter::AttributeAbsence | AccessAdapter::CompleteDocumentExactWithAttributeAbsence
        ));
    }
}
