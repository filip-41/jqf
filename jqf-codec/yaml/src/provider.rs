//! YAML decoder provider and access session.
//!
//! Slot 0 is Whole/`CompleteDocument`; slot 1 is Exact/`Located`. The graph is
//! always fully parsed (YAML cannot defer). Exact prune trims only the located
//! subtree after that parse. Tags attach when the program reads `.@tag` or
//! preserves facts — see [`crate::document::demanded_intrinsic`].

use alloc::vec::Vec;

use jqf_codec_core::{
    AccessGuarantees, AccessRequirement, AccessResultKind, CodecError, DiagnosticPolicy, ErasedAccessSession,
    InputProvider, ProviderInput, RecycledSessionState, RouteDescription, RouteSlot,
};

use jqf_resource::ResourceContext;

/// Which YAML schema owns a decode request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DialectKind {
    /// The failsafe schema (map/seq/str only).
    Failsafe,
    /// The JSON schema (failsafe + null/bool/int/float with JSON regexes).
    Json,
    /// The core schema (the same seven tags with core resolution).
    Core,
}

pub(crate) struct YamlProvider {
    routes: Vec<RouteDescription>,
    dialect: DialectKind,
}

impl YamlProvider {
    pub(crate) fn try_new(
        diagnostics: DiagnosticPolicy,
        dialect: DialectKind,
        resources: &ResourceContext<'_>,
    ) -> Result<Self, CodecError> {
        let guarantees = AccessGuarantees::strict(diagnostics);
        let routes = RouteDescription::try_standard_document_table(guarantees, resources)?;
        Ok(Self { routes, dialect })
    }
}

impl InputProvider for YamlProvider {
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
        let dialect = self.dialect;
        let source = input.source();
        if slot == RouteSlot::new(0) {
            requirement.expect_whole(AccessResultKind::CompleteDocument)?;
            // The prune hint rides the whole-document requirement: the engine lowered the kept-subtree tree for the
            // requesting program, and the document walk omits mapping members the program can never read. `None` when
            // no tree rides the requirement or it keeps everything.
            let prune = requirement
                .prune()
                .and_then(crate::document::PruneLookup::from_transport);
            let coverage = jqf_codec_core::required_builder_coverage(requirement);
            let want_tags = jqf_codec_core::requirement_wants_intrinsic_tag(requirement);
            let state = crate::parse::YamlParseState::try_new(source, dialect, prune, coverage, want_tags, resources)?;
            return ErasedAccessSession::try_new_source_with_route(source, crate::FULL_PHYSICAL_ROUTE_ID, || Ok(state));
        }
        if slot == RouteSlot::new(1) {
            let (path, origin) = requirement.expect_exact(AccessResultKind::Located)?;
            let coverage = jqf_codec_core::required_builder_coverage(requirement);
            let want_tags = jqf_codec_core::requirement_wants_intrinsic_tag(requirement);
            // Re-anchored Exact prune: omit unread mapping members of the located subtree. The graph is still fully
            // parsed (YAML cannot defer); only the subtree materialize drops unobservable members.
            let prune = requirement
                .prune()
                .and_then(crate::document::PruneLookup::from_transport);
            let session = crate::scoped::NativeScopedSession::try_new(
                path.steps(),
                origin,
                dialect,
                coverage,
                want_tags,
                prune,
                requirement.type_demand(),
            )?;
            ErasedAccessSession::try_new_source_with_route(source, crate::SCOPED_PHYSICAL_ROUTE_ID, || Ok(session))
        } else {
            Err(CodecError::new(jqf_codec_core::CodecFailureKind::ProviderRouteMismatch))
        }
    }

    fn try_reopen_route(
        &mut self,
        state: &mut RecycledSessionState<'_>,
        slot: RouteSlot,
        requirement: &AccessRequirement,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        // Slot 0 decodes ONE document per adjacent value and resets in place for the next adjacent document. Slot 1 is
        // never recycled; the next `---` unit is a new session at `consumed_offset`.
        if slot != RouteSlot::new(0)
            || !requirement.footprint().is_whole()
            || !requirement.schedule().is_empty_complete()
            || requirement.result() != AccessResultKind::CompleteDocument
        {
            return Ok(false);
        }
        let Some(parse) = state.downcast_mut::<crate::parse::YamlParseState>() else {
            return Ok(false);
        };
        parse.try_reset(self.dialect)
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
            "test.yaml",
            bytes,
            0,
        )
    }

    fn decode_request() -> jqf_codec_core::DecodeRequest<'static> {
        let dialect: &'static jqf_data::DialectId = alloc::boxed::Box::leak(alloc::boxed::Box::new(
            jqf_data::DialectId::try_new(crate::YAML_CORE_DIALECT_ID).expect("dialect"),
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

    /// YAML has no markup attributes; the absence flag is truthful and an attribute demand on Exact binds through the
    /// absence adapter instead of `HardMismatch`.
    #[test]
    fn supports_attribute_absence_and_binds_attribute_demand_on_exact() {
        let mut resources = resources();
        let provider =
            YamlProvider::try_new(DiagnosticPolicy::ErrorsOnly, DialectKind::Core, &resources).expect("provider");
        assert!(provider.supports_attribute_absence());

        let requirement = attribute_exact_requirement(&resources, Some("a"));
        let registration = crate::registration().expect("registration");
        let mut provider: ErasedProvider = registration
            .decoder()
            .expect("decoder")
            .create_provider(source(b"a: 1\n"), decode_request(), &mut resources)
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
