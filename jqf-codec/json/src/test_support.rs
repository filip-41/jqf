// Shared test fixtures for unit tests and integration tests.

use jqf_codec_core::{AccessGuarantees, AccessRequirement, CodecDemand, DemandClause, DiagnosticPolicy, FactIntent};
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

/// Shared continue control for [`resources`] and custom test ledgers.
pub static CONTROL: ContinueControl = ContinueControl;

/// Unlimited ledger for tests that only care about correctness, not residency.
///
/// # Panics
///
/// Panics if the unlimited account, work meter, or resource context cannot be constructed — a test-fixture invariant,
/// not user input.
#[must_use]
pub fn resources() -> ResourceContext<'static> {
    ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
            .expect("account"),
        &CONTROL,
        WorkMeter::try_new_v1(4_096).expect("work"),
    )
    .expect("resources")
}

/// The whole-document demand a dialect decode raises: production inserts exactly `SemanticRoot` + `ValueShape`.
///
/// # Panics
///
/// Panics if clause insertion fails — a fixture invariant, not user input. Shared across five integration crates;
/// each target uses its own subset.
#[allow(dead_code)]
pub fn demand(resources: &ResourceContext<'_>) -> CodecDemand {
    let mut demand = CodecDemand::try_new(resources);
    demand.try_insert(&DemandClause::SemanticRoot).expect("semantic root");
    demand.try_insert(&DemandClause::ValueShape).expect("value shape");
    demand
}

/// The strict whole-document requirement the single-document codecs decode under.
///
/// # Panics
///
/// Panics if the requirement cannot bind — a fixture invariant, not user input. Shared across five integration
/// crates; each target uses its own subset.
#[allow(dead_code)]
pub fn requirement(resources: &ResourceContext<'_>) -> AccessRequirement {
    let guarantees = AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly);
    AccessRequirement::try_whole(demand(resources), guarantees, resources).expect("whole requirement")
}

/// [`requirement`] with comment facts preserved, for the commented-JSON decodes that attach `jsonc.comment@1` /
/// `json5.comment@1`. Shared across five integration crates; each target uses its own subset.
#[allow(dead_code)]
pub fn requirement_preserving_facts(resources: &ResourceContext<'_>) -> AccessRequirement {
    requirement(resources).with_fact_intent(FactIntent::Preserve)
}
