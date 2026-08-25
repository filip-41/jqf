//! The `MessagePack` decoder provider and access session.
//!
//! Route inventory: Whole/`CompleteDocument` and Exact/`Located`. The located slot walks the wire without building
//! nodes, then re-decodes only the located span. No other slots are advertised: core composes no fallback adapter, so
//! an unadvertised access fails to bind.

use alloc::vec::Vec;

use jqf_codec_core::{
    AccessGuarantees, AccessInput, AccessOutcome, AccessResult, AccessResultKind, AccessSession, CodecError,
    CodecRunContext, DiagnosticPolicy, DocumentProduct, ErasedAccessSession, InputProvider, ProviderInput,
    RecycledSessionState, RouteDescription, RouteSlot,
};
use jqf_data::{
    AccountedDocumentBuilder, AccountedDocumentFinalizer, DocumentFinalizationPoll, DocumentSourceBindingPoll,
    DocumentSourceBindingStage, NodeId,
};
use jqf_resource::ResourceContext;

use crate::materialize;
use crate::options::Dialect;
use crate::scan;

/// Provider-local slot of the whole-document decode route.
const DECODE_ROUTE_SLOT: RouteSlot = RouteSlot::new(0);

pub(crate) struct MessagepackProvider {
    routes: Vec<RouteDescription>,
    dialect: Dialect,
}

impl MessagepackProvider {
    pub(crate) fn try_new(
        dialect: Dialect,
        diagnostics: DiagnosticPolicy,
        resources: &ResourceContext<'_>,
    ) -> Result<Self, CodecError> {
        let guarantees = AccessGuarantees::strict(diagnostics);
        let routes = RouteDescription::try_standard_document_table(guarantees, resources)?;
        Ok(Self { routes, dialect })
    }
}

impl InputProvider for MessagepackProvider {
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
        let source = input.source();
        if slot == DECODE_ROUTE_SLOT {
            requirement.expect_whole(AccessResultKind::CompleteDocument)?;
            let dialect = self.dialect;
            // Same read as CBOR: a positive frontier is Some(depth), zero is None (eager). Count/element hints stay
            // monotone and are ignored.
            let lazy_frontier = (requirement.lazy_frontier() > 0)
                .then(|| usize::try_from(requirement.lazy_frontier()).unwrap_or(usize::MAX));
            let state = MessagepackDecodeSession::new(source, dialect, lazy_frontier);
            return ErasedAccessSession::try_new_source_with_route(source, crate::decode_route_id(), move || Ok(state));
        }
        if slot == RouteSlot::new(1) {
            let (path, origin) = requirement.expect_exact(AccessResultKind::Located)?;
            let session = crate::routes::NativeLocatedSession::try_new(path.steps(), origin, self.dialect)?;
            return ErasedAccessSession::try_new_source_with_route(source, crate::scoped_route_id(), || Ok(session));
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
        // Slot 0 decodes ONE whole document per adjacent value and resets in place.
        if slot != RouteSlot::new(0)
            || !requirement.footprint().is_whole()
            || !requirement.schedule().is_empty_complete()
            || requirement.result() != AccessResultKind::CompleteDocument
        {
            return Ok(false);
        }
        let Some(session) = state.downcast_mut::<MessagepackDecodeSession>() else {
            return Ok(false);
        };
        session.lazy_frontier = (requirement.lazy_frontier() > 0)
            .then(|| usize::try_from(requirement.lazy_frontier()).unwrap_or(usize::MAX));
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

/// The `MessagePack` access session: one source, one document.
pub(crate) struct MessagepackDecodeSession {
    dialect: Dialect,
    /// Container-span frontier depth, when the requirement asked for deferral. `None` is the eager build.
    lazy_frontier: Option<usize>,
    phase: Phase,
    builder: Option<AccountedDocumentBuilder<'static>>,
    root: Option<NodeId>,
    binding_stage: Option<DocumentSourceBindingStage>,
    finalizer: Option<AccountedDocumentFinalizer<'static>>,
    product: Option<DocumentProduct<'static>>,
    published: bool,
}

impl MessagepackDecodeSession {
    pub(crate) fn new(_source: jqf_source::ResolvedSource<'_>, dialect: Dialect, lazy_frontier: Option<usize>) -> Self {
        Self {
            dialect,
            lazy_frontier,
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

impl AccessSession for MessagepackDecodeSession {
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
                    // One straight-line validating scan (work-checked at each item head) followed by one straight-line
                    // build; the document always carries authored spans, so the source must be sealed before it may
                    // finish.
                    let skeleton = scan::scan(source, self.dialect, context)?;
                    if self.dialect == Dialect::KeyEquivalence {
                        // The dialect's one observable law: a map key repeated under the native key-equivalence law
                        // rejects. Runs on the skeleton — native keys, before timestamp/extension or object
                        // projection.
                        crate::keys::validate_duplicate_keys(&skeleton, source, context.resources())?;
                    }
                    let (builder, root) = materialize::build_document(
                        &skeleton,
                        self.dialect,
                        source,
                        self.lazy_frontier,
                        context.resources(),
                    )?;
                    self.builder = Some(builder);
                    self.root = Some(root);
                    self.binding_stage = Some(DocumentSourceBindingStage::new(source).map_err(map_data)?);
                    self.phase = Phase::Seal;
                }
                Phase::Seal => {
                    let stage = self.binding_stage.as_mut().ok_or_else(data_contract)?;
                    // SAFETY: codec-core retains one immutable source authority
                    // for the complete access session and passes that exact
                    // authority each poll; the stage was constructed over the
                    // same segment and re-verifies identity on every call.
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
                    // the same immutable ResolvedSource authority for every
                    // poll, and the whole source is the exact segment the binding was taken over.
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
    jqf_codec_core::map_data(error, "MessagePack builder rejected document construction")
}

fn data_contract() -> CodecError {
    jqf_codec_core::data_contract("MessagePack authoritative document construction")
}

#[cfg(test)]
mod tests {
    use super::*;
    use jqf_codec_core::AccessFootprintKind;

    /// The registration advertises Whole/`CompleteDocument` and Exact/`Located`.
    #[test]
    fn the_provider_advertises_whole_and_located_slots() {
        let resources = crate::test_support::resources();
        let provider =
            MessagepackProvider::try_new(crate::options::Dialect::Utf8, DiagnosticPolicy::ErrorsOnly, &resources)
                .expect("provider");
        let routes = provider.route_descriptions();
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0].slot(), RouteSlot::new(0));
        assert_eq!(routes[0].bundle().footprint(), AccessFootprintKind::Whole);
        assert_eq!(routes[0].bundle().result(), AccessResultKind::CompleteDocument);
        assert_eq!(routes[1].slot(), RouteSlot::new(1));
        assert_eq!(routes[1].bundle().footprint(), AccessFootprintKind::Exact);
        assert_eq!(routes[1].bundle().result(), AccessResultKind::Located);
    }
}
