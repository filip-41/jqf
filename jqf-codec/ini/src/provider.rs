//! The flat-config decoder provider and access session.
//!
//! Route inventory: two advertised slots, Whole/`CompleteDocument` (slot 0) and Exact/`Located` (slot 1), for every
//! grammar. Slot 1 opens a native scoped session: scan the whole input, materialize only the hit, republish as product
//! root. Attribute demand still binds through absence (Attributes are empty; `.&` is not in the grammar). Adjacent
//! values are a requirement mismatch.

use alloc::vec::Vec;

use jqf_codec_core::{
    AccessGuarantees, AccessInput, AccessOutcome, AccessResult, AccessResultKind, AccessSession, CodecError,
    CodecRunContext, DiagnosticPolicy, DocumentProduct, ErasedAccessSession, InputProvider, ProviderInput,
    RecycledSessionState, RouteDescription, RouteSlot,
};
use jqf_data::{
    AccountedDocumentBuilder, AccountedDocumentFinalizer, BuilderCoverage, DocumentFinalizationPoll,
    DocumentSourceBindingPoll, DocumentSourceBindingStage, NodeId,
};
use jqf_resource::ResourceContext;

use crate::materialize;
use crate::options::Grammar;
use crate::scan;
use crate::scoped;

/// Provider-local slot of the whole-document decode route.
const DECODE_ROUTE_SLOT: RouteSlot = RouteSlot::new(0);
/// Provider-local slot of the native Exact/Located route.
const SCOPED_ROUTE_SLOT: RouteSlot = RouteSlot::new(1);

pub(crate) struct FlatProvider {
    routes: Vec<RouteDescription>,
    grammar: Grammar,
}

impl FlatProvider {
    pub(crate) fn try_new(
        grammar: Grammar,
        diagnostics: DiagnosticPolicy,
        resources: &ResourceContext<'_>,
    ) -> Result<Self, CodecError> {
        let guarantees = AccessGuarantees::strict(diagnostics);
        // Slot 0: Whole/CompleteDocument. Slot 1: Exact/Located.
        let routes = RouteDescription::try_standard_document_table(guarantees, resources)?;
        Ok(Self { routes, grammar })
    }
}

impl InputProvider for FlatProvider {
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
        requirement: &jqf_codec_core::AccessRequirement,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedAccessSession<'source>, CodecError> {
        let source = input.source();
        let grammar = self.grammar;
        let coverage = jqf_codec_core::required_builder_coverage(requirement);
        if slot == DECODE_ROUTE_SLOT {
            requirement.expect_whole(AccessResultKind::CompleteDocument)?;
            let state = FlatDecodeSession::new(source, grammar, coverage);
            return ErasedAccessSession::try_new_source_with_route(source, crate::decode_route_id(grammar), || {
                Ok(state)
            });
        }
        if slot == SCOPED_ROUTE_SLOT {
            let (path, origin) = requirement.expect_exact(AccessResultKind::Located)?;
            let session = scoped::NativeScopedSession::try_new(path.steps(), origin, grammar, coverage)?;
            return ErasedAccessSession::try_new_source_with_route(source, crate::scoped_route_id(grammar), || {
                Ok(session)
            });
        }
        Err(CodecError::new(jqf_codec_core::CodecFailureKind::ProviderRouteMismatch))
    }

    fn try_reopen_route(
        &mut self,
        state: &mut RecycledSessionState<'_>,
        slot: RouteSlot,
        requirement: &jqf_codec_core::AccessRequirement,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        // Slot 0 resets the whole-document session in place. Slot 1 is a single document; reopen returns false.
        if slot != RouteSlot::new(0)
            || !requirement.footprint().is_whole()
            || !requirement.schedule().is_empty_complete()
            || requirement.result() != AccessResultKind::CompleteDocument
        {
            return Ok(false);
        }
        let Some(session) = state.downcast_mut::<FlatDecodeSession>() else {
            return Ok(false);
        };
        session.coverage = jqf_codec_core::required_builder_coverage(requirement);
        session.try_reset();
        Ok(true)
    }
}

enum Phase {
    Scan,
    Seal,
    Finalize,
    Publish,
}

/// The flat-config access session: one source, one document.
pub(crate) struct FlatDecodeSession {
    grammar: Grammar,
    coverage: BuilderCoverage,
    phase: Phase,
    builder: Option<AccountedDocumentBuilder<'static>>,
    root: Option<NodeId>,
    binding_stage: Option<DocumentSourceBindingStage>,
    finalizer: Option<AccountedDocumentFinalizer<'static>>,
    product: Option<DocumentProduct<'static>>,
    published: bool,
}

impl FlatDecodeSession {
    pub(crate) fn new(_source: jqf_source::ResolvedSource<'_>, grammar: Grammar, coverage: BuilderCoverage) -> Self {
        Self {
            grammar,
            coverage,
            phase: Phase::Scan,
            builder: None,
            root: None,
            binding_stage: None,
            finalizer: None,
            product: None,
            published: false,
        }
    }

    pub(crate) fn try_reset(&mut self) {
        self.phase = Phase::Scan;
        self.builder = None;
        self.root = None;
        self.binding_stage = None;
        self.finalizer = None;
        self.product = None;
        self.published = false;
    }

    fn begin_finalize(&mut self, resources: &mut ResourceContext<'_>) -> Result<(), CodecError> {
        let root = self.root.take().ok_or_else(data_contract)?;
        let builder = self.builder.take().ok_or_else(data_contract)?;
        self.finalizer = Some(builder.begin_finish(root, resources).map_err(map_data)?);
        self.phase = Phase::Finalize;
        Ok(())
    }
}

impl AccessSession for FlatDecodeSession {
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
    #[allow(
        unsafe_code,
        reason = "the source-binding and span-admission APIs are unsafe by jqf-data's design; the codec-core session continuously owns the exact whole-source authority every call receives"
    )]
    fn decode<'source>(
        &mut self,
        input: AccessInput<'_, 'source>,
        context: &mut CodecRunContext<'_, '_>,
    ) -> Result<AccessResult<'source>, CodecError> {
        let source = match input {
            AccessInput::Source(source) => source,
            AccessInput::Document(_) => {
                return Err(CodecError::new(jqf_codec_core::CodecFailureKind::ProviderRouteMismatch));
            }
        };
        if self.published {
            return Err(data_contract());
        }
        loop {
            match self.phase {
                Phase::Scan => {
                    // One straight-line validating scan (work-checked at its loop head) followed by one straight-line
                    // build; the document always carries authored spans, so the source must be sealed before it may
                    // finish.
                    let skeleton = scan::scan(source, self.grammar, context)?;
                    let (builder, root) =
                        materialize::build_document(&skeleton, self.grammar, self.coverage, context.resources())?;
                    self.builder = Some(builder);
                    self.root = Some(root);
                    self.binding_stage = Some(DocumentSourceBindingStage::new(source).map_err(map_data)?);
                    self.phase = Phase::Seal;
                }
                Phase::Seal => {
                    let stage = self.binding_stage.as_mut().ok_or_else(data_contract)?;
                    // SAFETY: codec-core retains one immutable source authority
                    // for the complete access session and passes that exact authority each poll; the stage was
                    // constructed over the same segment and re-verifies identity on every call.
                    match unsafe { stage.poll(source, context.resources()) }.map_err(map_data)? {
                        DocumentSourceBindingPoll::Pending => context.replenish_work()?,
                        DocumentSourceBindingPoll::Ready(binding) => {
                            self.binding_stage = None;
                            self.builder
                                .as_mut()
                                .ok_or_else(data_contract)?
                                .bind_source(binding)
                                .map_err(map_data)?;
                            self.begin_finalize(context.resources())?;
                        }
                    }
                }
                Phase::Finalize => {
                    let finalizer = self.finalizer.as_mut().ok_or_else(data_contract)?;
                    // SAFETY: the codec-core access session owns and supplies
                    // the same immutable ResolvedSource authority for every poll, and the whole source is the exact
                    // segment the binding was taken over.
                    let poll = unsafe { finalizer.poll_with_source(source, context.resources()) }.map_err(map_data)?;
                    let DocumentFinalizationPoll::Ready(document) = poll else {
                        context.replenish_work()?;
                        continue;
                    };
                    self.finalizer = None;
                    self.product = Some(DocumentProduct::try_new(document, context.resources())?);
                    self.phase = Phase::Publish;
                }
                Phase::Publish => {
                    let product = self.product.take().ok_or_else(data_contract)?;
                    // SAFETY: codec-core owns this exact immutable ResolvedSource
                    // for the whole access session; the binding was taken over this same whole segment.
                    let product =
                        unsafe { product.attach_borrowed_source_from_access_session(source, context.resources())? };
                    self.published = true;
                    return Ok(AccessResult::from_outcome(AccessOutcome::FullDocument(product)));
                }
            }
        }
    }
}

fn map_data(error: jqf_data::DataError) -> CodecError {
    jqf_codec_core::map_data(error, "flat-config builder rejected document construction")
}

fn data_contract() -> CodecError {
    jqf_codec_core::data_contract("flat-config authoritative document construction")
}

#[cfg(test)]
mod tests {
    use super::*;
    use jqf_codec_core::{
        AccessAdapter, AccessFootprint, AccessGuarantees, AccessRequirement, CodecDemand, CodecRunContext,
        DemandClause, DiagnosticPolicy, ErasedProvider, ExactPath, ValidationMode,
    };
    use jqf_data::{DialectId, ExpandedName};
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
            "test.properties",
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

    /// Attribute absence opens Exact with the absence adapter (Attributes are empty; `.&` is not in the grammar).
    #[test]
    fn supports_attribute_absence_and_binds_attribute_demand_on_exact() {
        let mut resources = resources();
        let provider =
            FlatProvider::try_new(Grammar::Properties, DiagnosticPolicy::ErrorsOnly, &resources).expect("provider");
        assert!(provider.supports_attribute_absence());

        let requirement = attribute_exact_requirement(&resources, Some("a"));
        let registration = crate::registration().expect("registration");
        let mut provider: ErasedProvider = registration
            .decoder()
            .expect("decoder")
            .create_provider(
                source(b"a=1\n"),
                jqf_codec_core::DecodeRequest {
                    validation: ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect: &DialectId::try_new(crate::PROPERTIES_JDK_DIALECT_ID).expect("dialect"),
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
            AccessAdapter::CompleteDocumentExactWithAttributeAbsence | AccessAdapter::AttributeAbsence
        ));
    }
}
