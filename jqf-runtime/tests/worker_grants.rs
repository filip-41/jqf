//! Envelope sizing, the oversized-record law, and the degradation seam.

use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_runtime::records::{WorkerRequest, plan_request};
use jqf_runtime::workers::{RecordWorkerEnvelope, reserve_record_worker_grants};
use jqf_sdk::RECORD_BATCH_TARGET_BYTES;

static CONTINUE: ContinueControl = ContinueControl;

/// The CLI's own default aggregate memory ceiling.
const DEFAULT_CEILING_BYTES: u64 = 512 << 20;

fn parent_resources(max_memory_bytes: u64) -> ResourceContext<'static> {
    ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, max_memory_bytes, u64::MAX, 64))
            .expect("parent account"),
        &CONTINUE,
        WorkMeter::try_new_v1(4_096).expect("work meter"),
    )
    .expect("parent resources")
}

/// The SDK's batch-bound construction (`RECORD_BATCH_TARGET_BYTES` = 256 KiB). Production does not use this: the relay
/// builds envelopes from `plan.morsel_bytes()`. This helper pins the batch-bound receipt so a regression that sizes
/// from the batch bound is visible.
fn window_envelope() -> RecordWorkerEnvelope {
    RecordWorkerEnvelope::try_new(RECORD_BATCH_TARGET_BYTES, RecordWorkerEnvelope::MEASURED_FIXED_BYTES)
        .expect("window envelope")
}

/// PRODUCTION's construction: `jqf-runtime/src/parallel/relay.rs` builds the worker envelope from `plan.morsel_bytes()`
/// (morsels range 128 KiB..4 MiB per `RECORD_WIDTH_POLICY`), never from the SDK batch bound. 2 MiB is the mid-range
/// representative morsel size; the regression guard that feeds a REAL plan's morsel size through the relay's
/// construction is `a_real_record_plan_morsel_envelope_admits_many_workers` below.
const REPRESENTATIVE_MORSEL_BYTES: u64 = 2 << 20;

fn morsel_envelope() -> RecordWorkerEnvelope {
    RecordWorkerEnvelope::try_new(REPRESENTATIVE_MORSEL_BYTES, RecordWorkerEnvelope::MEASURED_FIXED_BYTES)
        .expect("morsel envelope")
}

#[test]
fn an_envelope_sized_from_the_window_admits_many_workers() {
    let resources = parent_resources(DEFAULT_CEILING_BYTES);
    let baseline = resources.snapshot();
    let grants = reserve_record_worker_grants(50, window_envelope(), &resources).expect("grants");

    assert_eq!(grants.report().requested(), 50);
    assert_eq!(
        grants.report().granted(),
        50,
        "the default 512 MiB ceiling admits 50 window-sized envelopes"
    );
    assert!(!grants.report().degraded());

    drop(grants);
    assert_eq!(
        resources.snapshot().memory_current_bytes(),
        baseline.memory_current_bytes()
    );
}

/// The morsel-derived envelope admits wide requests at a representative morsel size — the receipt production actually
/// exercises.
#[test]
fn an_envelope_sized_from_the_plan_morsel_admits_many_workers() {
    let resources = parent_resources(DEFAULT_CEILING_BYTES);
    let baseline = resources.snapshot();
    let grants = reserve_record_worker_grants(50, morsel_envelope(), &resources).expect("grants");
    let granted = grants.report().granted();

    println!("morsel-sized envelope (2 MiB): granted {granted} of 50");

    assert_eq!(grants.report().requested(), 50);
    assert_eq!(
        granted, 50,
        "the default 512 MiB ceiling admits 50 envelopes at a representative 2 MiB morsel size"
    );
    assert!(!grants.report().degraded());

    drop(grants);
    assert_eq!(
        resources.snapshot().memory_current_bytes(),
        baseline.memory_current_bytes(),
        "dropping the grants returns every envelope byte to the parent"
    );
}

/// Production builds the envelope from `plan.morsel_bytes()` (relay.rs `relay_morsels`), never from the SDK batch
/// bound. This test REPLICATES the relay's documented construction — a plan from the record route's own planner, then
/// `RecordWorkerEnvelope::try_new(plan.morsel_bytes(), …)` with the exact call-site arguments relay.rs uses — so the
/// envelope under test is the one production builds, not a test-side constant. (It does not INVOKE the relay: a
/// regression at the relay's own call site is guarded by the planner-side and size-side pins below, not by invoking the
/// relay itself.)
///
/// The `morsel_bytes() > RECORD_BATCH_TARGET_BYTES` pin is load-bearing: it proves the value under test is NOT the 256
/// KiB batch bound, so a regression that substitutes the batch bound for `plan.morsel_bytes()` swaps in a strictly
/// SMALLER envelope (the assertion below checks that too), and a smaller output component is exactly what made every
/// large morsel yield to serial.
#[test]
fn a_real_record_plan_morsel_envelope_admits_many_workers() {
    // 2 GiB keeps the morsel strictly above the 256 KiB batch bound even at the 256-worker hard cap (2 GiB / (256
    // workers * 4 morsels) = 2 MiB); only a machine with thousands of physical cores could clamp it down to the batch
    // bound.
    let plan = plan_request(WorkerRequest::Auto, 2 << 30, None);
    assert!(plan.is_parallel(), "a 2 GiB record request must plan parallel");
    assert!(
        plan.morsel_bytes() > RECORD_BATCH_TARGET_BYTES,
        "a real morsel is strictly larger than the 256 KiB SDK batch bound \
         (morsels range 128 KiB..4 MiB): {}",
        plan.morsel_bytes()
    );

    // The relay's own construction (relay.rs `relay_morsels`).
    let envelope = RecordWorkerEnvelope::try_new(plan.morsel_bytes(), RecordWorkerEnvelope::MEASURED_FIXED_BYTES)
        .expect("relay envelope");
    let batch_bound_envelope =
        RecordWorkerEnvelope::try_new(RECORD_BATCH_TARGET_BYTES, RecordWorkerEnvelope::MEASURED_FIXED_BYTES)
            .expect("batch-bound envelope");
    assert!(
        batch_bound_envelope
            .task_grant_limits()
            .expect("limits")
            .total_memory_bytes()
            .expect("total")
            < envelope
                .task_grant_limits()
                .expect("limits")
                .total_memory_bytes()
                .expect("total"),
        "the batch-bound envelope (the regression's construction) is strictly \
         smaller than the morsel-sized one"
    );

    let resources = parent_resources(DEFAULT_CEILING_BYTES);
    let baseline = resources.snapshot();
    // 24 is SAFE under the default 512 MiB ceiling at the record route's LARGEST morsel (4 MiB: ~16.6 MiB per worker,
    // so 512 MiB / 16.6 MiB ≈ 30): which keeps the width assertion machine-independent: every real morsel up to the 4
    // MiB maximum grants the full 24.
    let grants = reserve_record_worker_grants(24, envelope, &resources).expect("grants");

    println!(
        "plan: workers={} morsel_bytes={}; envelope granted {} of 24",
        plan.workers(),
        plan.morsel_bytes(),
        grants.report().granted()
    );
    assert_eq!(grants.report().requested(), 24);
    assert_eq!(
        grants.report().granted(),
        24,
        "the real relay envelope admits the full width"
    );
    assert!(!grants.report().degraded());

    drop(grants);
    assert_eq!(
        resources.snapshot().memory_current_bytes(),
        baseline.memory_current_bytes(),
        "dropping the grants returns every envelope byte to the parent"
    );
}

#[test]
fn zero_requested_workers_is_the_serial_path() {
    let resources = parent_resources(DEFAULT_CEILING_BYTES);
    let grants = reserve_record_worker_grants(0, window_envelope(), &resources).expect("grants");

    assert_eq!(grants.report().granted(), 0);
    assert!(!grants.report().degraded());
}

/// A parent that can afford one envelope but not fifty reports the shrink.
#[test]
fn a_tight_ceiling_degrades_the_granted_worker_count() {
    let envelope = window_envelope();
    let per_worker = envelope
        .task_grant_limits()
        .expect("limits")
        .total_memory_bytes()
        .expect("total");
    let resources = parent_resources(per_worker + 1024);
    let grants = reserve_record_worker_grants(50, envelope, &resources).expect("grants");
    assert_eq!(grants.report().requested(), 50);
    assert_eq!(grants.report().granted(), 1);
    assert!(grants.report().degraded());
    drop(grants);
}
