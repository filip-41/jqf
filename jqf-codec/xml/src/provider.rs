//! XML decoder provider and access session.
//!
//! XML v1 advertises two slots: whole-document
//! (slot 0, `CompleteDocument`) and located/scoped (slot 1, `Exact`/`Located`).
//! No other rungs are registered — an XML child element's members are its
//! positional children, not named fields, so no projection-style slot is
//! meaningful. The `@`-family fact accessor (which would
//! give `.name`) is a separate engine surface. When a new slot becomes
//! meaningful it grows this inventory in the same commit.

use jqf_codec_core::{
    AccessFootprintKind, AccessGuarantees, AccessRequirement, AccessResultKind, CodecError, DiagnosticPolicy,
    ErasedAccessSession, InputProvider, ProviderInput, RouteDescription, RouteSlot, markup_measure_demand,
    required_builder_coverage,
};
use jqf_resource::ResourceContext;

use alloc::vec::Vec;

use crate::session::XmlSession;

pub(crate) struct XmlProvider {
    routes: Vec<RouteDescription>,
}

impl XmlProvider {
    pub(crate) fn try_new(diagnostics: DiagnosticPolicy, resources: &ResourceContext<'_>) -> Result<Self, CodecError> {
        let guarantees = AccessGuarantees::strict(diagnostics);
        let routes = RouteDescription::try_table(
            &[
                // Slot 0: Whole/CompleteDocument.
                (
                    RouteSlot::new(0),
                    AccessFootprintKind::Whole,
                    AccessResultKind::CompleteDocument,
                ),
                // Slot 1: Exact/Located (scoped).
                (RouteSlot::new(1), AccessFootprintKind::Exact, AccessResultKind::Located),
            ],
            guarantees,
            resources,
        )?;
        Ok(Self { routes })
    }
}

impl InputProvider for XmlProvider {
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
        let exact = !requirement.footprint().is_whole() && !requirement.schedule().is_empty_complete();
        if slot == RouteSlot::new(0) {
            if exact || requirement.result() != AccessResultKind::CompleteDocument {
                return Err(mismatch());
            }
            // Empty-path count or Whole bare-root `type` licenses the
            // measure-skeleton decode: the provider serves the document element
            // as an array of deferred child spans — the same kind `type`/`length`
            // read on the full document. Element demand keeps the full tree.
            // Content stays on: xpath string(.) reads xml.content@1 after decode.
            // The whole build forces NAME_FACT and takes topology from the
            // requirement (Preserve for encode).
            let attach_content = true;
            let coverage = required_builder_coverage(requirement);
            let session = XmlSession::new(source, markup_measure_demand(requirement), attach_content, coverage)?;
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
            let session = crate::scoped::NativeScopedSession::try_new(source, path.steps(), origin)?;
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
                source(b"<catalog><item id=\"0\"/><item id=\"1\"/></catalog>"),
                DecodeRequest {
                    validation: jqf_codec_core::ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect: &DialectId::try_new(crate::XML_DETERMINISTIC_DIALECT_ID).expect("dialect"),
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
                    product.document().container_span_count(),
                    2,
                    "the measure skeleton defers both children: spans={}",
                    product.document().container_span_count()
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
                source(b"<catalog><item id=\"0\"/><item id=\"1\"/></catalog>"),
                DecodeRequest {
                    validation: jqf_codec_core::ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect: &DialectId::try_new(crate::XML_DETERMINISTIC_DIALECT_ID).expect("dialect"),
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
                assert_eq!(
                    document.container_span_count(),
                    2,
                    "the measure skeleton defers both children"
                );
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
}
