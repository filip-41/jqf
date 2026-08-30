//! The flat-config decoder provider and access session.
//!
//! Route inventory: ONE advertised slot, Whole/`CompleteDocument`, for every grammar. A richer demand (Located) is
//! served by core's generic exact adapter over the whole route. Adjacent values are a requirement mismatch.

use alloc::vec::Vec;

use jqf_codec_core::{
    AccessFootprintKind, AccessGuarantees, AccessInput, AccessOutcome, AccessResult, AccessResultKind, AccessSession,
    CodecError, CodecRunContext, DiagnosticPolicy, DocumentProduct, ErasedAccessSession, InputProvider, ProviderInput,
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

/// Provider-local slot of the whole-document decode route.
const DECODE_ROUTE_SLOT: RouteSlot = RouteSlot::new(0);

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
        // Slot 0: Whole/CompleteDocument.
        let routes = RouteDescription::try_table(
            &[(
                RouteSlot::new(0),
                AccessFootprintKind::Whole,
                AccessResultKind::CompleteDocument,
            )],
            guarantees,
            resources,
        )?;
        Ok(Self { routes, grammar })
    }
}

impl InputProvider for FlatProvider {
    fn route_descriptions(&self) -> &[RouteDescription] {
        self.routes.as_slice()
    }

    fn open_route<'source>(
        &mut self,
        input: ProviderInput<'source>,
        slot: RouteSlot,
        requirement: &jqf_codec_core::AccessRequirement,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedAccessSession<'source>, CodecError> {
        if slot != DECODE_ROUTE_SLOT
            || !requirement.footprint().is_whole()
            || !requirement.schedule().is_empty_complete()
            || requirement.result() != AccessResultKind::CompleteDocument
        {
            return Err(CodecError::new(jqf_codec_core::CodecFailureKind::ProviderRouteMismatch));
        }
        let source = input.source();
        let grammar = self.grammar;
        let coverage = jqf_codec_core::required_builder_coverage(requirement);
        let state = FlatDecodeSession::new(source, grammar, coverage);
        ErasedAccessSession::try_new_source_with_route(source, crate::decode_route_id(grammar), move || Ok(state))
    }

    fn try_reopen_route(
        &mut self,
        state: &mut RecycledSessionState<'_>,
        slot: RouteSlot,
        requirement: &jqf_codec_core::AccessRequirement,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        // Slot 0 resets the whole-document session in place.
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
