//! The one session-drive scaffold shared by every codec battery: source
//! setup, resource context/limits, the poll-loop resume step, straight-line
//! decode, and pass/fail reporting. Per-codec modules (`smoke/<codec>.rs`,
//! `differential/<codec>/`) call into this file.

use jqf_codec_core::{
    AccessFootprint, AccessGuarantees, AccessRequirement, CodecDemand, DemandClause, DiagnosticPolicy, ExactPath,
};
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

static CONTROL: ContinueControl = ContinueControl;

/// A caller-owned resource context with the default (maximal) limits: the
/// battery's ordinary drive context.
pub fn resources() -> ResourceContext<'static> {
    ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
            .expect("account"),
        &CONTROL,
        WorkMeter::try_new_v1(4_096).expect("work"),
    )
    .expect("resources")
}

/// A resource context whose nesting ceiling is `depth` levels: the
/// `resource_depth` receipt proves the guards reject at the ceiling with the
/// resource error, exactly as the JSON codec's does.
pub fn limited_resources(depth: u32) -> ResourceContext<'static> {
    ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, depth)).expect("account"),
        &CONTROL,
        WorkMeter::try_new_v1(4_096).expect("work"),
    )
    .expect("resources")
}

/// A resolved input source for a battery fixture.
pub fn source(bytes: &[u8]) -> ResolvedSource<'_> {
    ResolvedSource::new(
        SourceRef::new(SourceId::new(94), SourceKind::Input),
        "smoke.input",
        bytes,
        0,
    )
}

/// The whole-document (`CompleteDocument`) requirement.
pub fn whole_requirement(resources: &ResourceContext<'_>) -> AccessRequirement {
    let mut demand = CodecDemand::try_new(resources);
    demand.try_insert(&DemandClause::SemanticRoot).expect("semantic root");
    demand.try_insert(&DemandClause::ValueShape).expect("value shape");
    AccessRequirement::try_whole(
        demand,
        AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
        resources,
    )
    .expect("requirement")
}

/// Builds an exact-path requirement: `members` name semantic member steps in
/// order, `index` a trailing semantic index step, `range` a trailing slice.
pub fn exact_requirement(
    resources: &ResourceContext<'_>,
    members: &[&str],
    index: Option<i64>,
    range: Option<(Option<i64>, Option<i64>)>,
) -> AccessRequirement {
    let mut demand = CodecDemand::try_new(resources);
    demand.try_insert(&DemandClause::SemanticRoot).expect("semantic root");
    demand.try_insert(&DemandClause::ValueShape).expect("value shape");
    let mut path = ExactPath::try_new(resources);
    for member in members {
        path.try_push_semantic_member(member, resources).expect("member");
    }
    if let Some(index) = index {
        path.try_push_semantic_index(index, resources);
    }
    if let Some((start, end)) = range {
        path.try_push_semantic_range(start, end, resources);
    }
    let footprint = AccessFootprint::try_exact(path, resources);
    AccessRequirement::try_exact(
        footprint,
        demand,
        AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
        resources,
    )
    .expect("requirement")
}

/// The Pending arm of every poll loop: the codec asks for more cooperative
/// credit, which the battery grants.
pub fn resume(resources: &mut ResourceContext<'_>) {
    resources.try_begin_next_cooperative_entry(4_096).expect("resume");
}

/// The pass/fail reporting shape: a battery `run` prints its own receipt line
/// and returns `Err(message)` on failure; this prints
/// `{codec}-smoke: FAILED: {message}` and exits 1.
pub fn run_smoke(codec: &str, run: impl FnOnce() -> Result<(), String>) {
    if let Err(error) = run() {
        eprintln!("{codec}-smoke: FAILED: {error}");
        std::process::exit(1);
    }
}

/// Decodes one access session to its single outcome. The cooperative budget
/// is the battery's 4 096 credit resume value, set on the run context so the
/// codec replenishes at its own loop heads.
pub fn decode_session<'source>(
    session: &mut jqf_codec_core::ErasedAccessSession<'source>,
    resources: &mut ResourceContext<'_>,
) -> Result<jqf_codec_core::AccessResult<'source>, jqf_codec_core::CodecError> {
    let mut run = jqf_codec_core::CodecRunContext::new(resources);
    run.set_cooperative_credits(4_096);
    session.decode(&mut run)
}
