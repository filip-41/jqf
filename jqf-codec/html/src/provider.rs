//! The HTML decoder provider: route advertisement and opening.

use alloc::vec::Vec;

use jqf_codec_core::{
    AccessFootprintKind, AccessGuarantees, AccessRequirement, AccessResultKind, CodecError, DecodeRequest,
    DiagnosticPolicy, ErasedAccessSession, ErasedProvider, InputProvider, ProviderInput, RouteDescription, RouteSlot,
    markup_measure_demand, required_builder_coverage,
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
            requirement.expect_whole(AccessResultKind::CompleteDocument)?;
            // Empty-path count or Whole bare-root `type` licenses the
            // measure-skeleton decode: after WHATWG recover the session serves
            // the document element as an array of cheap child nodes — the same
            // kind `type`/`length` read on the full document. Element demand
            // keeps `build_document`; measure children are NAME-only stubs.
            let coverage = required_builder_coverage(requirement);
            let session = HtmlSession::new(source, self.fragment, coverage, markup_measure_demand(requirement))?;
            return ErasedAccessSession::try_new_source_with_route(source, crate::FULL_PHYSICAL_ROUTE_ID, || {
                Ok(session)
            });
        }
        if slot == RouteSlot::new(1) {
            let (path, origin) = requirement.expect_exact(AccessResultKind::Located)?;
            // Re-anchored Exact prune: omit unread child elements of the located subtree. Recover still runs on every
            // byte first; only the subtree materialize drops unobservable named children.
            let prune = requirement
                .prune()
                .and_then(jqf_codec_core::PruneLookup::from_transport);
            let coverage = required_builder_coverage(requirement);
            let session = crate::scoped::NativeScopedSession::try_new(
                source,
                path.steps(),
                origin,
                self.fragment,
                prune,
                coverage,
            )?;
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

#[cfg(test)]
mod measure_provider_tests {
    use super::*;
    use jqf_codec_core::{CodecRunContext, DecodeRequest};
    use jqf_data::{CountDemand, CountRow, DialectId};

    fn resources() -> jqf_resource::ResourceContext<'static> {
        jqf_resource::ResourceContext::new(
            jqf_resource::RequestAccount::try_new(jqf_resource::ResourceLimits::new(
                u64::MAX,
                u64::MAX,
                u64::MAX,
                u64::MAX,
                u32::MAX,
            ))
            .expect("account"),
            &jqf_resource::ContinueControl,
            jqf_resource::WorkMeter::try_new_v1(4096).expect("work"),
        )
        .expect("resources")
    }

    fn source(bytes: &[u8]) -> jqf_source::ResolvedSource<'_> {
        jqf_source::ResolvedSource::new(
            jqf_source::SourceRef::new(jqf_source::SourceId::new(0), jqf_source::SourceKind::Input),
            "input",
            bytes,
            0,
        )
    }

    #[test]
    fn count_requirement_opens_the_measure_session() {
        let mut resources = resources();
        let registration = crate::registration().expect("registration");
        let mut provider = registration
            .decoder()
            .expect("decoder")
            .create_provider(
                source(b"<a href=\"https://ex\">hi</a>"),
                DecodeRequest {
                    validation: jqf_codec_core::ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect: &DialectId::try_new(crate::HTML_DOCUMENT_DIALECT_ID).expect("dialect"),
                    options: None,
                    allow_adjacent_values: false,
                    value_separator: &[],
                },
                &mut resources,
            )
            .expect("provider");
        let demand = jqf_codec_core::CodecDemand::try_new(&resources);
        let requirement = jqf_codec_core::AccessRequirement::try_whole(
            demand,
            jqf_codec_core::AccessGuarantees::new(jqf_codec_core::ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly),
            &resources,
        )
        .expect("requirement")
        .with_count(CountDemand {
            row: CountRow::Container,
            path: Vec::new(),
            range: None,
            probe: Vec::new(),
            filter: None,
        });
        let handle = provider.bind(&requirement).expect("bind");
        let mut session = provider.open(&handle, &mut resources).expect("open");
        let result = {
            let mut run = CodecRunContext::new(&mut resources);
            run.set_cooperative_credits(4096);
            session.decode(&mut run).expect("decode")
        };
        match result.outcome() {
            jqf_codec_core::AccessOutcome::FullDocument(product) => {
                assert_eq!(
                    product.document().node_count(),
                    3,
                    "the measure skeleton is html plus two cheap children: nodes={}",
                    product.document().node_count()
                );
            }
            jqf_codec_core::AccessOutcome::Located(_) => {
                panic!("expected a full document outcome")
            }
        }
    }

    #[test]
    fn type_requirement_opens_the_measure_session() {
        let mut resources = resources();
        let registration = crate::registration().expect("registration");
        let mut provider = registration
            .decoder()
            .expect("decoder")
            .create_provider(
                source(b"<a href=\"https://ex\">hi</a>"),
                DecodeRequest {
                    validation: jqf_codec_core::ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect: &DialectId::try_new(crate::HTML_DOCUMENT_DIALECT_ID).expect("dialect"),
                    options: None,
                    allow_adjacent_values: false,
                    value_separator: &[],
                },
                &mut resources,
            )
            .expect("provider");
        let demand = jqf_codec_core::CodecDemand::try_new(&resources);
        let requirement = jqf_codec_core::AccessRequirement::try_whole(
            demand,
            jqf_codec_core::AccessGuarantees::new(jqf_codec_core::ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly),
            &resources,
        )
        .expect("requirement")
        .with_type_demand();
        assert!(requirement.type_demand(), "the requirement must carry the type hint");
        let handle = provider.bind(&requirement).expect("bind");
        let mut session = provider.open(&handle, &mut resources).expect("open");
        let result = {
            let mut run = CodecRunContext::new(&mut resources);
            run.set_cooperative_credits(4096);
            session.decode(&mut run).expect("decode")
        };
        match result.outcome() {
            jqf_codec_core::AccessOutcome::FullDocument(product) => {
                let document = product.document();
                let kind = document
                    .value_view(document.root_handle())
                    .expect("root view")
                    .kind()
                    .expect("root kind");
                assert_eq!(kind, jqf_data::ValueKind::Array, "root kind is array");
            }
            jqf_codec_core::AccessOutcome::Located(_) => {
                panic!("expected a full document outcome")
            }
        }
    }

    #[test]
    fn identity_demand_keeps_the_doctype_fact() {
        let mut resources = resources();
        let registration = crate::registration().expect("registration");
        let mut provider = registration
            .decoder()
            .expect("decoder")
            .create_provider(
                source(b"<!DOCTYPE html><html></html>"),
                DecodeRequest {
                    validation: jqf_codec_core::ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect: &DialectId::try_new(crate::HTML_DOCUMENT_DIALECT_ID).expect("dialect"),
                    options: None,
                    allow_adjacent_values: false,
                    value_separator: &[],
                },
                &mut resources,
            )
            .expect("provider");
        let requirement = jqf_codec_core::AccessRequirement::try_whole(
            jqf_codec_core::CodecDemand::try_new(&resources),
            jqf_codec_core::AccessGuarantees::new(jqf_codec_core::ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly),
            &resources,
        )
        .expect("requirement");
        let handle = provider.bind(&requirement).expect("bind");
        let mut session = provider.open(&handle, &mut resources).expect("open");
        let result = {
            let mut run = CodecRunContext::new(&mut resources);
            run.set_cooperative_credits(4096);
            session.decode(&mut run).expect("decode")
        };
        let jqf_codec_core::AccessOutcome::FullDocument(product) = result.outcome() else {
            panic!("expected a full document outcome");
        };
        let document = product.document();
        let root = document.root();
        let has_doctype = document.owner_fact_ids(root).iter().any(|id| {
            document
                .fact(*id)
                .is_ok_and(|fact| fact.role().as_str() == crate::document::DOCTYPE_FACT)
        });
        assert!(has_doctype, "identity must still attach html.doctype@1");
    }
}
