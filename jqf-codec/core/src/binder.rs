//! Requirement-to-route binding and session open/reuse dispatch.
//!
//! Demand clauses refuse at bind. Result authority may fall through to the whole-document adapter. Sibling:
//! [`crate::access`].

use jqf_resource::ResourceContext;
use jqf_source::ResolvedSource;

use crate::access::{ResidualKey, residual_key};
use crate::capability::{AccessFootprintKind, AccessResultKind};
use crate::provider::{InputProvider, ProviderInput};
use crate::{
    AccessAdapter, AccessBindError, AccessHandle, AccessRequirement, CodecError, CodecFailureKind, ErasedAccessSession,
    RouteDescription, RouteSlot,
};

/// A caller-held recycled access session for adjacent-value decoding.
///
/// One instance is carried across a whole adjacent-value sequence (NDJSON and friends). It retains the previous value's
/// session carrier so each successive [`crate::ErasedProvider::open_at_reusing`] resets the retained workspaces instead
/// of allocating and dropping a session per value. The residual requirement is rebuilt from the live handle each open
/// — count, type, and element hints are not recycle keys, so a cached residual would reopen under the previous value's
/// profile.
///
/// The retained carrier stays ledger-visible for as long as this slot lives: it is retained capacity, not per-value
/// working charge, bounded by one session — never by the number of values decoded through the slot.
#[derive(Debug, Default)]
pub struct ReusableAccessSession<'source> {
    session: Option<ErasedAccessSession<'source>>,
    key: Option<(u64, RouteSlot, ResidualKey)>,
}

impl ReusableAccessSession<'_> {
    /// An empty slot; the first open constructs the session it then recycles.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            session: None,
            key: None,
        }
    }

    /// Drops the recycled session, releasing its retained charge before the request ends.
    pub fn release(&mut self) {
        self.session = None;
        self.key = None;
    }
}

impl<'source> crate::ErasedProvider<'source> {
    /// Constructs a source-backed downstream provider behind a `Box<dyn InputProvider>` trait object.
    pub fn try_new_provider<T, F>(
        source: ResolvedSource<'source>,
        _resources: &ResourceContext<'_>,
        constructor: F,
    ) -> Result<Self, CodecError>
    where
        T: InputProvider,
        F: FnOnce() -> Result<T, CodecError>,
    {
        let provider = Self::try_new_with(source, constructor)?;
        let routes = provider.route_descriptions();
        validate_routes(routes)?;
        if !support_profile_compatible(provider.supports_attribute_absence(), routes) {
            return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                contract: "provider support profile contradicts executable routes",
            }));
        }
        Ok(provider)
    }

    /// Inspects immutable route descriptions without reading source bytes.
    #[must_use]
    pub fn route_descriptions(&self) -> &[RouteDescription] {
        self.owner.route_descriptions()
    }
    /// Whether the format authoritatively lacks markup attributes.
    #[must_use]
    pub fn supports_attribute_absence(&self) -> bool {
        self.owner.supports_attribute_absence()
    }

    /// Deterministically binds a requirement to one advertised route: the first bundle matching every contract leg
    /// wins, and a requirement no direct leg serves falls through the documented adapters (complete-document exact
    /// selection, attribute absence, then the whole-document demand fallback). A demand clause no advertised bundle
    /// covers refuses here rather than binding a handle the open-time plan re-check would reject.
    pub fn bind<'requirement>(
        &self,
        requirement: &'requirement AccessRequirement,
    ) -> Result<AccessHandle<'requirement>, AccessBindError> {
        bind_route(
            self.provider_id,
            self.route_descriptions(),
            self.supports_attribute_absence(),
            requirement,
        )
    }

    /// Validates and opens exactly one sealed route; dispatch failure never falls back.
    pub fn open(
        &mut self,
        handle: &AccessHandle<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedAccessSession<'source>, CodecError> {
        self.open_from(handle, self.source, resources)
    }

    /// Validates and opens exactly one sealed route over the provider's already-charged source narrowed to start at
    /// `start_offset`.
    ///
    /// No additional input bytes are charged: [`Self::try_new_provider`] (via `create_provider`) charged the whole
    /// retained source exactly once at construction. A caller decoding that source as several adjacent complete values
    /// (e.g. NDJSON / space-separated JSON texts) uses this to open each successive value at its own start offset
    /// without re-charging bytes already paid for, while every opened session remains its own independently accounted,
    /// verified result. Requires a route whose guarantees permit reopening; the same requirement/handle is bound once
    /// and reused across calls.
    pub fn open_at(
        &mut self,
        handle: &AccessHandle<'_>,
        start_offset: u64,
        resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedAccessSession<'source>, CodecError> {
        let narrowed = self.narrowed_source(start_offset)?;
        self.open_from(handle, narrowed, resources)
    }

    /// Opens the same sealed route as [`Self::open_at`] over a session RECYCLED from the previous adjacent value
    /// instead of a fresh one.
    ///
    /// A long adjacent-value sequence otherwise pays one session construction and one session teardown per value —
    /// carrier allocation, workspace allocation, and the residual requirement derived from the bound handle, all
    /// rebuilt for input that never changed shape. `reuse` carries those across values: the residual requirement is
    /// derived once per provider/slot, and the concrete session is reset in place through
    /// [`InputProvider::try_reopen_route`], keeping its retained workspaces.
    ///
    /// The recycled state is otherwise the fresh state exactly: the core lifecycle is reset before it is sealed again,
    /// so a value that terminalized, completed, or published nothing cannot affect the next one. A provider that
    /// declines to recycle (the default) simply gets a fresh session, so this never changes what any route observes —
    /// only how many times its workspaces are allocated.
    ///
    /// # Errors
    ///
    /// Returns the same failures as [`Self::open_at`].
    pub fn open_at_reusing<'reuse>(
        &mut self,
        handle: &AccessHandle<'_>,
        start_offset: u64,
        reuse: &'reuse mut ReusableAccessSession<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<&'reuse mut ErasedAccessSession<'source>, CodecError> {
        let source = self.narrowed_source(start_offset)?;
        self.open_reusing_from(handle, source, reuse, resources)
    }

    /// Shared body of [`Self::open_at_reusing`] and [`Self::open_range_reusing`].
    fn open_reusing_from<'reuse>(
        &mut self,
        handle: &AccessHandle<'_>,
        source: ResolvedSource<'source>,
        reuse: &'reuse mut ReusableAccessSession<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<&'reuse mut ErasedAccessSession<'source>, CodecError> {
        let slot = self.validated_slot(handle)?;
        let key = handle.residual_key;
        // ResidualKey omits count/type/element so the same carrier can be offered across those hints; the residual
        // itself is always rebuilt from the live handle so try_reopen_route sees the new profile and can decline.
        if reuse.key != Some((self.provider_id, slot, key)) {
            reuse.release();
        }
        let residual = residual_requirement(handle.requirement, handle.adapter, resources)?;
        // The cached session stops matching until the reopen and the seal below both succeed: a failure between here
        // and the restore at the end must leave nothing half-reopened under a still-matching key.
        let matched = reuse.session.is_some();
        reuse.key = None;
        let recycled = match reuse.session.as_mut().filter(|_| matched) {
            Some(session) => self
                .owner
                .try_reopen_route(&mut session.recycled_state(), slot, &residual, resources)?,
            None => false,
        };
        let session = if recycled {
            let session = reuse.session.as_mut().ok_or_else(|| {
                CodecError::new(CodecFailureKind::InternalContractViolation {
                    contract: "recycled access session retention",
                })
            })?;
            session.reset_for_reuse(source);
            self.seal_opened(session, handle, slot, resources)?;
            session
        } else {
            // Seal BEFORE storing: a seal failure must leave the previous recycled session/key pair intact, not a
            // half-adopted fresh one.
            let input = ProviderInput::new(source);
            let mut session = self.owner.open_route(input, slot, &residual, resources)?;
            self.seal_opened(&mut session, handle, slot, resources)?;
            reuse.session.insert(session)
        };
        reuse.key = Some((self.provider_id, slot, key));
        Ok(session)
    }

    /// Opens the same sealed route as [`Self::open_at_reusing`] over the provider's already-charged source narrowed to
    /// the EXACT byte range `start_offset..end_offset`.
    ///
    /// This is the record-route entry point. A record framer has already proven where one record's payload ends, so the
    /// payload must be decoded as a COMPLETE document over exactly those bytes rather than as an adjacent value that
    /// merely starts there: with `allow_adjacent_values: false`, a second value or any trailing non-whitespace byte
    /// inside the payload raises strict JSON's ordinary trailing-content diagnostic at its true absolute offset, which
    /// is exactly the record law "one payload holds one complete text".
    ///
    /// No input bytes are charged: the whole retained source was charged once at provider construction. The narrowed
    /// view keeps the source's absolute base offset, so every diagnostic span the route produces still names positions
    /// in the ORIGINAL source, not in the record.
    ///
    /// # Errors
    ///
    /// Returns the same failures as [`Self::open_at_reusing`], plus [`CodecFailureKind::Overflow`] for a range outside
    /// the retained source or with `end_offset < start_offset`.
    pub fn open_range_reusing<'reuse>(
        &mut self,
        handle: &AccessHandle<'_>,
        start_offset: u64,
        end_offset: u64,
        reuse: &'reuse mut ReusableAccessSession<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<&'reuse mut ErasedAccessSession<'source>, CodecError> {
        let source = self.narrowed_range(start_offset, end_offset)?;
        self.open_reusing_from(handle, source, reuse, resources)
    }

    /// Narrows the provider's already-charged source to `start..end`.
    fn narrowed_range(&self, start_offset: u64, end_offset: u64) -> Result<ResolvedSource<'source>, CodecError> {
        let overflow = || CodecError::new(CodecFailureKind::Overflow);
        let start = usize::try_from(start_offset).map_err(|_| overflow())?;
        let end = usize::try_from(end_offset).map_err(|_| overflow())?;
        if end < start {
            return Err(overflow());
        }
        let bytes = self.source.bytes().get(start..end).ok_or_else(overflow)?;
        Ok(ResolvedSource::new(
            self.source.source(),
            self.source.label(),
            bytes,
            self.source
                .base_offset()
                .checked_add(start_offset)
                .ok_or_else(overflow)?,
        ))
    }

    /// Narrows the provider's already-charged source to start at `start_offset`.
    fn narrowed_source(&self, start_offset: u64) -> Result<ResolvedSource<'source>, CodecError> {
        self.narrowed_range(start_offset, self.source.bytes().len() as u64)
    }

    /// Revalidates accounts, handle identity, and the sealed route bundle, and returns the provider-local slot the
    /// dispatch must open.
    fn validated_slot(&self, handle: &AccessHandle<'_>) -> Result<RouteSlot, CodecError> {
        if handle.provider_id != self.provider_id {
            return Err(CodecError::new(CodecFailureKind::ProviderRouteMismatch));
        }
        let route = resolve_sealed_route(self.route_descriptions(), handle)?;
        let support = self.supports_attribute_absence();
        if !support_profile_compatible(support, self.route_descriptions())
            || !plan_compatible(route.bundle(), handle.requirement, handle.adapter, support)
        {
            return Err(CodecError::new(CodecFailureKind::ProviderRouteMismatch));
        }
        Ok(route.slot())
    }

    /// Applies the coverage, plan, and route-receipt seals one opened session carries for the exact bound requirement.
    fn seal_opened(
        &self,
        session: &mut ErasedAccessSession<'source>,
        handle: &AccessHandle<'_>,
        slot: RouteSlot,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        session.seal_required_coverage(crate::access::required_coverage(handle.requirement, handle.adapter))?;
        session.seal_plan(
            handle.requirement,
            handle.adapter,
            matches!(
                handle.adapter,
                AccessAdapter::CompleteDocumentExact | AccessAdapter::CompleteDocumentExactWithAttributeAbsence
            ),
            resources,
        )?;
        session.seal_route_receipt(self.provider_id, slot)
    }

    fn open_from(
        &mut self,
        handle: &AccessHandle<'_>,
        source: ResolvedSource<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedAccessSession<'source>, CodecError> {
        let slot = self.validated_slot(handle)?;
        let input = ProviderInput::new(source);
        let residual = residual_requirement(handle.requirement, handle.adapter, resources)?;
        let mut session = self.owner.open_route(input, slot, &residual, resources)?;
        self.seal_opened(&mut session, handle, slot, resources)?;
        Ok(session)
    }
}

pub(crate) fn validate_routes(routes: &[RouteDescription]) -> Result<(), CodecError> {
    for (index, route) in routes.iter().enumerate() {
        let expected = u32::try_from(index).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?;
        if route.slot() != RouteSlot::new(expected) {
            return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                contract: "provider route slots must be unique and dense",
            }));
        }
    }
    Ok(())
}

fn support_profile_compatible(support: bool, routes: &[RouteDescription]) -> bool {
    !routes.iter().any(|route| {
        support
            && route
                .bundle()
                .demand()
                .clauses()
                .iter()
                .any(|clause| matches!(clause, crate::DemandClause::Attribute(_)))
    })
}

fn bind_route<'requirement>(
    provider_id: u64,
    routes: &[RouteDescription],
    support: bool,
    requirement: &'requirement AccessRequirement,
) -> Result<AccessHandle<'requirement>, AccessBindError> {
    if !support_profile_compatible(support, routes) {
        return Err(AccessBindError::InvalidRequirement);
    }
    let first = routes.first().ok_or(AccessBindError::NoRoutes)?;
    let mut first_mismatch = None;
    for description in routes {
        if let Some(mismatch) = requirement.first_mismatch(description.bundle()) {
            first_mismatch.get_or_insert(AccessBindError::HardMismatch {
                slot: description.slot(),
                mismatch,
            });
            continue;
        }
        return Ok(bound_handle(
            provider_id,
            description.slot(),
            requirement,
            AccessAdapter::None,
        ));
    }
    // No shortcut past a failed leg here, not even for an advertised Exact slot: every open re-runs these same legs
    // through the plan compatibility check, so a handle bound while the demand leg disagrees can never be opened. An
    // uncovered clause falls through to the adapters below and refuses at bind if they cannot serve it either.
    if requirement.result() == AccessResultKind::Located {
        let wants_attribute = requirement
            .demand()
            .clauses()
            .iter()
            .any(|clause| matches!(clause, crate::DemandClause::Attribute(_)));
        let absence = wants_attribute && support;
        for description in routes {
            let bundle = description.bundle();
            let adapter = match (absence, bundle.footprint(), bundle.result()) {
                (false, AccessFootprintKind::Whole, AccessResultKind::CompleteDocument) => {
                    AccessAdapter::CompleteDocumentExact
                }
                (true, AccessFootprintKind::Whole, AccessResultKind::CompleteDocument) => {
                    AccessAdapter::CompleteDocumentExactWithAttributeAbsence
                }
                (true, AccessFootprintKind::Exact, AccessResultKind::Located) => AccessAdapter::AttributeAbsence,
                _ => continue,
            };
            if plan_compatible(bundle, requirement, adapter, support) {
                return Ok(bound_handle(provider_id, description.slot(), requirement, adapter));
            }
        }
    }
    // THE WHOLE-DOCUMENT DEMAND FALLBACK: binding can never fail for capability reasons. The requirement's result
    // authority is a HINT the provider may honor or ignore — a provider that advertises nothing more specific serves
    // the lazy whole document and the consumer runs the whole program against it, so a query over a sparse codec takes
    // the same shape it takes over a rich one and differs only in how much the codec chooses to defer. The demand
    // CLAUSES still bind (a provider that cannot retain a fact the program reads must not serve a document without it);
    // only the result authority falls back. The residual requirement keeps the lazy frontier, so slot 0 opens exactly
    // as it does for an ordinary root request.
    for description in routes {
        let bundle = description.bundle();
        if plan_compatible(bundle, requirement, AccessAdapter::CompleteDocumentDemand, support) {
            return Ok(bound_handle(
                provider_id,
                description.slot(),
                requirement,
                AccessAdapter::CompleteDocumentDemand,
            ));
        }
    }
    Err(first_mismatch.unwrap_or(AccessBindError::HardMismatch {
        slot: first.slot(),
        mismatch: crate::CapabilityMismatch::Demand,
    }))
}

fn bound_handle(
    provider_id: u64,
    slot: RouteSlot,
    requirement: &AccessRequirement,
    adapter: AccessAdapter,
) -> AccessHandle<'_> {
    AccessHandle {
        provider_id,
        slot,
        requirement,
        adapter,
        residual_key: residual_key(requirement, adapter),
    }
}

fn resolve_sealed_route<'route>(
    routes: &'route [RouteDescription],
    handle: &AccessHandle<'_>,
) -> Result<&'route RouteDescription, CodecError> {
    let route = routes
        .get(usize::try_from(handle.slot.get()).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?)
        .filter(|route| route.slot() == handle.slot)
        .ok_or_else(|| CodecError::new(CodecFailureKind::ProviderRouteMismatch))?;
    Ok(route)
}

fn plan_compatible(
    bundle: &crate::CapabilityBundle,
    requirement: &AccessRequirement,
    adapter: AccessAdapter,
    support: bool,
) -> bool {
    if adapter == AccessAdapter::None {
        return requirement.first_mismatch(bundle).is_none();
    }
    // NO-CORE-FALLBACK for range footprints. Every adapter below this line drives the core fallback interpreter over an
    // already-materialized document, and serving a range step there would mean reimplementing the len-relative slice
    // law in the format-neutral core. It is declined at the PLAN gate instead, so the failure is a clean bind mismatch
    // and the caller keeps its ordinary route.
    if requirement
        .footprint()
        .exact_path()
        .is_some_and(crate::ExactPath::has_semantic_range)
    {
        return false;
    }
    let absence = matches!(
        adapter,
        AccessAdapter::AttributeAbsence | AccessAdapter::CompleteDocumentExactWithAttributeAbsence
    );
    if absence && !support {
        return false;
    }
    let structural = match adapter {
        AccessAdapter::CompleteDocumentExact
        | AccessAdapter::CompleteDocumentExactWithAttributeAbsence
        | AccessAdapter::CompleteDocumentDemand => {
            bundle.footprint() == AccessFootprintKind::Whole && bundle.result() == AccessResultKind::CompleteDocument
        }
        AccessAdapter::AttributeAbsence => {
            bundle.footprint() == AccessFootprintKind::Exact && bundle.result() == AccessResultKind::Located
        }
        // Unreachable: `None` early-returns above; the arm keeps the match total.
        AccessAdapter::None => true,
    };
    structural
        && demand_implies_residual(bundle.demand(), requirement.demand())
        && bundle.validation() == requirement.validation()
        && bundle.diagnostics() == requirement.diagnostics()
}

fn demand_implies_residual(available: &crate::CodecDemand, required: &crate::CodecDemand) -> bool {
    required.clauses().iter().all(|needle| {
        matches!(needle, crate::DemandClause::Attribute(_))
            || available.clauses().iter().any(|candidate| candidate == needle)
    })
}

fn residual_requirement(
    requirement: &AccessRequirement,
    adapter: AccessAdapter,
    resources: &ResourceContext<'_>,
) -> Result<AccessRequirement, CodecError> {
    let omit_attributes = matches!(
        adapter,
        AccessAdapter::AttributeAbsence | AccessAdapter::CompleteDocumentExactWithAttributeAbsence
    );
    let complete = matches!(
        adapter,
        AccessAdapter::CompleteDocumentExact | AccessAdapter::CompleteDocumentExactWithAttributeAbsence
    );
    let demand = if omit_attributes {
        requirement.demand().try_clone_without_attributes(resources)?
    } else {
        requirement.demand().try_clone_in(resources)?
    };
    // The residual requirement is a RECONSTRUCTION (the adapter may swap the footprint/schedule/result shape), so every
    // carried field must be copied explicitly — including the lazy-frontier depth, which the whole-document route
    // reads at open. Dropping it here would silently make every request eager regardless of the engine's route
    // decision.
    let guarantees = crate::AccessGuarantees::new(requirement.validation(), requirement.diagnostics());
    let rebuilt = if complete || requirement.footprint().is_whole() {
        AccessRequirement::try_whole(demand, guarantees, resources)
    } else {
        let footprint = requirement.footprint().try_clone_in(resources)?;
        // The residual carries the AUTHORED singleton origin, not a fresh origin 0: session reuse keys on that origin
        // (residual_key), so a rebuild that renumbered the schedule would hand the provider a schedule its own key does
        // not describe.
        let origin = requirement
            .schedule()
            .singleton_origin()
            .unwrap_or_else(|| crate::SelectionOrigin::new(0));
        AccessRequirement::try_exact_with_origin(footprint, origin, demand, guarantees, resources)
    };
    rebuilt.and_then(|residual| {
        let residual = residual
            .with_lazy_frontier(requirement.lazy_frontier())
            .with_canonicality_probe(requirement.canonicality_probe())
            .with_authored_spans(requirement.authored_spans())
            .with_fact_intent(requirement.fact_intent());
        let residual = match requirement.count() {
            Some(demand) => residual.with_count(demand.clone()),
            None => residual,
        };
        let residual = if requirement.type_demand() {
            residual.with_type_demand()
        } else {
            residual
        };
        let residual = match requirement.element() {
            Some(demand) => residual.with_element(demand.clone()),
            None => residual,
        };
        if let Some(tree) = requirement.prune() {
            let clone = tree
                .try_clone_in(resources)
                .map_err(|error| map_residual_prune_error(error, "residual prune-tree clone"))?;
            Ok(residual.with_prune(clone))
        } else if !complete {
            Ok(residual)
        } else if let Some(path) = requirement.footprint().exact_path() {
            match crate::PruneTree::try_from_exact_path(path, resources) {
                Ok(Some(tree)) => Ok(residual.with_prune(tree)),
                Ok(None) => Ok(residual),
                Err(error) => Err(map_residual_prune_error(error, "path-spine prune tree")),
            }
        } else {
            Ok(residual)
        }
    })
}

fn map_residual_prune_error(error: crate::PruneTreeError, contract: &'static str) -> CodecError {
    match error {
        crate::PruneTreeError::Resource(resource) => CodecError::from(resource),
        _ => CodecError::new(CodecFailureKind::InternalContractViolation { contract }),
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::{bind_route, plan_compatible};
    use crate::capability::{AccessFootprintKind, AccessResultKind};
    use crate::pattern::{AccessFootprint, AccessFootprintFingerprint, ExactPath};
    use crate::test_support::resources;
    use crate::{
        AccessAdapter, AccessBindError, AccessGuarantees, AccessInput, AccessOutcome, AccessRequirement, AccessResult,
        AccessSession, CapabilityBundle, CodecDemand, CodecError, CodecFailureKind, CodecRunContext, DemandClause,
        DiagnosticPolicy, DocumentProduct, ErasedAccessSession, ErasedProvider, ExactSelectionRecord, FactIntent,
        InputProvider, LocatedOutcome, ProviderInput, ReusableAccessSession, RouteDescription, RouteSlot,
        SelectionOrigin,
    };
    use alloc::vec;
    use alloc::vec::Vec;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use jqf_data::{
        AccountedDocumentBuilder, AccountedSemanticNode, AuthoritativeEmptyFamilies, DiagnosticCoverage,
        DocumentCapabilityFamily, ExpandedName, FactKindId, FactRoleId,
    };
    use jqf_resource::{ResourceContext, WorkAdmission};
    use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

    static MUTATED_PROFILE: AtomicBool = AtomicBool::new(false);
    static OPEN_CALLED: AtomicBool = AtomicBool::new(false);
    static FAILING_REOPEN_OPENS: AtomicUsize = AtomicUsize::new(0);
    static SOURCE_POLLS: AtomicUsize = AtomicUsize::new(0);
    static RESIDUAL_ATTRIBUTE_SEEN: AtomicBool = AtomicBool::new(false);
    static SOURCE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static RESIDUAL_FINGERPRINTS: std::sync::Mutex<Vec<AccessFootprintFingerprint>> = std::sync::Mutex::new(Vec::new());

    struct MutableProfileProvider {
        initial: Vec<RouteDescription>,
        mutated: Vec<RouteDescription>,
    }

    /// Records the residual requirement fingerprint each open receives, so a test can prove a recycled session never
    /// served one requirement's residual to a different requirement on the same slot.
    struct ResidualRecordingProvider {
        routes: Vec<RouteDescription>,
    }

    struct FailingReopenProvider {
        routes: Vec<RouteDescription>,
    }

    impl InputProvider for ResidualRecordingProvider {
        fn route_descriptions(&self) -> &[RouteDescription] {
            self.routes.as_slice()
        }

        fn open_route<'source>(
            &mut self,
            input: ProviderInput<'source>,
            _slot: RouteSlot,
            requirement: &AccessRequirement,
            resources: &mut ResourceContext<'_>,
        ) -> Result<ErasedAccessSession<'source>, CodecError> {
            RESIDUAL_FINGERPRINTS
                .lock()
                .expect("residual fingerprints")
                .push(requirement.footprint().fingerprint());
            let product = source_product(resources, false, DiagnosticCoverage::NotRequested);
            ErasedAccessSession::try_new_source(input.source(), || {
                Ok(SourceFullState {
                    product: Some(product),
                    behavior: SourceBehavior::Ready,
                })
            })
        }
    }

    impl InputProvider for FailingReopenProvider {
        fn route_descriptions(&self) -> &[RouteDescription] {
            self.routes.as_slice()
        }

        fn open_route<'source>(
            &mut self,
            input: ProviderInput<'source>,
            _slot: RouteSlot,
            _requirement: &AccessRequirement,
            resources: &mut ResourceContext<'_>,
        ) -> Result<ErasedAccessSession<'source>, CodecError> {
            FAILING_REOPEN_OPENS.fetch_add(1, Ordering::SeqCst);
            let product = source_product(resources, false, DiagnosticCoverage::NotRequested);
            ErasedAccessSession::try_new_source(input.source(), || {
                Ok(SourceFullState {
                    product: Some(product),
                    behavior: SourceBehavior::Ready,
                })
            })
        }

        fn try_reopen_route(
            &mut self,
            _state: &mut crate::RecycledSessionState<'_>,
            _slot: RouteSlot,
            _requirement: &AccessRequirement,
            _resources: &mut ResourceContext<'_>,
        ) -> Result<bool, CodecError> {
            Err(CodecError::new(CodecFailureKind::InvalidInput))
        }
    }

    impl InputProvider for MutableProfileProvider {
        fn route_descriptions(&self) -> &[RouteDescription] {
            if MUTATED_PROFILE.load(Ordering::SeqCst) {
                self.mutated.as_slice()
            } else {
                self.initial.as_slice()
            }
        }

        fn open_route<'source>(
            &mut self,
            _input: ProviderInput<'source>,
            _slot: RouteSlot,
            _requirement: &AccessRequirement,
            _resources: &mut ResourceContext<'_>,
        ) -> Result<ErasedAccessSession<'source>, CodecError> {
            OPEN_CALLED.store(true, Ordering::SeqCst);
            Err(CodecError::new(CodecFailureKind::InvalidInput))
        }
    }

    #[derive(Clone, Copy)]
    enum SourceBehavior {
        ErrorThenReady,
        Ready,
        ReadyAfterConsumingCredit,
    }

    struct SourceFullState {
        product: Option<DocumentProduct<'static>>,
        behavior: SourceBehavior,
    }

    impl AccessSession for SourceFullState {
        fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
            self
        }
        fn decode<'source>(
            &mut self,
            _input: AccessInput<'_, 'source>,
            context: &mut CodecRunContext<'_, '_>,
        ) -> Result<AccessResult<'source>, CodecError> {
            let poll = SOURCE_POLLS.fetch_add(1, Ordering::SeqCst);
            if matches!(self.behavior, SourceBehavior::ErrorThenReady) && poll == 0 {
                return Err(CodecError::new(CodecFailureKind::InvalidInput));
            }
            if matches!(self.behavior, SourceBehavior::ReadyAfterConsumingCredit) {
                assert_eq!(
                    context.resources().admit_work_transition().expect("work admission"),
                    WorkAdmission::Granted(1)
                );
            }
            let product = self
                .product
                .take()
                .ok_or_else(|| CodecError::new(CodecFailureKind::ProviderRouteMismatch))?;
            Ok(AccessResult::from_outcome(AccessOutcome::FullDocument(product)))
        }
    }

    struct SourceFullProvider {
        routes: Vec<RouteDescription>,
        product: Option<DocumentProduct<'static>>,
        behavior: SourceBehavior,
        absence: bool,
    }

    impl InputProvider for SourceFullProvider {
        fn route_descriptions(&self) -> &[RouteDescription] {
            self.routes.as_slice()
        }

        fn supports_attribute_absence(&self) -> bool {
            self.absence
        }

        fn open_route<'source>(
            &mut self,
            input: ProviderInput<'source>,
            _slot: RouteSlot,
            requirement: &AccessRequirement,
            _resources: &mut ResourceContext<'_>,
        ) -> Result<ErasedAccessSession<'source>, CodecError> {
            RESIDUAL_ATTRIBUTE_SEEN.store(
                requirement
                    .demand()
                    .clauses()
                    .iter()
                    .any(|clause| matches!(clause, DemandClause::Attribute(_))),
                Ordering::SeqCst,
            );
            let product = self
                .product
                .take()
                .ok_or_else(|| CodecError::new(CodecFailureKind::ProviderRouteMismatch))?;
            let behavior = self.behavior;
            ErasedAccessSession::try_new_source(input.source(), || {
                Ok(SourceFullState {
                    product: Some(product),
                    behavior,
                })
            })
        }
    }

    /// Serves an EXACT slot directly: the session publishes one Located observation over its own product, as a real
    /// provider's scoped route would, so a direct bind can be opened and decoded end-to-end.
    struct LocatedRouteProvider {
        routes: Vec<RouteDescription>,
        product: Option<DocumentProduct<'static>>,
        origin: SelectionOrigin,
    }

    impl InputProvider for LocatedRouteProvider {
        fn route_descriptions(&self) -> &[RouteDescription] {
            self.routes.as_slice()
        }

        fn open_route<'source>(
            &mut self,
            input: ProviderInput<'source>,
            _slot: RouteSlot,
            _requirement: &AccessRequirement,
            _resources: &mut ResourceContext<'_>,
        ) -> Result<ErasedAccessSession<'source>, CodecError> {
            let product = self
                .product
                .take()
                .ok_or_else(|| CodecError::new(CodecFailureKind::ProviderRouteMismatch))?;
            let origin = self.origin;
            ErasedAccessSession::try_new_source(input.source(), || {
                Ok(LocatedStubState {
                    product: Some(product),
                    origin,
                })
            })
        }
    }

    struct LocatedStubState {
        product: Option<DocumentProduct<'static>>,
        origin: SelectionOrigin,
    }

    impl AccessSession for LocatedStubState {
        fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
            self
        }

        fn decode<'source>(
            &mut self,
            _input: AccessInput<'_, 'source>,
            _context: &mut CodecRunContext<'_, '_>,
        ) -> Result<AccessResult<'source>, CodecError> {
            let product = self
                .product
                .take()
                .ok_or_else(|| CodecError::new(CodecFailureKind::ProviderRouteMismatch))?;
            let node = product.document().root_handle();
            let record = ExactSelectionRecord::Node {
                node,
                origin: self.origin,
            };
            let located = LocatedOutcome::try_new(&product, record)?;
            Ok(AccessResult::from_outcome(AccessOutcome::Located(located)))
        }
    }

    fn source_product(
        resources: &ResourceContext<'_>,
        attribute_absence: bool,
        diagnostics: DiagnosticCoverage,
    ) -> DocumentProduct<'static> {
        let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder");
        if attribute_absence {
            builder.set_authoritative_empty_families(AuthoritativeEmptyFamilies::from_family(
                DocumentCapabilityFamily::Attributes,
            ));
        }
        builder.set_diagnostic_coverage(diagnostics);
        let root = builder
            .add_node("test.bool", AccountedSemanticNode::Bool(true), None, resources)
            .expect("root");
        DocumentProduct::try_new(builder.finish(root, resources).expect("document"), resources).expect("product")
    }

    fn whole_routes(resources: &ResourceContext<'_>, diagnostics: DiagnosticPolicy) -> Vec<RouteDescription> {
        let routes = vec![RouteDescription::new(
            RouteSlot::new(0),
            CapabilityBundle::new(
                AccessFootprintKind::Whole,
                AccessResultKind::CompleteDocument,
                CodecDemand::try_new(resources),
                AccessGuarantees::strict(diagnostics),
            ),
        )];
        routes
    }

    /// The standard two-slot table as every production codec advertises it: `try_table` stamps the minimal semantic
    /// pair on each row, so a test binding against a realistic surface must carry those demands too.
    fn standard_routes(resources: &ResourceContext<'_>) -> Vec<RouteDescription> {
        let demand = |resources: &ResourceContext<'_>| {
            let mut demand = CodecDemand::try_new(resources);
            demand.try_insert(&DemandClause::SemanticRoot).expect("root clause");
            demand.try_insert(&DemandClause::ValueShape).expect("shape clause");
            demand
        };
        vec![
            RouteDescription::new(
                RouteSlot::new(0),
                CapabilityBundle::new(
                    AccessFootprintKind::Whole,
                    AccessResultKind::CompleteDocument,
                    demand(resources),
                    AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
                ),
            ),
            RouteDescription::new(
                RouteSlot::new(1),
                CapabilityBundle::new(
                    AccessFootprintKind::Exact,
                    AccessResultKind::Located,
                    demand(resources),
                    AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
                ),
            ),
        ]
    }

    fn exact_source_requirement(
        resources: &ResourceContext<'_>,
        attribute: bool,
        diagnostics: DiagnosticPolicy,
        with_step: bool,
    ) -> AccessRequirement {
        let mut path = ExactPath::try_new(resources);
        if with_step {
            path.try_push_semantic_member("missing", resources).expect("step");
        }
        let footprint = AccessFootprint::try_exact(path, resources);
        let mut demand = CodecDemand::try_new(resources);
        if attribute {
            demand
                .try_insert(&DemandClause::Attribute(
                    ExpandedName::try_new("urn:test", "id").expect("attribute"),
                ))
                .expect("demand");
        }
        AccessRequirement::try_exact(footprint, demand, AccessGuarantees::strict(diagnostics), resources)
            .expect("requirement")
    }

    /// Residual-hints receipt: `residual_requirement` RECONSTRUCTS the requirement for the fallback path, so every
    /// carried field must be copied explicitly — a new [`AccessRequirement`] hint that skips the copy dies silently
    /// there. This receipt pins every known carried field through the complete-document adapter (the one that swaps the
    /// most).
    #[test]
    fn residual_requirement_carries_every_hint() {
        let resources = resources();
        let prune = crate::PruneTree::try_new(&resources).expect("prune");
        let demand = jqf_data::CountDemand {
            row: jqf_data::CountRow::Container,
            path: Vec::new(),
            range: None,
            probe: Vec::new(),
            filter: None,
        };
        let element = jqf_data::ElementDemand {
            row: jqf_data::ElementRow::FanOut,
            path: Vec::new(),
            range: None,
            probe: jqf_data::ElementProbe::Length,
            increment: None,
            filter: None,
        };
        let requirement = exact_source_requirement(&resources, false, DiagnosticPolicy::ErrorsOnly, false)
            .with_lazy_frontier(7)
            .with_prune(prune)
            .with_count(demand)
            .with_element(element)
            .with_canonicality_probe(true)
            .with_type_demand()
            .with_authored_spans(false)
            .with_fact_intent(FactIntent::Preserve);
        let residual = super::residual_requirement(&requirement, AccessAdapter::CompleteDocumentExact, &resources)
            .expect("residual");
        assert_eq!(residual.result(), AccessResultKind::CompleteDocument);
        assert_eq!(residual.validation(), requirement.validation());
        assert_eq!(residual.diagnostics(), requirement.diagnostics());
        assert_eq!(residual.lazy_frontier(), 7);
        assert!(residual.prune().is_some(), "prune hint dropped by residual");
        assert!(
            residual.count().is_some(),
            "count hint dropped by the residual reconstruction"
        );
        assert!(
            residual.type_demand(),
            "type hint dropped by the residual reconstruction"
        );
        assert!(
            residual.element().is_some(),
            "element hint dropped by the residual reconstruction"
        );
        assert!(
            residual.canonicality_probe(),
            "canonicality probe dropped by the residual reconstruction"
        );
        assert!(
            !residual.authored_spans(),
            "cleared authored-span hint reverted to the default by the residual reconstruction"
        );
        assert_eq!(
            residual.fact_intent(),
            FactIntent::Preserve,
            "fact intent dropped by the residual reconstruction"
        );
    }

    /// The residual reconstruction must preserve the AUTHORED singleton origin. Session reuse keys on that origin
    /// (`residual_key`), so a rebuild that silently renumbered the schedule would hand the provider a schedule its own
    /// reuse key does not describe.
    #[test]
    fn residual_requirement_preserves_the_authored_singleton_origin() {
        let resources = resources();
        let footprint = AccessFootprint::try_exact(ExactPath::try_new(&resources), &resources);
        let requirement = AccessRequirement::try_exact_with_origin(
            footprint,
            crate::SelectionOrigin::new(7),
            CodecDemand::try_new(&resources),
            AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            &resources,
        )
        .expect("requirement");
        assert_eq!(
            requirement.schedule().singleton_origin(),
            Some(crate::SelectionOrigin::new(7))
        );
        let residual = super::residual_requirement(&requirement, AccessAdapter::None, &resources).expect("residual");
        assert_eq!(
            residual.schedule().singleton_origin(),
            Some(crate::SelectionOrigin::new(7)),
            "the residual rebuild renumbered the authored selection origin"
        );
    }

    #[test]
    fn exact_fallback_residual_synthesizes_path_spine_prune() {
        let resources = resources();
        let requirement = exact_source_requirement(&resources, false, DiagnosticPolicy::ErrorsOnly, true);
        assert!(requirement.prune().is_none());
        let routes = whole_routes(&resources, DiagnosticPolicy::ErrorsOnly);
        let handle = bind_route(1, &routes, false, &requirement).expect("bind");
        assert_eq!(handle.adapter, AccessAdapter::CompleteDocumentExact);
        let residual = super::residual_requirement(&requirement, handle.adapter, &resources).expect("residual");
        let tree = residual.prune().expect("path-spine prune");
        let child = tree.root().member(b"missing").expect("path key");
        assert!(tree.node(child).expect("leaf").is_all());
        assert!(tree.root().member(b"other").is_none());
        assert_ne!(
            crate::access::residual_key(&requirement, handle.adapter),
            crate::access::residual_key(
                &exact_source_requirement(&resources, false, DiagnosticPolicy::ErrorsOnly, false),
                handle.adapter
            )
        );
    }

    #[test]
    fn exact_fallback_leaves_engine_prune_in_place() {
        let resources = resources();
        let mut engine = crate::PruneTree::try_new(&resources).expect("tree");
        let keep = engine.try_push_node(true).expect("node");
        engine
            .try_push_key(crate::PruneTree::ROOT, "engine", keep)
            .expect("key");
        let requirement =
            exact_source_requirement(&resources, false, DiagnosticPolicy::ErrorsOnly, true).with_prune(engine);
        let residual = super::residual_requirement(&requirement, AccessAdapter::CompleteDocumentExact, &resources)
            .expect("residual");
        let tree = residual.prune().expect("engine prune");
        assert!(tree.root().member(b"engine").is_some());
        assert!(
            tree.root().member(b"missing").is_none(),
            "path-spine must not replace a more specific engine prune"
        );
    }

    #[test]
    fn residual_key_differs_when_prune_differs() {
        let resources = resources();
        let whole = |resources: &ResourceContext<'_>| {
            AccessRequirement::try_whole(
                CodecDemand::try_new(resources),
                AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
                resources,
            )
            .expect("requirement")
        };
        let mut tree_a = crate::PruneTree::try_new(&resources).expect("a");
        let keep_a = tree_a.try_push_node(true).expect("node");
        tree_a.try_push_key(crate::PruneTree::ROOT, "a", keep_a).expect("key");
        let mut tree_b = crate::PruneTree::try_new(&resources).expect("b");
        let keep_b = tree_b.try_push_node(true).expect("node");
        tree_b.try_push_key(crate::PruneTree::ROOT, "b", keep_b).expect("key");
        let bare = whole(&resources);
        let pruned_a = whole(&resources).with_prune(tree_a);
        let pruned_b = whole(&resources).with_prune(tree_b);
        assert_ne!(
            crate::access::residual_key(&bare, AccessAdapter::None),
            crate::access::residual_key(&pruned_a, AccessAdapter::None)
        );
        assert_ne!(
            crate::access::residual_key(&pruned_a, AccessAdapter::None),
            crate::access::residual_key(&pruned_b, AccessAdapter::None)
        );
    }

    #[test]
    fn exact_request_serves_complete_document_fallback_through_open() {
        static SOURCE: [u8; 0] = [];
        let _guard = SOURCE_TEST_LOCK.lock().expect("source test lock");
        let mut resources = resources();
        let path = ExactPath::try_new(&resources);
        let footprint = AccessFootprint::try_exact(path, &resources);
        let required_demand = CodecDemand::try_new(&resources);
        let requirement = AccessRequirement::try_exact(
            footprint,
            required_demand,
            AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            &resources,
        )
        .expect("requirement");
        let route_demand = CodecDemand::try_new(&resources);
        let routes = vec![RouteDescription::new(
            RouteSlot::new(0),
            CapabilityBundle::new(
                AccessFootprintKind::Whole,
                AccessResultKind::CompleteDocument,
                route_demand,
                AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            ),
        )];
        // OPEN what bind returned: a fallback handle that cannot survive open's own plan re-check is the defect shape
        // this suite exists to catch, so the adapter must actually serve, not merely bind.
        let product = source_product(&resources, false, DiagnosticCoverage::NotRequested);
        SOURCE_POLLS.store(0, Ordering::SeqCst);
        let source = ResolvedSource::new(
            SourceRef::new(SourceId::new(54), SourceKind::Input),
            "fallback-open",
            &SOURCE,
            0,
        );
        let mut provider = ErasedProvider::try_new_provider(source, &resources, || {
            Ok(SourceFullProvider {
                routes,
                product: Some(product),
                behavior: SourceBehavior::Ready,
                absence: false,
            })
        })
        .expect("provider");
        let handle = provider.bind(&requirement).expect("fallback");
        assert_eq!(handle.adapter, AccessAdapter::CompleteDocumentExact);
        assert_eq!(handle.slot, RouteSlot::new(0));
        let mut session = provider.open(&handle, &mut resources).expect("open");
        let mut context = CodecRunContext::new(&mut resources);
        let result = session.decode(&mut context).expect("decode");
        assert_eq!(result.report().adapter(), AccessAdapter::CompleteDocumentExact);
        assert!(matches!(result.outcome(), AccessOutcome::Located(_)));
        assert_eq!(SOURCE_POLLS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn exact_request_opens_advertised_exact_slot_not_the_whole_walk() {
        static SOURCE: [u8; 0] = [];
        let mut resources = resources();
        let path = ExactPath::try_new(&resources);
        let footprint = AccessFootprint::try_exact(path, &resources);
        let mut required_demand = CodecDemand::try_new(&resources);
        required_demand
            .try_insert(&DemandClause::ValueShape)
            .expect("hint demand");
        let requirement = AccessRequirement::try_exact(
            footprint,
            required_demand,
            AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            &resources,
        )
        .expect("requirement");
        let routes = standard_routes(&resources);
        // The direct leg: the advertised Exact slot covers the requirement's demand, so binding takes it without any
        // adapter and never enters the whole-document walk.
        let handle = bind_route(8, &routes, false, &requirement).expect("advertised exact");
        assert_eq!(handle.slot, RouteSlot::new(1));
        assert_eq!(handle.adapter, AccessAdapter::None);
        // OPEN what bind returned: every open re-runs bind's compatibility legs, so only a handle that survives this
        // open was ever legal. Serving Located through the opened session proves the direct Exact bind end-to-end.
        let product = source_product(&resources, false, DiagnosticCoverage::NotRequested);
        let origin = requirement.schedule().singleton_origin().expect("singleton origin");
        let source = ResolvedSource::new(
            SourceRef::new(SourceId::new(53), SourceKind::Input),
            "advertised-exact",
            &SOURCE,
            0,
        );
        let mut provider = ErasedProvider::try_new_provider(source, &resources, || {
            Ok(LocatedRouteProvider {
                routes,
                product: Some(product),
                origin,
            })
        })
        .expect("provider");
        let handle = provider.bind(&requirement).expect("bind");
        let mut session = provider.open(&handle, &mut resources).expect("open");
        let mut context = CodecRunContext::new(&mut resources);
        let result = session.decode(&mut context).expect("decode");
        assert!(matches!(result.outcome(), AccessOutcome::Located(_)));
    }

    #[test]
    fn an_uncovered_demand_clause_refuses_at_bind_not_a_dead_handle() {
        let resources = resources();
        let path = ExactPath::try_new(&resources);
        let footprint = AccessFootprint::try_exact(path, &resources);
        let mut required_demand = CodecDemand::try_new(&resources);
        required_demand
            .try_insert(&DemandClause::IntrinsicTag)
            .expect("clause no route advertises");
        let requirement = AccessRequirement::try_exact(
            footprint,
            required_demand,
            AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            &resources,
        )
        .expect("requirement");
        // Both rows advertise the minimal pair only. The Exact slot agrees on footprint, result, validation, and
        // diagnostics but cannot serve the clause — and since open re-runs those legs before opening anything,
        // binding it anyway would hand out a handle that dies at open. The adapters decline for the same reason, so the
        // refusal happens HERE.
        let routes = standard_routes(&resources);
        assert!(matches!(
            bind_route(10, &routes, false, &requirement),
            Err(AccessBindError::HardMismatch { .. })
        ));
    }

    /// A requirement whose demand exceeds what the advertised Exact route serves must fail AT BIND with the prose
    /// `HardMismatch`. The Exact shortcut applies the same plan gate the sealed dispatch re-runs at open, so a
    /// demand-exceeding requirement can no longer bind cleanly through it and then fail `ProviderRouteMismatch` on
    /// every open.
    #[test]
    fn exact_shortcut_declines_a_requirement_exceeding_advertised_demand() {
        let resources = resources();
        let footprint = AccessFootprint::try_exact(ExactPath::try_new(&resources), &resources);
        let mut demand = CodecDemand::try_new(&resources);
        demand
            .try_insert(&DemandClause::Topology(crate::TopologyDemand::Children))
            .expect("topology demand");
        let requirement = AccessRequirement::try_exact(
            footprint,
            demand,
            AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            &resources,
        )
        .expect("requirement");
        // One advertised Exact slot whose demand carries no topology clause.
        let routes = [RouteDescription::new(
            RouteSlot::new(0),
            CapabilityBundle::new(
                AccessFootprintKind::Exact,
                AccessResultKind::Located,
                CodecDemand::try_new(&resources),
                AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            ),
        )];
        let error = bind_route(3, &routes, false, &requirement)
            .expect_err("a requirement beyond the advertisement must fail at bind");
        assert_eq!(
            error,
            crate::AccessBindError::HardMismatch {
                slot: RouteSlot::new(0),
                mismatch: crate::CapabilityMismatch::Demand,
            }
        );
        let prose = alloc::format!("{error}");
        assert!(
            prose.contains("information demand"),
            "the HardMismatch must render its axis as prose: {prose}"
        );
    }

    #[test]
    fn attribute_absence_is_an_executable_residual_adapter() {
        let resources = resources();
        let path = ExactPath::try_new(&resources);
        let footprint = AccessFootprint::try_exact(path, &resources);
        let mut demand = CodecDemand::try_new(&resources);
        demand
            .try_insert(&DemandClause::Attribute(
                ExpandedName::try_new("urn:test", "id").expect("expanded name"),
            ))
            .expect("attribute demand");
        let requirement = AccessRequirement::try_exact(
            footprint,
            demand,
            AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            &resources,
        )
        .expect("requirement");
        let routes = [RouteDescription::new(
            RouteSlot::new(0),
            CapabilityBundle::new(
                AccessFootprintKind::Whole,
                AccessResultKind::CompleteDocument,
                CodecDemand::try_new(&resources),
                AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            ),
        )];
        let support = true;
        let handle = bind_route(9, &routes, support, &requirement).expect("absence fallback");
        assert_eq!(handle.adapter, AccessAdapter::CompleteDocumentExactWithAttributeAbsence,);
        assert!(plan_compatible(
            routes[0].bundle(),
            &requirement,
            AccessAdapter::CompleteDocumentExactWithAttributeAbsence,
            support,
        ));
        assert!(!plan_compatible(
            routes[0].bundle(),
            &requirement,
            AccessAdapter::CompleteDocumentExactWithAttributeAbsence,
            false,
        ));
    }

    #[test]
    fn production_table_advertises_catalogued_facts_and_falls_back_for_attributes() {
        let resources = resources();
        let routes = RouteDescription::try_standard_document_table(
            AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            &resources,
        )
        .expect("table");
        let path = ExactPath::try_new(&resources);
        let footprint = AccessFootprint::try_exact(path, &resources);
        let mut demand = CodecDemand::try_new(&resources);
        let kind = FactKindId::try_new("comment").expect("kind");
        let role = FactRoleId::try_new("comment").expect("role");
        demand
            .try_insert(&DemandClause::AttachedFact { kind, role })
            .expect("insert");
        let requirement = AccessRequirement::try_exact(
            footprint,
            demand,
            AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            &resources,
        )
        .expect("requirement");
        let handle = bind_route(1, &routes, false, &requirement).expect("catalogued fact");
        assert_eq!(handle.slot, RouteSlot::new(1));
        assert_eq!(handle.adapter, AccessAdapter::None);

        let path = ExactPath::try_new(&resources);
        let footprint = AccessFootprint::try_exact(path, &resources);
        let mut demand = CodecDemand::try_new(&resources);
        let kind = FactKindId::try_new("not-a-catalog-role").expect("kind");
        let role = FactRoleId::try_new("not-a-catalog-role").expect("role");
        demand
            .try_insert(&DemandClause::AttachedFact { kind, role })
            .expect("insert");
        let requirement = AccessRequirement::try_exact(
            footprint,
            demand,
            AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            &resources,
        )
        .expect("requirement");
        assert!(matches!(
            bind_route(1, &routes, false, &requirement),
            Err(AccessBindError::HardMismatch { .. })
        ));

        let path = ExactPath::try_new(&resources);
        let footprint = AccessFootprint::try_exact(path, &resources);
        let mut demand = CodecDemand::try_new(&resources);
        demand
            .try_insert(&DemandClause::Attribute(
                ExpandedName::try_new("", "href").expect("attribute"),
            ))
            .expect("insert");
        let requirement = AccessRequirement::try_exact(
            footprint,
            demand,
            AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            &resources,
        )
        .expect("requirement");
        let handle = bind_route(1, &routes, false, &requirement).expect("attribute fallback");
        assert_eq!(handle.slot, RouteSlot::new(0));
        assert_eq!(handle.adapter, AccessAdapter::CompleteDocumentExact);
    }

    #[test]
    fn open_revalidates_a_mutated_direct_route_before_calling_provider() {
        let mut resources = resources();
        let requirement = {
            let footprint = AccessFootprint::try_exact(ExactPath::try_new(&resources), &resources);
            AccessRequirement::try_exact(
                footprint,
                CodecDemand::try_new(&resources),
                AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
                &resources,
            )
            .expect("requirement")
        };
        let initial = vec![RouteDescription::new(
            RouteSlot::new(0),
            CapabilityBundle::new(
                AccessFootprintKind::Exact,
                AccessResultKind::Located,
                CodecDemand::try_new(&resources),
                AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            ),
        )];
        let mutated = vec![RouteDescription::new(
            RouteSlot::new(0),
            CapabilityBundle::new(
                AccessFootprintKind::Whole,
                AccessResultKind::CompleteDocument,
                CodecDemand::try_new(&resources),
                AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            ),
        )];
        MUTATED_PROFILE.store(false, Ordering::SeqCst);
        OPEN_CALLED.store(false, Ordering::SeqCst);
        let source = ResolvedSource::new(
            SourceRef::new(SourceId::new(44), SourceKind::Input),
            "mutable-profile",
            &[],
            0,
        );
        let mut provider =
            ErasedProvider::try_new_provider(source, &resources, || Ok(MutableProfileProvider { initial, mutated }))
                .expect("provider");
        let handle = provider.bind(&requirement).expect("bind");
        MUTATED_PROFILE.store(true, Ordering::SeqCst);
        assert!(matches!(
            provider.open(&handle, &mut resources),
            Err(error) if error.kind() == CodecFailureKind::ProviderRouteMismatch
        ));
        assert!(!OPEN_CALLED.load(Ordering::SeqCst));
        MUTATED_PROFILE.store(false, Ordering::SeqCst);
    }

    #[test]
    fn source_safe_fallback_terminalizes_first_inner_error() {
        static SOURCE: [u8; 0] = [];
        let _guard = SOURCE_TEST_LOCK.lock().expect("source test lock");
        let mut resources = resources();
        let requirement = exact_source_requirement(&resources, false, DiagnosticPolicy::ErrorsOnly, false);
        let routes = whole_routes(&resources, DiagnosticPolicy::ErrorsOnly);
        let product = source_product(&resources, false, DiagnosticCoverage::NotRequested);
        SOURCE_POLLS.store(0, Ordering::SeqCst);
        let source = ResolvedSource::new(
            SourceRef::new(SourceId::new(45), SourceKind::Input),
            "fallback-error",
            &SOURCE,
            0,
        );
        let mut provider = ErasedProvider::try_new_provider(source, &resources, || {
            Ok(SourceFullProvider {
                routes,
                product: Some(product),
                behavior: SourceBehavior::ErrorThenReady,
                absence: false,
            })
        })
        .expect("provider");
        let handle = provider.bind(&requirement).expect("bind");
        let mut session = provider.open(&handle, &mut resources).expect("open");
        let mut context = CodecRunContext::new(&mut resources);
        assert!(matches!(
            session.decode(&mut context),
            Err(error) if error.kind() == CodecFailureKind::InvalidInput
        ));
        // The failure is terminal-stable: a second decode re-raises it.
        assert!(matches!(
            session.decode(&mut context),
            Err(error) if error.kind() == CodecFailureKind::InvalidInput
        ));
        assert_eq!(SOURCE_POLLS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn source_attribute_absence_executes_residual_and_seals_report() {
        static SOURCE: [u8; 0] = [];
        let _guard = SOURCE_TEST_LOCK.lock().expect("source test lock");
        let mut resources = resources();
        let requirement = exact_source_requirement(&resources, true, DiagnosticPolicy::ErrorsOnly, false);
        let routes = whole_routes(&resources, DiagnosticPolicy::ErrorsOnly);
        let product = source_product(&resources, true, DiagnosticCoverage::NotRequested);
        SOURCE_POLLS.store(0, Ordering::SeqCst);
        RESIDUAL_ATTRIBUTE_SEEN.store(true, Ordering::SeqCst);
        let source = ResolvedSource::new(
            SourceRef::new(SourceId::new(46), SourceKind::Input),
            "attribute-absence",
            &SOURCE,
            0,
        );
        let mut provider = ErasedProvider::try_new_provider(source, &resources, || {
            Ok(SourceFullProvider {
                routes,
                product: Some(product),
                behavior: SourceBehavior::Ready,
                absence: true,
            })
        })
        .expect("provider");
        let handle = provider.bind(&requirement).expect("bind");
        let mut session = provider.open(&handle, &mut resources).expect("open");
        assert!(!RESIDUAL_ATTRIBUTE_SEEN.load(Ordering::SeqCst));
        let mut context = CodecRunContext::new(&mut resources);
        let result = session.decode(&mut context).expect("decode");
        assert_eq!(
            result.report().adapter(),
            AccessAdapter::CompleteDocumentExactWithAttributeAbsence
        );
        assert!(result.report().route().is_some());
        assert_eq!(SOURCE_POLLS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn source_fallback_rejects_insufficient_diagnostics_before_interpretation() {
        static SOURCE: [u8; 0] = [];
        let _guard = SOURCE_TEST_LOCK.lock().expect("source test lock");
        let mut resources = resources();
        let requirement = exact_source_requirement(&resources, false, DiagnosticPolicy::All, false);
        let routes = whole_routes(&resources, DiagnosticPolicy::All);
        let product = source_product(&resources, false, DiagnosticCoverage::NotRequested);
        SOURCE_POLLS.store(0, Ordering::SeqCst);
        let source = ResolvedSource::new(
            SourceRef::new(SourceId::new(47), SourceKind::Input),
            "coverage-rejection",
            &SOURCE,
            0,
        );
        let mut provider = ErasedProvider::try_new_provider(source, &resources, || {
            Ok(SourceFullProvider {
                routes,
                product: Some(product),
                behavior: SourceBehavior::Ready,
                absence: false,
            })
        })
        .expect("provider");
        let handle = provider.bind(&requirement).expect("bind");
        let mut session = provider.open(&handle, &mut resources).expect("open");
        let mut context = CodecRunContext::new(&mut resources);
        for _ in 0..2 {
            assert!(matches!(
                session.decode(&mut context),
                Err(error) if matches!(error.kind(), CodecFailureKind::InternalContractViolation {
                    contract: "fallback product does not satisfy residual coverage"
                })
            ));
        }
        assert_eq!(SOURCE_POLLS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn source_fallback_validates_and_retains_inner_product() {
        static SOURCE: [u8; 0] = [];
        let _guard = SOURCE_TEST_LOCK.lock().expect("source test lock");
        let mut resources = resources();
        let requirement = exact_source_requirement(&resources, false, DiagnosticPolicy::ErrorsOnly, true);
        let routes = whole_routes(&resources, DiagnosticPolicy::ErrorsOnly);
        let product = source_product(&resources, false, DiagnosticCoverage::NotRequested);
        SOURCE_POLLS.store(0, Ordering::SeqCst);
        let source = ResolvedSource::new(
            SourceRef::new(SourceId::new(48), SourceKind::Input),
            "fallback-pending",
            &SOURCE,
            0,
        );
        let mut provider = ErasedProvider::try_new_provider(source, &resources, || {
            Ok(SourceFullProvider {
                routes,
                product: Some(product),
                behavior: SourceBehavior::ReadyAfterConsumingCredit,
                absence: false,
            })
        })
        .expect("provider");
        let handle = provider.bind(&requirement).expect("bind");
        let mut session = provider.open(&handle, &mut resources).expect("open");
        let mut context = CodecRunContext::new(&mut resources);
        context.set_cooperative_credits(4_096);
        // Straight-line decode: the fallback runs the inner decode once and the exact interpreter over the retained
        // product; the cooperative-yield protocol no longer exists to split across polls.
        let result = session.decode(&mut context).expect("decode");
        // The fallback serves the EXACT selection over the retained inner product, so the outcome is the interpreter's
        // Located observation.
        assert!(matches!(result.outcome(), AccessOutcome::Located(_)));
        assert_eq!(SOURCE_POLLS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn reused_session_rejects_a_stale_residual_across_requirements_on_one_slot() {
        fn exact_member_requirement(resources: &ResourceContext<'_>, member: &str) -> AccessRequirement {
            let mut path = ExactPath::try_new(resources);
            path.try_push_semantic_member(member, resources).expect("step");
            let footprint = AccessFootprint::try_exact(path, resources);
            AccessRequirement::try_exact(
                footprint,
                CodecDemand::try_new(resources),
                AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
                resources,
            )
            .expect("requirement")
        }

        static SOURCE: [u8; 0] = [];
        let _guard = SOURCE_TEST_LOCK.lock().expect("source test lock");
        let mut resources = resources();
        let routes = vec![RouteDescription::new(
            RouteSlot::new(0),
            CapabilityBundle::new(
                AccessFootprintKind::Exact,
                AccessResultKind::Located,
                CodecDemand::try_new(&resources),
                AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            ),
        )];

        let requirement_a = exact_member_requirement(&resources, "a");
        let requirement_b = exact_member_requirement(&resources, "b");
        let expected_a = requirement_a.footprint().fingerprint();
        let expected_b = requirement_b.footprint().fingerprint();
        assert_ne!(expected_a, expected_b, "probe members must differ");

        let source = ResolvedSource::new(
            SourceRef::new(SourceId::new(50), SourceKind::Input),
            "reuse-residual",
            &SOURCE,
            0,
        );
        RESIDUAL_FINGERPRINTS.lock().expect("clear").clear();
        let mut provider =
            ErasedProvider::try_new_provider(source, &resources, || Ok(ResidualRecordingProvider { routes }))
                .expect("provider");
        let handle_a = provider.bind(&requirement_a).expect("bind a");
        let handle_b = provider.bind(&requirement_b).expect("bind b");
        assert_eq!(handle_a.slot, handle_b.slot, "both handles bind slot 0");

        let mut reuse = ReusableAccessSession::new();
        provider
            .open_at_reusing(&handle_a, 0, &mut reuse, &mut resources)
            .expect("first open");
        provider
            .open_at_reusing(&handle_b, 0, &mut reuse, &mut resources)
            .expect("second open");

        assert_eq!(
            *RESIDUAL_FINGERPRINTS.lock().expect("residuals"),
            vec![expected_a, expected_b],
            "the second handle must not reuse the first requirement's residual"
        );
    }

    #[test]
    fn an_open_after_a_failed_reopen_constructs_a_fresh_session() {
        static SOURCE: [u8; 0] = [];
        let _guard = SOURCE_TEST_LOCK.lock().expect("source test lock");
        let mut resources = resources();
        let guarantees = AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly);
        let routes = vec![RouteDescription::new(
            RouteSlot::new(0),
            CapabilityBundle::new(
                AccessFootprintKind::Exact,
                AccessResultKind::Located,
                CodecDemand::try_new(&resources),
                guarantees,
            ),
        )];
        let mut path = ExactPath::try_new(&resources);
        path.try_push_semantic_member("a", &resources).expect("step");
        let footprint = AccessFootprint::try_exact(path, &resources);
        let requirement =
            AccessRequirement::try_exact(footprint, CodecDemand::try_new(&resources), guarantees, &resources)
                .expect("requirement");

        let source = ResolvedSource::new(
            SourceRef::new(SourceId::new(51), SourceKind::Input),
            "failing-reopen",
            &SOURCE,
            0,
        );
        FAILING_REOPEN_OPENS.store(0, Ordering::SeqCst);
        let mut provider =
            ErasedProvider::try_new_provider(source, &resources, || Ok(FailingReopenProvider { routes }))
                .expect("provider");
        let handle = provider.bind(&requirement).expect("bind");

        let mut reuse = ReusableAccessSession::new();
        provider
            .open_at_reusing(&handle, 0, &mut reuse, &mut resources)
            .expect("first open");
        assert!(
            provider
                .open_at_reusing(&handle, 0, &mut reuse, &mut resources)
                .is_err(),
            "the reopen hook fails, so the second open fails"
        );
        provider
            .open_at_reusing(&handle, 0, &mut reuse, &mut resources)
            .expect("open after a failed reopen");
        assert_eq!(
            FAILING_REOPEN_OPENS.load(Ordering::SeqCst),
            2,
            "the open after the failure must construct a session instead of \
             reopening the one the failure left behind"
        );
    }
}
