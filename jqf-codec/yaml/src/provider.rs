//! YAML decoder provider and access session.

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
            let state = crate::parse::YamlParseState::try_new(source, dialect, prune, resources)?;
            return ErasedAccessSession::try_new_source_with_route(source, crate::FULL_PHYSICAL_ROUTE_ID, || Ok(state));
        }
        if slot == RouteSlot::new(1) {
            let (path, origin) = requirement.expect_exact(AccessResultKind::Located)?;
            let session = crate::scoped::NativeScopedSession::try_new(path.steps(), origin, dialect)?;
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
