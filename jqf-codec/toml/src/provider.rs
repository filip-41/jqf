//! TOML decoder provider: two access slots.
//!
//! Slot 0 is the whole document. Slot 1 is the exact-path walk. Sibling: [`crate::parse`], [`crate::scoped`].

use jqf_codec_core::{
    AccessGuarantees, AccessRequirement, AccessResultKind, CodecError, DiagnosticPolicy, ErasedAccessSession,
    InputProvider, ProviderInput, RouteDescription, RouteSlot,
};

use alloc::vec::Vec;
use jqf_resource::ResourceContext;

pub(crate) struct TomlProvider {
    routes: Vec<RouteDescription>,
    dialect: DialectKind,
}

/// Which TOML grammar owns a decode request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DialectKind {
    Toml10,
    Toml11,
}

impl TomlProvider {
    pub(crate) fn try_new(
        diagnostics: DiagnosticPolicy,
        dialect: DialectKind,
        resources: &ResourceContext<'_>,
    ) -> Result<Self, CodecError> {
        // The standard two-slot document table: slot 0 serves the whole-document route (with the prune and
        // lazy-frontier hints), slot 1 the exact-path (located) route. There are no other native slots; counts and
        // element iteration answer from the whole-document floor through core's demand composition, so an unadvertised
        // access simply fails to bind.
        let guarantees = AccessGuarantees::strict(diagnostics);
        let routes = RouteDescription::try_standard_document_table(guarantees, resources)?;
        Ok(Self { routes, dialect })
    }
}

impl InputProvider for TomlProvider {
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
        _resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedAccessSession<'source>, CodecError> {
        let dialect = self.dialect;
        let source = input.source();
        if slot == RouteSlot::new(0) {
            requirement.expect_whole(AccessResultKind::CompleteDocument)?;
            let lazy_frontier = (requirement.lazy_frontier() > 0)
                .then(|| usize::try_from(requirement.lazy_frontier()).unwrap_or(usize::MAX));
            // The prune hint rides the whole-document requirement: the build omits members the program provably cannot
            // read. `None` when no tree rides the requirement or it keeps everything.
            let prune = requirement.prune().and_then(crate::parse::PruneLookup::from_transport);
            let coverage = jqf_codec_core::required_builder_coverage(requirement);
            let state = crate::parse::TomlParseState::try_new(dialect, lazy_frontier, prune, coverage);
            return ErasedAccessSession::try_new_source_with_route(source, crate::FULL_PHYSICAL_ROUTE_ID, || Ok(state));
        }
        if slot == RouteSlot::new(1) {
            let (path, origin) = requirement.expect_exact(AccessResultKind::Located)?;
            let coverage = jqf_codec_core::required_builder_coverage(requirement);
            let session = crate::scoped::NativeScopedSession::try_new(
                path.steps(),
                origin,
                dialect,
                coverage,
                requirement.located_skeleton(),
            )?;
            ErasedAccessSession::try_new_source_with_route(source, crate::SCOPED_PHYSICAL_ROUTE_ID, || Ok(session))
        } else {
            Err(CodecError::new(jqf_codec_core::CodecFailureKind::ProviderRouteMismatch))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jqf_codec_core::{
        AccessAdapter, AccessFootprint, AccessGuarantees, AccessRequirement, CodecDemand, CodecRunContext,
        DemandClause, DiagnosticPolicy, ErasedProvider, ExactPath, ValidationMode,
    };
    use jqf_data::{DialectId, ExpandedName};
    use jqf_resource::ResourceContext;
    use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

    fn resources() -> ResourceContext<'static> {
        crate::test_support::resources()
    }

    fn source(bytes: &[u8]) -> ResolvedSource<'_> {
        ResolvedSource::new(
            SourceRef::new(SourceId::new(1), SourceKind::Input),
            "test.toml",
            bytes,
            0,
        )
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
        let provider =
            TomlProvider::try_new(DiagnosticPolicy::ErrorsOnly, DialectKind::Toml10, &resources).expect("provider");
        assert!(provider.supports_attribute_absence());

        let requirement = attribute_exact_requirement(&resources, Some("a"));
        let registration = crate::registration_1_0().expect("registration");
        let mut provider: ErasedProvider = registration
            .decoder()
            .expect("decoder")
            .create_provider(
                source(b"a = 1\n"),
                jqf_codec_core::DecodeRequest {
                    validation: ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect: &DialectId::try_new(crate::TOML_JQF_1_0_DIALECT_ID).expect("dialect"),
                    options: None,
                    allow_adjacent_values: false,
                    value_separator: &[],
                },
                &mut resources,
            )
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
