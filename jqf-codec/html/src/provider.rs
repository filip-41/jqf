//! The HTML decoder provider: route advertisement and opening.

use alloc::vec::Vec;

use jqf_codec_core::{
    AccessFootprintKind, AccessGuarantees, AccessRequirement, AccessResultKind, CodecError, DecodeRequest,
    DiagnosticPolicy, ErasedAccessSession, ErasedProvider, InputProvider, ProviderInput, RouteDescription, RouteSlot,
};
use jqf_resource::ResourceContext;

use crate::session::HtmlSession;

/// The decoder factory entry point, specialized per input dialect: `false` is the document dialect, `true` the fragment
/// dialect . The dialect does not reach the provider through `DecodeRequest`, so the two HTML input dialects register
/// as two SEPARATE registrations (the YAML per-schema precedent); the registry's `DecoderFactory` is a fn pointer, so
/// each mode is its own non-capturing entry point into one shared constructor.
pub(crate) type DecoderEntry = jqf_codec_core::DecoderFactory;

pub(crate) fn decoder_for(fragment: bool) -> DecoderEntry {
    if fragment {
        create_provider_fragment
    } else {
        create_provider_document
    }
}

fn create_provider_document<'source>(
    source: jqf_source::ResolvedSource<'source>,
    request: DecodeRequest<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedProvider<'source>, CodecError> {
    create_provider_for(source, request, resources, false)
}

fn create_provider_fragment<'source>(
    source: jqf_source::ResolvedSource<'source>,
    request: DecodeRequest<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedProvider<'source>, CodecError> {
    create_provider_for(source, request, resources, true)
}

fn create_provider_for<'source>(
    source: jqf_source::ResolvedSource<'source>,
    request: DecodeRequest<'_>,
    resources: &mut ResourceContext<'_>,
    fragment: bool,
) -> Result<ErasedProvider<'source>, CodecError> {
    request.expect_strict_defaults()?;
    let provider = HtmlProvider::try_new(request.diagnostics, fragment, resources)?;
    ErasedProvider::try_new_provider(source, resources, || Ok(provider))
}

/// The HTML provider: two access slots — whole-document (slot 0) and located/scoped (slot 1) — both served from the
/// recovered document (the whole document must be recovered before ANY selection is authoritative, §4.10's own law).
/// Specialized rungs bind through the core fallback composition instead.
pub(crate) struct HtmlProvider {
    routes: Vec<RouteDescription>,
    /// Whether every route parses the input as a FRAGMENT (the `html.fragment@1` registration) rather than a document.
    fragment: bool,
}

impl HtmlProvider {
    pub(crate) fn try_new(
        diagnostics: DiagnosticPolicy,
        fragment: bool,
        resources: &ResourceContext<'_>,
    ) -> Result<Self, CodecError> {
        let guarantees = AccessGuarantees::strict(diagnostics);
        let routes = RouteDescription::try_table(
            &[
                (
                    RouteSlot::new(0),
                    AccessFootprintKind::Whole,
                    AccessResultKind::CompleteDocument,
                ),
                (RouteSlot::new(1), AccessFootprintKind::Exact, AccessResultKind::Located),
            ],
            guarantees,
            resources,
        )?;
        Ok(Self { routes, fragment })
    }
}

impl InputProvider for HtmlProvider {
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
        let source = input.source();
        if slot == RouteSlot::new(0) {
            let exact = !requirement.footprint().is_whole() && !requirement.schedule().is_empty_complete();
            if exact || requirement.result() != AccessResultKind::CompleteDocument {
                return Err(mismatch());
            }
            let session = HtmlSession::new(source, self.fragment)?;
            return ErasedAccessSession::try_new_source_with_route(source, crate::FULL_PHYSICAL_ROUTE_ID, || {
                Ok(session)
            });
        }
        let path = requirement.footprint().exact_path().ok_or_else(mismatch)?;
        let origin = requirement.schedule().singleton_origin().ok_or_else(mismatch)?;
        if slot == RouteSlot::new(1) {
            if requirement.result() != AccessResultKind::Located {
                return Err(mismatch());
            }
            let session = crate::scoped::NativeScopedSession::try_new(source, path.steps(), origin, self.fragment)?;
            return ErasedAccessSession::try_new_source_with_route(source, crate::SCOPED_PHYSICAL_ROUTE_ID, || {
                Ok(session)
            });
        }
        Err(mismatch())
    }
}

fn mismatch() -> CodecError {
    CodecError::new(jqf_codec_core::CodecFailureKind::ProviderRouteMismatch)
}
