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
            let session = crate::scoped::NativeScopedSession::try_new(path.steps(), origin, dialect, coverage)?;
            ErasedAccessSession::try_new_source_with_route(source, crate::SCOPED_PHYSICAL_ROUTE_ID, || Ok(session))
        } else {
            Err(CodecError::new(jqf_codec_core::CodecFailureKind::ProviderRouteMismatch))
        }
    }
}
