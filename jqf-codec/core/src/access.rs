//! Exact access requirements, sealed route handles, and decode outcomes.
//!
//! [`AccessRequirement`] is what the binder matches. [`ErasedAccessSession`] is the opened route. Sibling:
//! [`crate::binder`].

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::any::Any;
use jqf_data::{BuilderCoverage, DiagnosticCoverage, DocumentCapability, DocumentCapabilityFamily, DocumentCoverage};
use jqf_resource::ResourceContext;
use jqf_source::ResolvedSource;

use crate::capability::{AccessFootprintKind, AccessResultKind};
use crate::demand::DemandFingerprint;
use crate::pattern::{AccessFootprint, AccessFootprintFingerprint, ExactPath};
use crate::schedule::{SelectionOrigin, SelectionSchedule};
use crate::{
    AccessGuarantees, CapabilityBundle, CodecDemand, CodecError, CodecFailureKind, CodecRunContext, DiagnosticPolicy,
    DocumentProduct, LocatedOutcome, ValidationMode,
};
use crate::{AccessReport, DemandClause};

/// Dense provider-local route slot.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct RouteSlot(u32);

impl RouteSlot {
    /// Creates a route slot from a dense index.
    #[must_use]
    pub const fn new(index: u32) -> Self {
        Self(index)
    }
    /// Returns the dense index.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Format-neutral stable identity of the physical implementation executed by a session.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct PhysicalRouteId(u64);

impl PhysicalRouteId {
    /// Reserved identity for legacy or harness sessions without a named physical route.
    pub const UNSPECIFIED: Self = Self(0);

    /// Creates a stable nonzero physical route identity.
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }

    /// Packs `(format, kind, specialization)` into a nonzero identity: the first six ASCII bytes of `format`, then
    /// `kind`, then `specialization`. Distinct only if no two formats share a six-byte prefix. `None` only for the
    /// all-zero triple.
    #[must_use]
    pub const fn derive(format: &'static str, kind: u8, specialization: u8) -> Option<Self> {
        let mut bytes = [0_u8; 8];
        let format_bytes = format.as_bytes();
        let mut index = 0;
        while index < format_bytes.len() && index < 6 {
            bytes[index] = format_bytes[index];
            index += 1;
        }
        let format_bits = u64::from_be_bytes(bytes);
        let value = format_bits | ((kind as u64) << 8) | (specialization as u64);
        if value == 0 { None } else { Some(Self(value)) }
    }

    /// [`Self::derive`] for a route constant: the `None` arm is a codec that declared an all-zero triple, which is a
    /// bug in the declaration and not a runtime condition, so it fails the build of that constant.
    ///
    /// # Panics
    ///
    /// When the derived identity is zero — only reachable from an empty format id with a zero kind and a zero
    /// specialization.
    #[must_use]
    pub const fn derive_or_panic(format: &'static str, kind: u8, specialization: u8) -> Self {
        match Self::derive(format, kind, specialization) {
            Some(id) => id,
            None => panic!("nonzero route identity"),
        }
    }

    /// Returns the stable opaque numeric identity for receipts and benchmarks.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Receipt proving the concrete physical route returned by sealed provider dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhysicalRouteReceipt {
    route: PhysicalRouteId,
    provider_id: u64,
    slot: RouteSlot,
}

impl PhysicalRouteReceipt {
    /// Seals one receipt from its three core-owned fields.
    pub(crate) const fn seal(route: PhysicalRouteId, provider_id: u64, slot: RouteSlot) -> Self {
        Self {
            route,
            provider_id,
            slot,
        }
    }

    /// Concrete format-neutral physical implementation identity.
    #[must_use]
    pub const fn route(self) -> PhysicalRouteId {
        self.route
    }

    /// Core-sealed provider identity that returned the session.
    #[must_use]
    pub const fn provider_id(self) -> u64 {
        self.provider_id
    }

    /// Core-sealed provider-local route slot that returned the session.
    #[must_use]
    pub const fn slot(self) -> RouteSlot {
        self.slot
    }
}

/// Core adapter used between a requirement and an executable route.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccessAdapter {
    /// No core adapter was used.
    None,
    /// Exact selection was interpreted over complete authority.
    CompleteDocumentExact,
    /// Exact attributes are authoritatively absent and removed from residual demand.
    AttributeAbsence,
    /// Complete-document exact selection composed with authoritative attribute absence.
    CompleteDocumentExactWithAttributeAbsence,
    /// The whole-document DEMAND fallback: the provider could not serve the requirement's specific result authority, so
    /// it serves the lazy whole document and the consumer runs the whole program against it. The demand is a HINT the
    /// provider may ignore; binding never fails for capability reasons, and a sparse codec's query takes the same shape
    /// a rich one's does, differing only in how much the codec defers.
    CompleteDocumentDemand,
}

/// One route slot and its complete indivisible capability bundle.
#[derive(Debug)]
pub struct RouteDescription {
    slot: RouteSlot,
    bundle: CapabilityBundle,
}

impl RouteDescription {
    /// Describes one concrete provider route.
    #[must_use]
    pub const fn new(slot: RouteSlot, bundle: CapabilityBundle) -> Self {
        Self { slot, bundle }
    }
    /// Provider-local slot.
    #[must_use]
    pub const fn slot(&self) -> RouteSlot {
        self.slot
    }
    /// Complete route guarantee conjunction.
    #[must_use]
    pub const fn bundle(&self) -> &CapabilityBundle {
        &self.bundle
    }

    /// Builds a provider's whole route table from a closed row literal: every codec's `try_new` pushed the same 13-line
    /// `RouteDescription::new(slot, CapabilityBundle::new(…))` block per slot, differing only in the slot and the
    /// footprint/result pair. The row literal collapses it to one line per slot and the demand builder lives here.
    ///
    /// # Errors
    ///
    /// Returns an allocation error when retained route storage cannot be reserved.
    pub fn try_table(
        rows: &[(RouteSlot, AccessFootprintKind, AccessResultKind)],
        guarantees: AccessGuarantees,
        resources: &ResourceContext<'_>,
    ) -> Result<Vec<RouteDescription>, CodecError> {
        let mut routes = Vec::new();
        routes
            .try_reserve_exact(rows.len())
            .map_err(jqf_resource::ResourceError::from)?;
        for &(slot, footprint, result) in rows {
            let demand = minimal_semantic_demand(resources)?;
            routes.push(RouteDescription::new(
                slot,
                CapabilityBundle::new(footprint, result, demand, guarantees),
            ));
        }
        Ok(routes)
    }

    /// Builds the standard two-route single-document table — whole (slot 0) and located (1) — that the tree-shaped
    /// document codecs (CBOR, TOML, YAML) advertise verbatim. A codec with any other slot layout keeps its own
    /// [`RouteDescription::try_table`] call.
    ///
    /// # Errors
    ///
    /// Returns an allocation error when retained route storage cannot be reserved.
    pub fn try_standard_document_table(
        guarantees: AccessGuarantees,
        resources: &ResourceContext<'_>,
    ) -> Result<Vec<RouteDescription>, CodecError> {
        Self::try_table(
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
        )
    }
}

/// The minimal demand every decode route advertises: the semantic root projection plus the exact payload-transparent
/// semantic category.
fn minimal_semantic_demand(resources: &ResourceContext<'_>) -> Result<CodecDemand, CodecError> {
    let mut demand = CodecDemand::try_new(resources);
    demand.try_insert(&DemandClause::SemanticRoot)?;
    demand.try_insert(&DemandClause::ValueShape)?;
    Ok(demand)
}

/// The diagnostic coverage a sealed plan carries for one request policy.
fn coverage_for(policy: DiagnosticPolicy) -> DiagnosticCoverage {
    match policy {
        DiagnosticPolicy::Off | DiagnosticPolicy::ErrorsOnly => DiagnosticCoverage::NotRequested,
        DiagnosticPolicy::All => DiagnosticCoverage::AuthoritativeEmpty,
    }
}

/// Whether a decode should retain attached facts for a later consumer that re-emits them.
///
/// MONOTONE: [`Self::Preserve`] asks the codec to attach facts it would otherwise omit; [`Self::None`] is the default
/// and never strips facts a demand already named. `--edit` and an output dialect whose encoder re-emits comments set
/// `Preserve` at requirement construction in the SDK/CLI, not inside a codec provider. A program that reads or writes
/// facts is the same bit: it needs the facts attached, so construction sets `Preserve` rather than growing a second
/// field.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FactIntent {
    /// Do not attach facts unless the demand named them.
    #[default]
    None,
    /// Attach facts so a later encode, edit, or fact-read can observe them.
    Preserve,
}

/// Complete semantic requirement owned by engine planning.
///
/// Every field added here must also be copied by the provider's `residual_requirement` reconstruction — the fallback
/// path rebuilds the requirement, and an uncopied field silently disappears there (the residual-hints receipt test in
/// provider.rs guards the known fields).
#[derive(Debug)]
pub struct AccessRequirement {
    footprint: AccessFootprint,
    schedule: SelectionSchedule,
    result: AccessResultKind,
    demand: CodecDemand,
    guarantees: AccessGuarantees,
    /// The on-demand container-span frontier depth the whole-document route should decode at (0 = eager, 1+ = defer
    /// containers below that depth).
    ///
    /// Carried per requirement so the lazy default follows the PROGRAM (the engine's route decision), not a process
    /// global: a library caller that never touches the codec-core knob still gets the request's own decision, and
    /// concurrent requests cannot race a global. Zero by construction — every non-root route is eager; the engine's
    /// root lowering sets it.
    lazy_frontier: u32,
    /// Whether the whole-document route must run the source-canonicality probe (the per-key duplicate fingerprint and
    /// its window scan).
    ///
    /// The canonicality VERDICT is consumed only by the canonical-identity echo (`execute_source_roundtrip`), and that
    /// lane is the only caller that sets this flag; every other decode pays nothing for a verdict it cannot consume.
    /// `false` by construction — a plain decode never runs the probe, and the canonicality disqualifiers that still
    /// fire on a plain decode (leading whitespace, byte-order mark, string escapes, exponent spellings) keep the flag
    /// meaningful on the echo path: only an input that clears them can answer the probe's verdict at all.
    canonicality_probe: bool,
    /// The kept-subtree prune HINT. The engine's whole-document root lowering may set it; the Exact-to-Whole binder
    /// residual synthesizes a path spine when the engine did not. MONOTONE: a codec may ignore it, and delivering more
    /// than it names is always sound — so it enters none of the footprint/schedule/result validation above.
    prune: Option<crate::prune::PruneTree>,
    /// The COUNT demand hint: a recognized count row's full demand, carried when the requesting program is a
    /// count-class row. MONOTONE, like [`Self::prune`]: a codec may ignore it (the document-core consumer then walks
    /// the demand against the document the decode produced) or shape its decode from it (the XML provider's measure
    /// parse). `None` by construction.
    count: Option<jqf_data::CountDemand>,
    /// The TYPE demand hint: a bare-root `type` call, whose answer is the root VALUE KIND — a kind-only read that
    /// needs no materialized tree. MONOTONE like [`Self::count`]: a codec may ignore it, and a codec that CAN profit
    /// shapes its decode from it (the XML provider's measure parse serves the type answer without building the tree —
    /// sound because a kind-only answer observes nothing beyond the root value's shape). `false` by construction.
    type_demand: bool,
    /// The ELEMENT-ITERATION demand hint: a recognized fan-out/fold row's full demand, carried when the requesting
    /// program is an element-iteration row. MONOTONE, like [`Self::count`]: a codec may ignore it (the document-core
    /// consumer then walks the demand against the document the decode produced, iterating deferred spans through the
    /// format leaf) or shape its decode from it (the XML provider's measure parse serves a root-bound element demand).
    /// `None` by construction.
    ///
    /// All three of these hints may shape how a decode runs, but none enters a recycle key: what the decode delivers is
    /// identical either way, so a recycled session re-shapes per requirement.
    element: Option<jqf_data::ElementDemand>,
    /// The authored-span demand hint: whether a consumer of this decode may read the out-of-band authored-span table
    /// (`Document::node_source_span`'s fallback) — the edit, splice, and lossless-encode lanes. Unlike every other
    /// hint its safe default is TRUE: recording spans no consumer reads costs only the table, while dropping spans a
    /// consumer needs floors an edit — so a residual reconstruction that lost the field would fail SAFE. Only a drive
    /// that can prove its program never edits, splices, or losslessly re-encodes the document clears it; a codec may
    /// ignore the hint entirely.
    authored_spans: bool,
    /// Whether the decode should retain attached facts for a later encode, edit, or fact-read. MONOTONE like
    /// [`Self::prune`]: a codec may ignore it, and delivering facts when this is [`FactIntent::None`] is always sound.
    /// `None` by construction.
    fact_intent: FactIntent,
}

impl AccessRequirement {
    /// Assembles a requirement from its closed structural fields; every carried-hint default lives here once, so the
    /// two named constructors cannot drift.
    fn assemble(
        footprint: AccessFootprint,
        schedule: SelectionSchedule,
        result: AccessResultKind,
        demand: CodecDemand,
        guarantees: AccessGuarantees,
    ) -> Self {
        Self {
            footprint,
            schedule,
            result,
            demand,
            guarantees,
            lazy_frontier: 0,
            canonicality_probe: false,
            prune: None,
            count: None,
            type_demand: false,
            element: None,
            authored_spans: true,
            fact_intent: FactIntent::None,
        }
    }

    /// Constructs the WHOLE-document requirement: whole footprint, empty-complete schedule,
    /// [`AccessResultKind::CompleteDocument`]. The only legal whole shape, so every illegal pairing is unexpressible at
    /// compile time.
    ///
    /// # Errors
    ///
    /// Returns an internal contract violation when the empty-complete schedule cannot be assembled under the request
    /// account.
    pub fn try_whole(
        demand: CodecDemand,
        guarantees: AccessGuarantees,
        resources: &ResourceContext<'_>,
    ) -> Result<Self, CodecError> {
        let footprint = AccessFootprint::try_whole(resources);
        let schedule = SelectionSchedule::try_empty_complete(&footprint, resources).map_err(|_| {
            CodecError::new(CodecFailureKind::InternalContractViolation {
                contract: "invalid footprint/schedule pairing",
            })
        })?;
        Ok(Self::assemble(
            footprint,
            schedule,
            AccessResultKind::CompleteDocument,
            demand,
            guarantees,
        ))
    }

    /// Constructs the EXACT-path requirement from an exact footprint: singleton selection schedule at selection origin
    /// 0, [`AccessResultKind::Located`]. A whole footprint is a contract error; the whole shape has its own constructor
    /// ([`Self::try_whole`]). The authored-origin variant is [`Self::try_exact_with_origin`].
    ///
    /// # Errors
    ///
    /// Returns an internal contract violation for a whole footprint or when the singleton schedule cannot be assembled
    /// under the request account.
    pub fn try_exact(
        footprint: AccessFootprint,
        demand: CodecDemand,
        guarantees: AccessGuarantees,
        resources: &ResourceContext<'_>,
    ) -> Result<Self, CodecError> {
        Self::try_exact_with_origin(footprint, SelectionOrigin::new(0), demand, guarantees, resources)
    }

    /// Constructs the EXACT-path requirement under an AUTHORED selection origin. Every production requirement schedules
    /// one selection at origin 0 today, but the residual reconstruction must carry whatever origin the requirement was
    /// authored with — it keys session reuse on that origin ([`crate::access::residual_key`]), so a rebuild that
    /// silently renumbered the schedule would hand the provider a schedule the key does not describe.
    ///
    /// # Errors
    ///
    /// Returns an internal contract violation for a whole footprint or when the singleton schedule cannot be assembled
    /// under the request account.
    pub(crate) fn try_exact_with_origin(
        footprint: AccessFootprint,
        origin: SelectionOrigin,
        demand: CodecDemand,
        guarantees: AccessGuarantees,
        resources: &ResourceContext<'_>,
    ) -> Result<Self, CodecError> {
        if footprint.is_whole() {
            return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                contract: "the exact requirement constructor requires an exact footprint",
            }));
        }
        let schedule = SelectionSchedule::try_singleton_exact(&footprint, origin, resources).map_err(|_| {
            CodecError::new(CodecFailureKind::InternalContractViolation {
                contract: "invalid footprint/schedule pairing",
            })
        })?;
        Ok(Self::assemble(
            footprint,
            schedule,
            AccessResultKind::Located,
            demand,
            guarantees,
        ))
    }

    /// The container-span frontier depth this requirement's whole-document route should decode at — 0 (eager) by
    /// construction for every non-root route, and set by the engine's root lowering from the program's consumption
    /// class, so the depth follows what the program actually reads rather than a process-wide default.
    #[must_use]
    pub fn lazy_frontier(&self) -> u32 {
        self.lazy_frontier
    }

    /// Builder: names the lazy-frontier depth this requirement carries. The whole-document route (slot 0) reads it at
    /// open; every other route is eager regardless.
    #[must_use]
    pub fn with_lazy_frontier(mut self, depth: u32) -> Self {
        self.lazy_frontier = depth;
        self
    }

    /// Whether the whole-document route must run the source-canonicality probe. Set only by the canonical-identity echo
    /// lane; see the field doc for why the probe is opt-in.
    #[must_use]
    pub const fn canonicality_probe(&self) -> bool {
        self.canonicality_probe
    }

    /// Builder: arms the source-canonicality probe on the whole-document route when `on` is true. The
    /// canonical-identity echo lane is the only caller — the probe's per-key fingerprints exist solely to decide that
    /// lane's verdict.
    #[must_use]
    pub fn with_canonicality_probe(mut self, on: bool) -> Self {
        self.canonicality_probe = on;
        self
    }

    /// Whether any consumer of this decode may read the out-of-band authored-span table. TRUE by construction — see
    /// the field doc for why this hint's safe default is inverted relative to every other hint.
    #[must_use]
    pub const fn authored_spans(&self) -> bool {
        self.authored_spans
    }

    /// Builder: declares whether any consumer reads authored spans. Pass `false` only from a drive that can prove its
    /// program never edits, splices, or losslessly re-encodes the decoded document.
    #[must_use]
    pub fn with_authored_spans(mut self, on: bool) -> Self {
        self.authored_spans = on;
        self
    }

    /// Whether this decode should retain attached facts for a later encode, edit, or fact-read. [`FactIntent::None`] by
    /// construction.
    #[must_use]
    pub const fn fact_intent(&self) -> FactIntent {
        self.fact_intent
    }

    /// Builder: names whether the decode should retain attached facts. SDK/CLI construction sets
    /// [`FactIntent::Preserve`] for `--edit`, for an output dialect whose encoder re-emits comments, and for a program
    /// that reads or writes facts.
    #[must_use]
    pub fn with_fact_intent(mut self, intent: FactIntent) -> Self {
        self.fact_intent = intent;
        self
    }

    /// Builder: attaches the kept-subtree prune hint. The engine's whole-document root lowering and the Exact-to-Whole
    /// binder residual call this; a codec that cannot profit ignores the hint (delivering more is always sound).
    #[must_use]
    pub fn with_prune(mut self, tree: crate::prune::PruneTree) -> Self {
        self.prune = Some(tree);
        self
    }

    /// The COUNT demand hint this requirement carries, when the requesting program is a count-class row. MONOTONE: a
    /// codec may ignore it, and the hint adds no capability — the document-core count consumer walks the demand
    /// against the document the decode produced, and a codec that serves the count differently (the XML provider's
    /// measure parse) reads it to shape its decode.
    #[must_use]
    pub const fn count(&self) -> Option<&jqf_data::CountDemand> {
        self.count.as_ref()
    }

    /// Builder: attaches a COUNT demand hint to this requirement. The hint travels untracked, like the prune hint —
    /// it is an engine-plan carrier built and retained on the engine side, never a codec-owned allocation.
    #[must_use]
    pub fn with_count(mut self, demand: jqf_data::CountDemand) -> Self {
        self.count = Some(demand);
        self
    }

    /// Whether the requesting program is a bare-root `type` call, whose answer is the root kind — a kind-only read.
    #[must_use]
    pub const fn type_demand(&self) -> bool {
        self.type_demand
    }

    /// Builder: marks the requirement as a bare-root `type` call. Only the engine's whole-document root lowering calls
    /// this; a codec that cannot profit ignores it (the ordinary decode still answers `type` from the root it builds).
    #[must_use]
    pub fn with_type_demand(mut self) -> Self {
        self.type_demand = true;
        self
    }

    /// The ELEMENT-ITERATION demand hint this requirement carries, when the requesting program is an element-iteration
    /// row. MONOTONE: a codec may ignore it, and the hint adds no capability — the document-core consumer walks the
    /// demand against the document the decode produced, and a codec that serves the iteration differently (the XML
    /// provider's measure parse) reads it to shape its decode.
    #[must_use]
    pub const fn element(&self) -> Option<&jqf_data::ElementDemand> {
        self.element.as_ref()
    }

    /// Builder: attaches an ELEMENT-ITERATION demand hint to this requirement. The hint travels untracked, like the
    /// count and prune hints — an engine-plan carrier built and retained on the engine side.
    #[must_use]
    pub fn with_element(mut self, demand: jqf_data::ElementDemand) -> Self {
        self.element = Some(demand);
        self
    }

    /// The kept-subtree prune hint, when the requesting program's reads bounded below the whole document.
    #[must_use]
    pub fn prune(&self) -> Option<&crate::prune::PruneTree> {
        self.prune.as_ref()
    }

    /// Exact information requirement.
    #[must_use]
    pub const fn demand(&self) -> &CodecDemand {
        &self.demand
    }
    /// Canonical structural footprint.
    #[must_use]
    pub const fn footprint(&self) -> &AccessFootprint {
        &self.footprint
    }
    /// Exact engine-authored emission schedule.
    #[must_use]
    pub const fn schedule(&self) -> &SelectionSchedule {
        &self.schedule
    }
    /// Required result authority.
    #[must_use]
    pub const fn result(&self) -> AccessResultKind {
        self.result
    }
    /// Required validation guarantee.
    #[must_use]
    pub const fn validation(&self) -> ValidationMode {
        self.guarantees.validation()
    }
    /// Required successful diagnostic policy.
    #[must_use]
    pub const fn diagnostics(&self) -> DiagnosticPolicy {
        self.guarantees.diagnostics()
    }

    /// Route-open guard for a WHOLE-document slot: the requirement must be whole-footprint, empty-complete-schedule,
    /// and name exactly `result`. Every codec's slot-0 dispatch restated this triple check; a mismatch is the binder
    /// handing the wrong requirement to the route.
    ///
    /// # Errors
    ///
    /// Returns `ProviderRouteMismatch` when any leg of the contract fails.
    pub fn expect_whole(&self, result: AccessResultKind) -> Result<(), CodecError> {
        if !self.footprint.is_whole() || !self.schedule.is_empty_complete() || self.result != result {
            return Err(CodecError::new(CodecFailureKind::ProviderRouteMismatch));
        }
        Ok(())
    }

    /// Route-open guard for an EXACT-path slot: the requirement must be exact-footprint with a live schedule, name
    /// exactly `result`, and carry both the exact path and the singleton selection origin — returned together, since
    /// every exact route navigates the one and stamps the other.
    ///
    /// # Errors
    ///
    /// Returns `ProviderRouteMismatch` when any leg of the contract fails.
    pub fn expect_exact(&self, result: AccessResultKind) -> Result<(&ExactPath, SelectionOrigin), CodecError> {
        if self.footprint.is_whole() || self.schedule.is_empty_complete() || self.result != result {
            return Err(CodecError::new(CodecFailureKind::ProviderRouteMismatch));
        }
        let path = self
            .footprint
            .exact_path()
            .ok_or_else(|| CodecError::new(CodecFailureKind::ProviderRouteMismatch))?;
        let origin = self
            .schedule
            .singleton_origin()
            .ok_or_else(|| CodecError::new(CodecFailureKind::ProviderRouteMismatch))?;
        Ok((path, origin))
    }

    pub(crate) fn first_mismatch(&self, bundle: &CapabilityBundle) -> Option<CapabilityMismatch> {
        let footprint = if self.footprint.is_whole() {
            AccessFootprintKind::Whole
        } else {
            AccessFootprintKind::Exact
        };
        if bundle.footprint() != footprint {
            return Some(CapabilityMismatch::Footprint);
        }
        if bundle.result() != self.result {
            return Some(CapabilityMismatch::Result);
        }
        if !bundle.demand().implies(&self.demand) {
            return Some(CapabilityMismatch::Demand);
        }
        if bundle.validation() != self.validation() {
            return Some(CapabilityMismatch::Validation);
        }
        if bundle.diagnostics() != self.diagnostics() {
            return Some(CapabilityMismatch::Diagnostics);
        }
        None
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RequiredCoverage {
    capabilities: u16,
    /// The attribute family must be AUTHORITATIVELY EMPTY. It is the only empty-family requirement any demand produces:
    /// an attribute clause served by an absence adapter asks the document to PROVE it holds no attributes, and no other
    /// clause asks a family to be empty at all.
    empty_attributes: bool,
    unsupported_positive_attribute: bool,
    diagnostics: DiagnosticCoverage,
}
impl RequiredCoverage {
    fn add(&mut self, cap: DocumentCapability) {
        self.capabilities |= cap_bit(cap);
    }
    /// Maps the required capabilities to the side-data coverage a builder must physically construct. Mandatory
    /// semantics stay on; whole-source retention is decided by the bound source, not the demand, so it is not gated
    /// here.
    pub(crate) fn to_builder_coverage(self) -> BuilderCoverage {
        BuilderCoverage::complete()
            .with_topology(self.capabilities & cap_bit(DocumentCapability::Topology) != 0)
            .with_attached_facts(self.capabilities & cap_bit(DocumentCapability::AttachedFacts) != 0)
    }

    pub(crate) fn is_satisfied_by(self, coverage: DocumentCoverage) -> bool {
        const ALL: [DocumentCapability; 6] = [
            DocumentCapability::SemanticRoot,
            DocumentCapability::SemanticNodes,
            DocumentCapability::IntrinsicTags,
            DocumentCapability::Topology,
            DocumentCapability::AttachedFacts,
            DocumentCapability::WholeSource,
        ];
        !self.unsupported_positive_attribute
            && ALL
                .into_iter()
                .all(|cap| self.capabilities & cap_bit(cap) == 0 || coverage.contains(cap))
            && (!self.empty_attributes
                || coverage
                    .authoritative_empty_families()
                    .contains(DocumentCapabilityFamily::Attributes))
            && coverage.diagnostic_coverage() == self.diagnostics
    }
}
const fn cap_bit(cap: DocumentCapability) -> u16 {
    match cap {
        DocumentCapability::SemanticRoot => 1,
        DocumentCapability::SemanticNodes => 2,
        DocumentCapability::IntrinsicTags => 4,
        DocumentCapability::Topology => 8,
        DocumentCapability::AttachedFacts => 16,
        DocumentCapability::WholeSource => 32,
    }
}
fn required_coverage_for_demand(
    demand: &CodecDemand,
    diagnostics: DiagnosticPolicy,
    adapter: AccessAdapter,
) -> RequiredCoverage {
    let mut result = RequiredCoverage {
        capabilities: 0,
        empty_attributes: false,
        unsupported_positive_attribute: false,
        diagnostics: coverage_for(diagnostics),
    };
    result.add(DocumentCapability::SemanticRoot);
    result.add(DocumentCapability::SemanticNodes);
    for clause in demand.clauses() {
        match clause {
            DemandClause::SemanticRoot | DemandClause::ValueShape | DemandClause::IntrinsicTag => {
                result.add(DocumentCapability::IntrinsicTags);
            }
            DemandClause::AttachedFact { .. } => result.add(DocumentCapability::AttachedFacts),
            DemandClause::Topology(_) => result.add(DocumentCapability::Topology),
            DemandClause::Source(_) => result.add(DocumentCapability::WholeSource),
            DemandClause::Attribute(_) => {
                if matches!(
                    adapter,
                    AccessAdapter::AttributeAbsence | AccessAdapter::CompleteDocumentExactWithAttributeAbsence
                ) {
                    result.empty_attributes = true;
                } else {
                    result.unsupported_positive_attribute = true;
                }
            }
        }
    }
    result
}

pub(crate) fn required_coverage(requirement: &AccessRequirement, adapter: AccessAdapter) -> RequiredCoverage {
    required_coverage_for_demand(requirement.demand(), requirement.diagnostics(), adapter)
}

/// Side-data coverage a document builder must physically construct to satisfy one bound access requirement.
///
/// A requirement whose demand names no topology, attached-fact, or provenance clause maps to
/// [`BuilderCoverage::minimal_semantic`]; each such clause turns its family back on. Providers pass the result to the
/// builder so decode routes pay only for the coverage they were asked for.
#[must_use]
pub fn required_builder_coverage(requirement: &AccessRequirement) -> BuilderCoverage {
    required_builder_coverage_with(requirement, CoveragePolicy::PerClause)
}

/// The builder-coverage POLICY a provider serves with.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CoveragePolicy {
    /// Per-clause minimal coverage: the decode builds only the capabilities the requirement's demand names (the
    /// strict-JSON scoped routes).
    PerClause,
    /// Whole-route decode: the provider's route always decodes the complete document, so any demand beyond the minimal
    /// semantic pair requires complete coverage, and the minimal pair takes the minimal builder coverage (the cbor
    /// provider's policy).
    WholeRouteRichComplete,
}

/// Computes the builder coverage a provider must record for one requirement under one policy. One home for the policy
/// table so codecs do not grow private clause scans that drift apart.
#[must_use]
pub fn required_builder_coverage_with(requirement: &AccessRequirement, policy: CoveragePolicy) -> BuilderCoverage {
    match policy {
        CoveragePolicy::PerClause => required_coverage(requirement, AccessAdapter::None).to_builder_coverage(),
        CoveragePolicy::WholeRouteRichComplete => {
            let rich = requirement.demand().clauses().iter().any(|clause| {
                matches!(
                    clause,
                    DemandClause::AttachedFact { .. } | DemandClause::Topology(_) | DemandClause::Source(_)
                )
            });
            if rich {
                BuilderCoverage::complete()
            } else {
                BuilderCoverage::minimal_semantic()
            }
        }
    }
}

/// Deterministic first incompatible bundle axis.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapabilityMismatch {
    /// Structural footprint family.
    Footprint,
    /// Result authority family.
    Result,
    /// Exact information set.
    Demand,
    /// Validation guarantee.
    Validation,
    /// Diagnostic guarantee.
    Diagnostics,
}

/// Exact profile-bind failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccessBindError {
    /// The provider advertised no routes.
    NoRoutes,
    /// A provider-only route capability marker entered an engine requirement.
    InvalidRequirement,
    /// No route matched; this is the first mismatch of the first route.
    HardMismatch {
        /// First inspected provider slot.
        slot: RouteSlot,
        /// First incompatible axis in fixed comparison order.
        mismatch: CapabilityMismatch,
    },
}

impl core::fmt::Display for CapabilityMismatch {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str(match self {
            Self::Footprint => "structural footprint",
            Self::Result => "result authority",
            Self::Demand => "information demand",
            Self::Validation => "validation guarantee",
            Self::Diagnostics => "diagnostic guarantee",
        })
    }
}

impl core::fmt::Display for AccessBindError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoRoutes => formatter.write_str("the codec advertises no access routes"),
            Self::InvalidRequirement => {
                formatter.write_str("the requirement carries a provider-only capability marker")
            }
            Self::HardMismatch { slot, mismatch } => write!(
                formatter,
                "no codec route satisfies the requirement; the first inspected route (slot {}) diverges on the {mismatch}",
                slot.get()
            ),
        }
    }
}

/// Identity of the residual a recycled session was built from.
///
/// Computed once at bind (the requirement and adapter cannot change for the handle's lifetime) and reused on every
/// adjacent-value open. Every field that shapes what a decode delivers is keyed: the footprint and demand fingerprints,
/// the result kind, validation mode, diagnostic policy, the schedule's completeness and singleton origin, the
/// lazy-frontier depth, the canonicality-probe flag, the fact intent, the authored-span flag, the prune fingerprint,
/// and the adapter. Prune is keyed because a path-derived or engine prune omits siblings — a pruned Whole session
/// must not be reused for an unpruned Whole open — and `authored_spans` because dropping it would floor an
/// edit/splice/lossless-encode lane that the span-recording session would have served. By design NOT keyed: the count,
/// type-demand, and element hints do not change what a decode delivers (a reopen re-shapes its decode per requirement),
/// so reuse is sound without them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ResidualKey {
    footprint: AccessFootprintFingerprint,
    demand: DemandFingerprint,
    result: AccessResultKind,
    validation: ValidationMode,
    diagnostics: DiagnosticPolicy,
    schedule_complete: bool,
    schedule_origin: Option<SelectionOrigin>,
    lazy_frontier: u32,
    canonicality_probe: bool,
    fact_intent: FactIntent,
    authored_spans: bool,
    prune: Option<u64>,
    adapter: AccessAdapter,
}

pub(crate) fn residual_key(requirement: &AccessRequirement, adapter: AccessAdapter) -> ResidualKey {
    ResidualKey {
        footprint: requirement.footprint().fingerprint(),
        demand: requirement.demand().fingerprint(),
        result: requirement.result(),
        validation: requirement.validation(),
        diagnostics: requirement.diagnostics(),
        schedule_complete: requirement.schedule().is_empty_complete(),
        schedule_origin: requirement.schedule().singleton_origin(),
        lazy_frontier: requirement.lazy_frontier(),
        canonicality_probe: requirement.canonicality_probe(),
        fact_intent: requirement.fact_intent(),
        authored_spans: requirement.authored_spans(),
        prune: residual_prune_fingerprint(requirement, adapter),
        adapter,
    }
}

fn residual_prune_fingerprint(requirement: &AccessRequirement, adapter: AccessAdapter) -> Option<u64> {
    if let Some(tree) = requirement.prune() {
        return Some(tree.fingerprint());
    }
    if matches!(
        adapter,
        AccessAdapter::CompleteDocumentExact | AccessAdapter::CompleteDocumentExactWithAttributeAbsence
    ) {
        requirement
            .footprint()
            .exact_path()
            .and_then(crate::prune::PruneTree::spine_fingerprint)
    } else {
        None
    }
}

/// Core-sealed provider/route handle.
#[derive(Debug)]
pub struct AccessHandle<'requirement> {
    pub(crate) provider_id: u64,
    pub(crate) slot: RouteSlot,
    pub(crate) requirement: &'requirement AccessRequirement,
    pub(crate) adapter: AccessAdapter,
    pub(crate) residual_key: ResidualKey,
}
impl AccessHandle<'_> {
    /// Provider-local route slot selected by the binding.
    #[must_use]
    pub const fn slot(&self) -> RouteSlot {
        self.slot
    }

    /// Whether this binding is the whole-document DEMAND fallback — the provider could not serve the requirement's
    /// specific result authority and served the lazy whole document instead. A caller whose drive polls a specific
    /// result kind must DECLINE on such a binding (the session publishes the document, not the kind the drive would
    /// poll), letting the ordinary whole-program drive take the request.
    #[must_use]
    pub const fn demand_fallback(&self) -> bool {
        matches!(self.adapter, AccessAdapter::CompleteDocumentDemand)
    }
}

/// Borrowed external authority visible to one concrete access poll.
pub enum AccessInput<'authority, 'source> {
    /// Retained source bytes.
    Source(ResolvedSource<'source>),
    /// Existing authoritative document.
    Document(&'authority DocumentProduct<'source>),
}

/// Exactly one owned access result.
#[derive(Debug)]
pub enum AccessOutcome<'source> {
    /// Complete authoritative document.
    FullDocument(DocumentProduct<'source>),
    /// Located node in the same authoritative document.
    Located(LocatedOutcome<'source>),
}

/// One successful product plus its allocation-free report.
#[derive(Debug)]
pub struct AccessResult<'source> {
    outcome: AccessOutcome<'source>,
    report: AccessReport,
}
impl<'source> AccessResult<'source> {
    pub(crate) const fn new(outcome: AccessOutcome<'source>, report: AccessReport) -> Self {
        Self { outcome, report }
    }
    /// Creates a concrete-route result whose core-owned report fields are sealed at publication.
    #[must_use]
    pub const fn from_outcome(outcome: AccessOutcome<'source>) -> Self {
        Self {
            outcome,
            report: AccessReport::new(DiagnosticCoverage::NotRequested, AccessAdapter::None),
        }
    }
    /// Creates a concrete-route result reporting the exact source bytes the complete value consumed, for routes whose
    /// caller opted into partial-input consumption (e.g. decoding one of several adjacent JSON texts from the same
    /// buffer). Format-neutral: `consumed_offset` is a plain byte offset, not a JSON-specific position.
    #[must_use]
    pub const fn from_outcome_with_consumed_offset(outcome: AccessOutcome<'source>, consumed_offset: u64) -> Self {
        Self {
            outcome,
            report: AccessReport::new(DiagnosticCoverage::NotRequested, AccessAdapter::None)
                .with_consumed_offset(consumed_offset),
        }
    }
    /// Marks the consumed value as one more input could EXTEND, for a route whose grammar ends a token at the last byte
    /// it was given rather than at a delimiter (a bare JSON number). See [`AccessReport::open_ended`].
    #[must_use]
    pub const fn open_ended(mut self) -> Self {
        self.report = self.report.with_open_ended();
        self
    }
    /// Borrows the exactly-once outcome.
    #[must_use]
    pub const fn outcome(&self) -> &AccessOutcome<'source> {
        &self.outcome
    }
    /// Returns the fixed report.
    #[must_use]
    pub const fn report(&self) -> AccessReport {
        self.report
    }
    /// Consumes the result into its authority-bearing outcome and fixed report.
    #[must_use]
    pub fn into_parts(self) -> (AccessOutcome<'source>, AccessReport) {
        (self.outcome, self.report)
    }
}

/// Whether a single-outcome plan expecting `expected` may publish through the [`AccessOutcome`] SHAPE `published`
/// names.
///
/// The two are the same name for every surviving result kind; this is the only place the mapping lives, so a new result
/// kind has to answer it here rather than by widening an equality.
///
/// The record-stream kind never reaches this check at all: it has its own poll entry point, gated on its own
/// `expected_result`.
const fn publishes_outcome_shape(expected: AccessResultKind, published: AccessResultKind) -> bool {
    match expected {
        AccessResultKind::CompleteDocument => {
            matches!(published, AccessResultKind::CompleteDocument)
        }
        AccessResultKind::Located => matches!(published, AccessResultKind::Located),
        AccessResultKind::RecordStream => false,
    }
}

/// Downstream codec route state stored in a core-owned erased carrier.
///
/// The straight-line contract: `decode` runs the whole decode to completion in one call, replenishing its own
/// cooperative work budget at its loop heads ([`CodecRunContext::replenish_work`]) instead of yielding a pending poll.
/// The record framer is the one resumable poll in the tree (its own [`crate::RecordPoll`]); this trait is never the
/// framer.
pub trait AccessSession: Any {
    /// Decodes the whole input to exactly one outcome in a straight-line call.
    fn decode<'source>(
        &mut self,
        input: AccessInput<'_, 'source>,
        context: &mut CodecRunContext<'_, '_>,
    ) -> Result<AccessResult<'source>, CodecError>;

    /// Downcasts to the concrete session state for the adjacent-value reuse hook. The concrete `&mut Self` coercion
    /// records the concrete type id — a trait-object upcast to `dyn Any` would record the erased trait's id instead,
    /// which is what the hand-rolled vtables' runtime downcasts needed an error path to detect.
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

/// Sized core-owned pollable erased access session.
pub struct ErasedAccessSession<'source> {
    authority: ResolvedSource<'source>,
    state: Box<dyn AccessSession>,
    terminal: Option<CodecError>,
    complete: bool,
    physical_route: PhysicalRouteId,
    receipt: Option<PhysicalRouteReceipt>,
    required_coverage: Option<RequiredCoverage>,
    expected_result: Option<AccessResultKind>,
    expected_adapter: Option<AccessAdapter>,
    expected_diagnostics: Option<DiagnosticCoverage>,
    fallback: Option<CompleteExactFallback<'source>>,
}

/// Mutable access to a recycled session's concrete provider-owned state.
///
/// Handed to [`crate::InputProvider::try_reopen_route`] so a provider can reinitialize its own session type in place
/// — reusing the retained workspaces the previous adjacent value left behind — without codec-core naming that
/// concrete type or exposing its erased carrier.
pub struct RecycledSessionState<'state> {
    state: &'state mut dyn Any,
}

impl<'state> RecycledSessionState<'state> {
    pub(crate) const fn new(state: &'state mut dyn Any) -> Self {
        Self { state }
    }

    /// Mutably borrows the state only when its concrete type is `T`.
    #[must_use]
    pub fn downcast_mut<T: Any>(&mut self) -> Option<&mut T> {
        self.state.downcast_mut::<T>()
    }
}

/// Core-composed floor adapter that serves an exact selection by materializing the whole document through the inner
/// route, then navigating the exact path.
struct CompleteExactFallback<'source> {
    interpreter: crate::fallback::ExactFallbackState,
    inner_required_coverage: RequiredCoverage,
    product: Option<DocumentProduct<'source>>,
    /// The `consumed_offset` reported by the inner whole-document decode, and whether that decode called the value
    /// open-ended. The exact interpreter runs over the retained product and cannot observe the source span the inner
    /// decode consumed, so both are captured here and re-applied to the interpreter's published report; otherwise a
    /// fallback exact access over one of several adjacent texts would drop the offset that tells the caller where the
    /// next value begins, and the warning that more input could extend it.
    inner_consumed_offset: Option<u64>,
    inner_open_ended: bool,
}

impl core::fmt::Debug for ErasedAccessSession<'_> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("ErasedAccessSession")
            .field("terminal", &self.terminal)
            .field("complete", &self.complete)
            .finish_non_exhaustive()
    }
}

impl<'source> ErasedAccessSession<'source> {
    /// Assembles the erased session around a constructed state and the constructors' identical field literal.
    fn assemble(
        authority: ResolvedSource<'source>,
        state: Box<dyn AccessSession>,
        physical_route: PhysicalRouteId,
    ) -> Self {
        Self {
            authority,
            state,
            terminal: None,
            complete: false,
            physical_route,
            receipt: None,
            required_coverage: None,
            expected_result: None,
            expected_adapter: None,
            expected_diagnostics: None,
            fallback: None,
        }
    }

    /// Constructs a source-backed session state.
    pub fn try_new_source<T, F>(source: ResolvedSource<'source>, constructor: F) -> Result<Self, CodecError>
    where
        T: AccessSession,
        F: FnOnce() -> Result<T, CodecError>,
    {
        Self::try_new(source, PhysicalRouteId::UNSPECIFIED, constructor)
    }
    /// Constructs a source-backed session with a stable physical implementation identity.
    pub fn try_new_source_with_route<T, F>(
        source: ResolvedSource<'source>,
        physical_route: PhysicalRouteId,
        constructor: F,
    ) -> Result<Self, CodecError>
    where
        T: AccessSession,
        F: FnOnce() -> Result<T, CodecError>,
    {
        Self::try_new(source, physical_route, constructor)
    }
    fn try_new<T, F>(
        authority: ResolvedSource<'source>,
        physical_route: PhysicalRouteId,
        constructor: F,
    ) -> Result<Self, CodecError>
    where
        T: AccessSession,
        F: FnOnce() -> Result<T, CodecError>,
    {
        let state: Box<dyn AccessSession> = crate::erased::fallible_box(constructor()?)?;
        Ok(Self::assemble(authority, state, physical_route))
    }

    /// Borrows the concrete session state for the adjacent-value reuse hook.
    pub(crate) fn recycled_state(&mut self) -> RecycledSessionState<'_> {
        RecycledSessionState::new(self.state.as_any_mut())
    }

    /// Returns this session to the pre-seal state so the provider-owned dispatch can seal it again for one more
    /// adjacent value.
    ///
    /// The concrete state behind [`Self::recycled_state`] is reset separately by its own provider; this clears only the
    /// core-owned lifecycle so a value that terminalized cannot poison the next one (every terminal, completion, seal,
    /// and floor-adapter carrier is dropped here).
    pub(crate) fn reset_for_reuse(&mut self, source: ResolvedSource<'source>) {
        self.authority = source;
        self.terminal = None;
        self.complete = false;
        self.receipt = None;
        self.required_coverage = None;
        self.expected_result = None;
        self.expected_adapter = None;
        self.expected_diagnostics = None;
        self.fallback = None;
    }

    pub(crate) fn seal_route_receipt(&mut self, provider_id: u64, slot: RouteSlot) -> Result<(), CodecError> {
        if self.receipt.is_some() {
            return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                contract: "physical route receipt sealed twice",
            }));
        }
        self.receipt = Some(PhysicalRouteReceipt {
            route: self.physical_route,
            provider_id,
            slot,
        });
        Ok(())
    }

    pub(crate) fn seal_required_coverage(&mut self, coverage: RequiredCoverage) -> Result<(), CodecError> {
        if self.required_coverage.replace(coverage).is_some() {
            return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                contract: "required coverage sealed twice",
            }));
        }
        Ok(())
    }

    pub(crate) fn seal_plan(
        &mut self,
        requirement: &AccessRequirement,
        adapter: AccessAdapter,
        wrap_floor: bool,
        resources: &ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        if self.expected_result.is_some() {
            return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                contract: "access plan sealed twice",
            }));
        }
        // The whole-document DEMAND fallback publishes the document itself (the consumer runs the whole program against
        // it), so the plan the session seals is the DOCUMENT's shape, not the requirement's unserved result authority
        // — the publication shape law would otherwise reject the only outcome the route can produce.
        self.expected_result = Some(if adapter == AccessAdapter::CompleteDocumentDemand {
            AccessResultKind::CompleteDocument
        } else {
            requirement.result()
        });
        self.expected_adapter = Some(adapter);
        self.expected_diagnostics = Some(coverage_for(requirement.diagnostics()));
        if wrap_floor {
            let diagnostics = self.expected_diagnostics.ok_or_else(|| {
                CodecError::new(CodecFailureKind::InternalContractViolation {
                    contract: "floor adapter without sealed diagnostics",
                })
            })?;
            let inner_required_coverage = self.required_coverage.ok_or_else(|| {
                CodecError::new(CodecFailureKind::InternalContractViolation {
                    contract: "floor adapter without sealed coverage",
                })
            })?;
            let path = requirement
                .footprint()
                .exact_path()
                .ok_or_else(|| {
                    CodecError::new(CodecFailureKind::InternalContractViolation {
                        contract: "floor adapter without exact footprint",
                    })
                })?
                .try_clone_in(resources)?;
            self.fallback = Some(CompleteExactFallback {
                interpreter: crate::fallback::ExactFallbackState::new(
                    AccessResultKind::Located,
                    diagnostics,
                    adapter,
                    requirement.schedule().singleton_origin(),
                    Some(path),
                ),
                inner_required_coverage,
                product: None,
                inner_consumed_offset: None,
                inner_open_ended: false,
            });
        }
        Ok(())
    }

    /// Returns the executed physical route receipt sealed by provider dispatch.
    #[must_use]
    pub const fn physical_route_receipt(&self) -> Option<PhysicalRouteReceipt> {
        self.receipt
    }

    /// Decodes the whole input to exactly one outcome in a single straight-line call. The concrete session runs to
    /// completion, replenishing its own cooperative budget at its loop heads; the fallback adapter, publication
    /// validation, and report sealing all happen once, atomically, around it.
    ///
    /// Every failure is TERMINAL: the error is retained and re-raised by any later call, so no concrete session, and no
    /// interpreter walk, is ever re-entered over state a failed attempt already consumed.
    pub fn decode(&mut self, context: &mut CodecRunContext<'_, '_>) -> Result<AccessResult<'source>, CodecError> {
        if let Some(error) = &self.terminal {
            return Err(error.clone());
        }
        if self.complete {
            return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                contract: "access session already completed",
            }));
        }
        match self.decode_once(context) {
            Ok(result) => Ok(result),
            Err(error) => {
                self.terminal = Some(error.clone());
                Err(error)
            }
        }
    }

    fn decode_once(&mut self, context: &mut CodecRunContext<'_, '_>) -> Result<AccessResult<'source>, CodecError> {
        if let Err(error) = context.resources().check_control() {
            return Err(CodecError::new(CodecFailureKind::Control(error)));
        }
        let source = self.authority;
        let input = AccessInput::Source(source);
        let mut result = if let Some(fallback) = &mut self.fallback {
            let inner = self.state.decode(input, context)?;
            match inner.outcome {
                AccessOutcome::FullDocument(product) => {
                    if fallback
                        .inner_required_coverage
                        .is_satisfied_by(product.document().coverage())
                    {
                        // Retain the inner decode's consumed span so the exact interpreter's report can carry it
                        // instead of dropping it (AccessReport is Copy; `inner.outcome` is moved just below).
                        fallback.inner_consumed_offset = inner.report.consumed_offset();
                        fallback.inner_open_ended = inner.report.open_ended();
                        let stored = fallback.product.insert(product);
                        fallback.interpreter.decode(AccessInput::Document(stored), context)?
                    } else {
                        return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                            contract: "fallback product does not satisfy residual coverage",
                        }));
                    }
                }
                AccessOutcome::Located(_) => {
                    return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                        contract: "fallback route returned located result",
                    }));
                }
            }
        } else {
            self.state.decode(input, context)?
        };
        let product_coverage = match &result.outcome {
            AccessOutcome::FullDocument(product) => Some(product.document().coverage()),
            AccessOutcome::Located(located) => Some(located.product().document().coverage()),
        };
        let actual_result = match &result.outcome {
            AccessOutcome::FullDocument(_) => AccessResultKind::CompleteDocument,
            AccessOutcome::Located(_) => AccessResultKind::Located,
        };
        if !self
            .expected_result
            .is_some_and(|expected| publishes_outcome_shape(expected, actual_result))
        {
            return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                contract: "published access result kind differs from sealed plan",
            }));
        }
        if self
            .required_coverage
            .is_some_and(|required| product_coverage.is_some_and(|coverage| !required.is_satisfied_by(coverage)))
        {
            return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                contract: "published product does not satisfy access coverage",
            }));
        }
        if let Err(error) = context.resources().check_control() {
            return Err(CodecError::new(CodecFailureKind::Control(error)));
        }
        // Re-apply the inner whole-document decode's consumed span, and its open-ended verdict on that span, to the
        // exact interpreter's report; the interpreter runs over the retained product and never sees the source, so
        // without this both would be lost on the fallback path. Non-fallback results carry their own and are left
        // untouched.
        if let Some(fallback) = self.fallback.as_ref() {
            if let Some(offset) = fallback.inner_consumed_offset {
                result.report = result.report.with_consumed_offset(offset);
            }
            if fallback.inner_open_ended {
                result.report = result.report.with_open_ended();
            }
        }
        let receipt = self.receipt.ok_or_else(|| {
            CodecError::new(CodecFailureKind::InternalContractViolation {
                contract: "published access result without sealed route",
            })
        })?;
        result.report.seal(
            self.expected_diagnostics.ok_or_else(|| {
                CodecError::new(CodecFailureKind::InternalContractViolation {
                    contract: "published access result without sealed diagnostics",
                })
            })?,
            self.expected_adapter.ok_or_else(|| {
                CodecError::new(CodecFailureKind::InternalContractViolation {
                    contract: "published access result without sealed adapter",
                })
            })?,
            receipt,
        );
        self.complete = true;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use core::any::Any;

    use super::{
        AccessInput, AccessOutcome, AccessRequirement, AccessSession, ErasedAccessSession, PhysicalRouteId, RouteSlot,
        required_coverage,
    };
    use crate::pattern::{AccessFootprint, ExactPath};
    use crate::{
        AccessAdapter, AccessGuarantees, AccessResult, CodecDemand, CodecError, CodecRunContext, DiagnosticPolicy,
        DocumentProduct,
    };
    use jqf_data::{AccountedDocumentBuilder, AccountedSemanticNode, DocumentCoverage};
    use jqf_resource::ResourceContext;
    use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

    use crate::test_support::{resources, resources_with_work_credits};

    /// The bind-error prose law, mirroring error.rs's rendering gate: every variant renders as prose, never as Rust
    /// syntax — the CLI prints these through Display, so a brace or variant name here reaches a user.
    #[test]
    fn every_bind_error_rendering_is_prose_not_rust_syntax() {
        use super::{AccessBindError, CapabilityMismatch};
        use alloc::format;
        let mismatches = [
            CapabilityMismatch::Footprint,
            CapabilityMismatch::Result,
            CapabilityMismatch::Demand,
            CapabilityMismatch::Validation,
            CapabilityMismatch::Diagnostics,
        ];
        let mut errors = alloc::vec![AccessBindError::NoRoutes, AccessBindError::InvalidRequirement,];
        for mismatch in mismatches {
            errors.push(AccessBindError::HardMismatch {
                slot: RouteSlot::new(1),
                mismatch,
            });
        }
        let banned = [
            "{",
            "}",
            "::",
            "NoRoutes",
            "InvalidRequirement",
            "HardMismatch",
            "RouteSlot",
            "CapabilityMismatch",
            "Footprint",
        ];
        for error in errors {
            let rendered = format!("{error}");
            for fragment in banned {
                assert!(
                    !rendered.contains(fragment),
                    "bind error rendering leaks Rust syntax: {rendered}"
                );
            }
        }
    }

    /// Seals the complete-exact plan triple the floor-adapter tests share.
    fn seal_exact(
        session: &mut ErasedAccessSession<'_>,
        requirement: &AccessRequirement,
        resources: &ResourceContext<'_>,
    ) {
        session
            .seal_required_coverage(required_coverage(requirement, AccessAdapter::CompleteDocumentExact))
            .expect("coverage");
        session
            .seal_plan(requirement, AccessAdapter::CompleteDocumentExact, true, resources)
            .expect("plan");
        session.seal_route_receipt(9, RouteSlot::new(0)).expect("receipt");
    }
    struct FullState(Option<DocumentProduct<'static>>);
    impl AccessSession for FullState {
        fn as_any_mut(&mut self) -> &mut dyn Any {
            self
        }
        fn decode<'source>(
            &mut self,
            _input: AccessInput<'_, 'source>,
            _context: &mut CodecRunContext<'_, '_>,
        ) -> Result<AccessResult<'source>, CodecError> {
            let product = self
                .0
                .take()
                .ok_or_else(|| CodecError::new(crate::CodecFailureKind::ProviderRouteMismatch))?;
            Ok(AccessResult::from_outcome(AccessOutcome::FullDocument(product)))
        }
    }
    fn product(resources: &ResourceContext<'_>) -> DocumentProduct<'static> {
        let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder");
        let root = builder
            .add_node("test.bool", AccountedSemanticNode::Bool(true), None, resources)
            .expect("root");
        DocumentProduct::try_new(builder.finish(root, resources).expect("document"), resources).expect("product")
    }
    fn exact_requirement(resources: &ResourceContext<'_>) -> AccessRequirement {
        let footprint = AccessFootprint::try_exact(ExactPath::try_new(resources), resources);
        AccessRequirement::try_exact(
            footprint,
            CodecDemand::try_new(resources),
            AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            resources,
        )
        .expect("requirement")
    }

    /// The illegal shapes are UNEXPRESSIBLE through the named constructors; the one runtime-rejected shape left is the
    /// exact constructor handed a whole footprint.
    #[test]
    fn exact_constructor_rejects_a_whole_footprint() {
        let resources = resources();
        assert!(
            AccessRequirement::try_exact(
                AccessFootprint::try_whole(&resources),
                CodecDemand::try_new(&resources),
                AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
                &resources
            )
            .is_err()
        );
    }

    /// The named constructor pins selection origin 0 — the single production selection. An authored non-zero origin
    /// goes through [`AccessRequirement::try_exact_with_origin`] (the residual reconstruction's path), never by
    /// mutating this default.
    #[test]
    fn exact_constructor_defaults_to_selection_origin_zero() {
        let resources = resources();
        let requirement = AccessRequirement::try_exact(
            AccessFootprint::try_exact(ExactPath::try_new(&resources), &resources),
            CodecDemand::try_new(&resources),
            AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            &resources,
        )
        .expect("requirement");
        assert_eq!(
            requirement.schedule().singleton_origin(),
            Some(crate::SelectionOrigin::new(0))
        );
    }

    #[test]
    fn required_coverage_distinguishes_diagnostic_authority() {
        let resources = resources();
        let path = ExactPath::try_new(&resources);
        let footprint = AccessFootprint::try_exact(path, &resources);
        let request = AccessRequirement::try_exact(
            footprint,
            CodecDemand::try_new(&resources),
            AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            &resources,
        )
        .expect("requirement");
        assert!(
            required_coverage(&request, AccessAdapter::None)
                .is_satisfied_by(DocumentCoverage::complete_without_source())
        );
    }

    #[test]
    fn sealed_complete_exact_fallback_executes_and_seals_report_once() {
        let mut resources = resources();
        let requirement = exact_requirement(&resources);
        let source = ResolvedSource::new(SourceRef::new(SourceId::new(44), SourceKind::Input), "test", b"true", 0);
        let built = product(&resources);
        let mut session =
            ErasedAccessSession::try_new_source_with_route(source, PhysicalRouteId::new(77).expect("route"), || {
                Ok(FullState(Some(built)))
            })
            .expect("session");
        seal_exact(&mut session, &requirement, &resources);
        let mut context = CodecRunContext::new(&mut resources);
        let result = session.decode(&mut context).expect("decode");
        assert!(matches!(result.outcome(), AccessOutcome::Located(_)));
        assert_eq!(result.report().adapter(), AccessAdapter::CompleteDocumentExact);
        assert_eq!(
            result.report().diagnostics(),
            jqf_data::DiagnosticCoverage::NotRequested
        );
        assert_eq!(result.report().route().expect("route").route().get(), 77);
        // A completed session is single-outcome: a second decode is a contract violation rather than a `Complete`
        // terminal (the straight-line contract — the reuse path resets the session instead).
        assert!(session.decode(&mut context).is_err());
    }

    struct FullStateWithConsumed(Option<DocumentProduct<'static>>, u64);
    impl AccessSession for FullStateWithConsumed {
        fn as_any_mut(&mut self) -> &mut dyn Any {
            self
        }
        fn decode<'source>(
            &mut self,
            _input: AccessInput<'_, 'source>,
            _context: &mut CodecRunContext<'_, '_>,
        ) -> Result<AccessResult<'source>, CodecError> {
            let product = self
                .0
                .take()
                .ok_or_else(|| CodecError::new(crate::CodecFailureKind::ProviderRouteMismatch))?;
            Ok(AccessResult::from_outcome_with_consumed_offset(
                AccessOutcome::FullDocument(product),
                self.1,
            ))
        }
    }

    /// The complete-exact fallback interpreter runs over the retained product and cannot observe the source, so the
    /// inner whole-decode's `consumed_offset` must be propagated onto the exact result's report rather than dropped —
    /// otherwise a fallback exact access over one of several adjacent texts would lose where the next value begins.
    #[test]
    fn sealed_complete_exact_fallback_propagates_inner_consumed_offset() {
        let mut resources = resources();
        let requirement = exact_requirement(&resources);
        let source = ResolvedSource::new(
            SourceRef::new(SourceId::new(46), SourceKind::Input),
            "test",
            b"true rest",
            0,
        );
        let built = product(&resources);
        let mut session =
            ErasedAccessSession::try_new_source_with_route(source, PhysicalRouteId::new(77).expect("route"), || {
                Ok(FullStateWithConsumed(Some(built), 4))
            })
            .expect("session");
        seal_exact(&mut session, &requirement, &resources);
        let mut context = CodecRunContext::new(&mut resources);
        let result = session.decode(&mut context).expect("decode");
        assert!(matches!(result.outcome(), AccessOutcome::Located(_)));
        assert_eq!(result.report().consumed_offset(), Some(4));
    }

    /// `[true]`: an index step resolves, and a second index step on the resolved bool is a type mismatch rather than a
    /// node.
    fn single_element_array_product(resources: &ResourceContext<'_>) -> DocumentProduct<'static> {
        let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder");
        let element = builder
            .add_node("test.bool", AccountedSemanticNode::Bool(true), None, resources)
            .expect("element");
        let root = builder
            .add_node(
                "test.array",
                AccountedSemanticNode::Array { item_role: "test.item" },
                None,
                resources,
            )
            .expect("root");
        builder
            .add_occurrence(
                jqf_data::LocalOwnerRef::Node(root),
                "test.item",
                None,
                element,
                resources,
            )
            .expect("edge");
        DocumentProduct::try_new(builder.finish(root, resources).expect("document"), resources).expect("product")
    }

    fn two_step_requirement(resources: &ResourceContext<'_>) -> AccessRequirement {
        let mut path = ExactPath::try_new(resources);
        path.try_push_semantic_index(0, resources);
        path.try_push_semantic_index(0, resources);
        let footprint = AccessFootprint::try_exact(path, resources);
        AccessRequirement::try_exact(
            footprint,
            CodecDemand::try_new(resources),
            AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            resources,
        )
        .expect("requirement")
    }

    /// A fallback-interpreter failure raised MID-WALK is terminal: a second decode re-raises it instead of resuming the
    /// walk from the document root, which would publish whatever the REMAINING steps name from the root rather than
    /// what the whole path names.
    #[test]
    fn a_mid_walk_fallback_failure_is_re_raised_instead_of_re_walked_from_the_root() {
        let resources = resources();
        let requirement = two_step_requirement(&resources);
        let source = ResolvedSource::new(
            SourceRef::new(SourceId::new(47), SourceKind::Input),
            "test",
            b"[true]",
            0,
        );
        let built = single_element_array_product(&resources);
        let mut session =
            ErasedAccessSession::try_new_source_with_route(source, PhysicalRouteId::new(77).expect("route"), || {
                Ok(FullState(Some(built)))
            })
            .expect("session");
        seal_exact(&mut session, &requirement, &resources);
        // One credit admits the first step and reports `Pending` at the second; the run context carries no cooperative
        // budget, so the interpreter fails between the two steps.
        let mut exhausting = resources_with_work_credits(1);
        let mut context = CodecRunContext::new(&mut exhausting);
        let first = session
            .decode(&mut context)
            .expect_err("the exhausted meter fails the walk mid-path");
        assert!(matches!(
            first.kind(),
            crate::CodecFailureKind::InternalContractViolation { .. }
        ));
        // A replenished meter would let a resumed walk finish — from the root.
        let mut replenished = resources_with_work_credits(4096);
        let mut context = CodecRunContext::new(&mut replenished);
        let second = session.decode(&mut context).expect_err("terminal");
        assert_eq!(second.kind(), first.kind());
    }

    /// The concrete state fails once and would succeed on a second call; the direct route (no fallback adapter) must
    /// never give it that second call.
    struct FailsOnceState(Option<DocumentProduct<'static>>);
    impl AccessSession for FailsOnceState {
        fn as_any_mut(&mut self) -> &mut dyn Any {
            self
        }
        fn decode<'source>(
            &mut self,
            _input: AccessInput<'_, 'source>,
            _context: &mut CodecRunContext<'_, '_>,
        ) -> Result<AccessResult<'source>, CodecError> {
            match self.0.take() {
                Some(_) => Err(CodecError::new(crate::CodecFailureKind::InvalidInput)),
                None => Err(CodecError::new(crate::CodecFailureKind::ProviderRouteMismatch)),
            }
        }
    }

    #[test]
    fn a_direct_route_decode_failure_is_re_raised_instead_of_re_entered() {
        let mut resources = resources();
        let requirement = exact_requirement(&resources);
        let source = ResolvedSource::new(SourceRef::new(SourceId::new(48), SourceKind::Input), "test", b"true", 0);
        let built = product(&resources);
        let mut session =
            ErasedAccessSession::try_new_source(source, || Ok(FailsOnceState(Some(built)))).expect("session");
        session
            .seal_required_coverage(required_coverage(&requirement, AccessAdapter::None))
            .expect("coverage");
        session
            .seal_plan(&requirement, AccessAdapter::None, false, &resources)
            .expect("plan");
        session.seal_route_receipt(11, RouteSlot::new(0)).expect("receipt");
        let mut context = CodecRunContext::new(&mut resources);
        let first = session.decode(&mut context).expect_err("first decode fails");
        assert_eq!(first.kind(), crate::CodecFailureKind::InvalidInput);
        let second = session.decode(&mut context).expect_err("terminal");
        assert_eq!(second.kind(), crate::CodecFailureKind::InvalidInput);
    }

    #[test]
    fn publication_rejects_wrong_result_kind_and_stabilizes_failure() {
        let mut resources = resources();
        let requirement = exact_requirement(&resources);
        let source = ResolvedSource::new(SourceRef::new(SourceId::new(45), SourceKind::Input), "test", b"true", 0);
        let built = product(&resources);
        let mut session = ErasedAccessSession::try_new_source(source, || Ok(FullState(Some(built)))).expect("session");
        session
            .seal_required_coverage(required_coverage(&requirement, AccessAdapter::None))
            .expect("coverage");
        session
            .seal_plan(&requirement, AccessAdapter::None, false, &resources)
            .expect("plan");
        session.seal_route_receipt(10, RouteSlot::new(0)).expect("receipt");
        let mut context = CodecRunContext::new(&mut resources);
        assert!(session.decode(&mut context).is_err());
        // The failure is terminal-stable: a second decode re-raises it rather than running again.
        assert!(session.decode(&mut context).is_err());
    }
}
