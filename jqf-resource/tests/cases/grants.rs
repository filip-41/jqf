//! Numeric grant, output-permit, nesting, and spill-disk contracts without a counting global allocator. Physical
//! allocation attribution is covered by the sibling `counting_alloc` and `worker_ambient` test binaries.

use jqf_resource::task::{DetachedTaskBuffer, TaskGrantBudget, TaskGrantLimits, TaskOutputBuffer};
use jqf_resource::{
    ContinueControl, Control, ControlError, ControlOutcome, RequestAccount, ResourceContext, ResourceError,
    ResourceLimit, ResourceLimits, WorkMeter,
};

static CONTINUE: ContinueControl = ContinueControl;

fn assert_send<T: Send>() {}

fn work() -> WorkMeter {
    WorkMeter::try_new_v1(4096).expect("valid work meter")
}

fn grant_limits(output_bytes: u64) -> TaskGrantLimits {
    TaskGrantLimits {
        retained_bytes: RequestAccount::minimum_memory_bytes() + 128,
        working_bytes: 256,
        pending_io_bytes: 128,
        output_bytes,
    }
}

fn parent_resources(extra_memory: u64) -> ResourceContext<'static> {
    let limits = ResourceLimits::new(
        u64::MAX,
        u64::MAX,
        RequestAccount::minimum_memory_bytes() + extra_memory,
        u64::MAX,
        64,
    );
    ResourceContext::new(
        RequestAccount::try_new(limits).expect("parent account"),
        &CONTINUE,
        work(),
    )
    .expect("parent resources")
}

/// A shared handle and the original account are one ledger: an ambient charge appears on the context snapshot, and a
/// grant hold appears on the ambient snapshot. That is the one-account law.
#[test]
fn a_shared_handle_is_the_same_ledger_as_the_context_account() {
    use jqf_resource::{MemoryCategory, install};
    let resources = parent_resources(64 * 1024);
    let share = resources.account().try_share().expect("share the parent account");
    let _scope = install(share);
    let before = resources.snapshot().memory(MemoryCategory::Working).current();
    jqf_resource::with_account(|account| account.charge_residency(1024, MemoryCategory::Working))
        .expect("ambient account")
        .expect("ambient charge");
    let after_charge = resources.snapshot().memory(MemoryCategory::Working).current();
    assert_eq!(
        after_charge,
        before + 1024,
        "an ambient charge must land on the context account"
    );

    let limits = grant_limits(32);
    let (reservation, _budget) = resources.reserve_task_grant(limits).expect("reserve grant");
    let after_hold = jqf_resource::with_account(RequestAccount::snapshot)
        .expect("ambient still installed")
        .memory(MemoryCategory::Working)
        .current();
    assert!(
        after_hold > after_charge,
        "a grant hold must land on the ambient view of the same ledger"
    );
    drop(reservation);
}

/// A child ledger's parent-dimension accessors report `u64::MAX`, so a deref to `ResourceContext` would otherwise admit
/// unlimited input and spill-disk charges. Those methods refuse instead (a refused morsel falls back to the serial
/// rerun, where the parent meters both).
#[test]
fn a_child_context_refuses_parent_only_charges() {
    let resources = parent_resources(8192);
    let (reservation, budget) = resources.reserve_task_grant(grant_limits(32)).expect("reserve grant");
    let child = budget.open_child().expect("open child");
    let child = child.bind(&CONTINUE, work()).expect("bind child");
    let limits = child.limits();
    assert_eq!(limits.max_input_bytes(), u64::MAX);
    assert_eq!(limits.max_output_bytes(), u64::MAX);
    assert_eq!(limits.max_spill_disk_bytes(), u64::MAX);
    assert_eq!(child.charge_input(1), Err(ResourceError::AccountingInvariantViolation));
    assert_eq!(
        child.charge_spill_disk(1),
        Err(ResourceError::AccountingInvariantViolation)
    );
    drop(child);
    drop(reservation);
}

/// Output is NOT a refused parent-only dimension: the publish path reserves an output permit on whatever context is
/// driving, so a worker child must issue permits or every parallel morsel fails its first published byte and the
/// request silently degrades to the serial rerun. The child's real bound is its grant's output authority; the permit
/// stays uncharged, exactly like probe mode, so commit and drop cannot underflow cells that never grew.
#[test]
fn a_child_context_issues_uncharged_output_permits() {
    let resources = parent_resources(8192);
    let (reservation, budget) = resources.reserve_task_grant(grant_limits(32)).expect("reserve grant");
    let child = budget.open_child().expect("open child");
    let child = child.bind(&CONTINUE, work()).expect("bind child");
    let permit = child
        .reserve_output(16)
        .expect("a worker publish reserves on the child context");
    permit.commit(8).expect("commit the published prefix");
    let rolled_back = child.reserve_output(16).expect("second permit");
    drop(rolled_back);
    assert_eq!(
        child.snapshot().output_bytes(),
        0,
        "an uncharged permit books nothing on the child ledger"
    );
    drop(child);
    drop(reservation);
}

/// The default request never configures a disk ceiling, and `0` is UNSET on that dimension alone: the charge records
/// the cumulative footprint and always succeeds, so the spill fallback law answers identically whether or not a host
/// asked for a ceiling.
#[test]
fn an_unset_spill_disk_ceiling_admits_every_charge_and_still_counts() {
    let resources = parent_resources(4096);
    assert_eq!(resources.limits().max_spill_disk_bytes(), 0, "unset");
    resources.charge_spill_disk(1 << 40).expect("unset admits");
    resources.charge_spill_disk(64).expect("unset admits");
    assert_eq!(
        resources.snapshot().spill_disk_bytes(),
        (1 << 40) + 64,
        "an unset ceiling still observes the footprint"
    );
}

/// The other side of the same dimension: a ceiling the host DID configure is cumulative and hard. The refusal reports
/// the disk dimension, not the aggregate memory prose.
#[test]
fn a_configured_spill_disk_ceiling_refuses_past_its_cumulative_total() {
    let limits = ResourceLimits::new(
        u64::MAX,
        u64::MAX,
        RequestAccount::minimum_memory_bytes() + 4096,
        u64::MAX,
        64,
    )
    .with_max_spill_disk_bytes(1024);
    let resources =
        ResourceContext::new(RequestAccount::try_new(limits).expect("account"), &CONTINUE, work()).expect("resources");

    resources.charge_spill_disk(1000).expect("under the ceiling");
    let error = resources
        .charge_spill_disk(25)
        .expect_err("the cumulative total crosses the ceiling");
    assert_eq!(
        error,
        ResourceError::LimitExceeded {
            limit_kind: ResourceLimit::SpillDiskBytes,
            limit: 1024,
            current: 1000,
            requested_delta: 25,
        }
    );
    assert_eq!(
        resources.snapshot().spill_disk_bytes(),
        1000,
        "a refused charge writes nothing"
    );
}

#[test]
fn send_side_is_closed_to_numeric_budget_and_detached_bytes() {
    assert_send::<TaskGrantBudget>();
    assert_send::<DetachedTaskBuffer>();
}

/// A worker's exact result capacity survives detach and adoption byte for byte on the parent's thread.
#[test]
fn worker_detaches_and_parent_adopts_exact_capacity() {
    let resources = parent_resources(8192);
    let limits = grant_limits(128);
    let (reservation, budget) = resources.reserve_task_grant(limits).expect("reserve grant");

    let detached = std::thread::spawn(move || {
        let child = budget.open_child().expect("open child");
        let mut child = child.bind(&CONTINUE, work()).expect("bind child");
        let mut output = TaskOutputBuffer::try_with_capacity(9, &mut child).expect("allocate child output");
        output
            .try_append_within_capacity(b"abcdefghi")
            .expect("write child output");
        child.detach_result(output).expect("detach child output")
    })
    .join()
    .expect("worker joins");

    let exact_capacity = detached.capacity() as u64;
    assert!(exact_capacity >= 9);
    assert!(exact_capacity <= limits.output_bytes);

    let adopted = reservation.adopt(detached).expect("adopt result");
    assert_eq!(adopted.as_slice(), b"abcdefghi");
    assert_eq!(adopted.capacity() as u64, exact_capacity);

    drop(adopted);
}

#[test]
fn adoption_charges_published_bytes_not_the_grant_ceiling() {
    use jqf_resource::MemoryCategory;

    let resources = parent_resources(8192);
    let before = resources.snapshot().memory(MemoryCategory::Working).current();
    let limits = grant_limits(64);
    let (reservation, budget) = resources.reserve_task_grant(limits).expect("reserve grant");
    let child = budget.open_child().expect("open child");
    let mut child = child.bind(&CONTINUE, work()).expect("bind child");
    let mut output = TaskOutputBuffer::try_with_capacity(64, &mut child).expect("ceiling");
    output.try_append_within_capacity(b"abcd").expect("publish four");
    let detached = child.detach_result(output).expect("detach");
    assert_eq!(detached.len(), 4);
    assert!(
        detached.capacity() < 64,
        "unused grant ceiling is not kept after detach"
    );
    let adopted = reservation.adopt(detached).expect("adopt");
    assert_eq!(adopted.as_slice(), b"abcd");
    assert!(adopted.capacity() < 64);
    assert_eq!(
        resources.snapshot().memory(MemoryCategory::Working).current(),
        before + adopted.capacity() as u64,
        "the parent retains exactly the adopted allocation capacity"
    );
    drop(adopted);
    assert_eq!(resources.snapshot().memory(MemoryCategory::Working).current(), before);
}

/// The one-shot output authority is a NUMERIC grant law that survives the ambient rework: a child can still detach only
/// one result against one reservation.
#[test]
fn one_child_cannot_detach_two_live_results_against_one_reservation() {
    let resources = parent_resources(8192);
    let limits = grant_limits(64);
    let (reservation, budget) = resources.reserve_task_grant(limits).expect("reserve grant");
    let child = budget.open_child().expect("open child");
    let mut child = child.bind(&CONTINUE, work()).expect("bind child");

    let first = TaskOutputBuffer::try_with_capacity(64, &mut child).expect("first output");
    let second = TaskOutputBuffer::try_with_capacity(1, &mut child)
        .err()
        .expect("the one-shot output authority was already consumed");
    assert_eq!(second, ResourceError::AccountingInvariantViolation);
    let first = child.detach_result(first).expect("first detach");
    assert_eq!(first.len(), 0);
    assert!(first.capacity() <= 64);

    let adopted = reservation.adopt(first).expect("adopt first");
    assert_eq!(adopted.len(), 0);
    assert!(adopted.capacity() <= 64);
    drop(adopted);
}

/// Adoption is guarded by the grant RECEIPT, not by account identity: a swapped inner context still carries this
/// child's output authority, so `try_with_capacity` charges the swapped context's own account and the matching
/// reservation still adopts. The row that would bite is a receipt mismatch, pinned by
/// `a_detached_result_cannot_cross_grant_identity`.
#[test]
fn adoption_is_guarded_by_the_grant_receipt_not_the_inner_account() {
    let resources = parent_resources(8192);
    let limits = grant_limits(32);
    let (reservation, budget) = resources.reserve_task_grant(limits).expect("reserve grant");
    let child = budget.open_child().expect("open child");
    let mut child = child.bind(&CONTINUE, work()).expect("bind child");

    let foreign = parent_resources(8192);
    let original = core::mem::replace(&mut *child, foreign);
    let output = TaskOutputBuffer::try_with_capacity(4, &mut child).expect("swapped context allocates");
    let detached = child.detach_result(output).expect("detach");
    let adopted = reservation.adopt(detached).expect("matching parent adopts");
    drop(adopted);
    let _ = original;
}

/// Detachment consumes a child ledger that is exactly its baseline plus the one result allocation. A forgotten
/// `DepthGuard` leaves the depth counter nonzero, so detachment rejects the non-quiescent child.
#[test]
fn detachment_consumes_a_fully_quiescent_child_ledger() {
    let resources = parent_resources(8192);
    let limits = grant_limits(32);
    let (reservation, budget) = resources.reserve_task_grant(limits).expect("reserve grant");
    let child = budget.open_child().expect("open child");
    let mut child = child.bind(&CONTINUE, work()).expect("bind child");
    let output = TaskOutputBuffer::try_with_capacity(8, &mut child).expect("output");
    let lingering = child.enter_nesting().expect("lingering depth guard");
    core::mem::forget(lingering);

    let error = child
        .detach_result(output)
        .err()
        .expect("live worker state must prevent detachment");
    assert_eq!(error, ResourceError::AccountingInvariantViolation);
    drop(reservation);

    let (reservation, budget) = resources.reserve_task_grant(limits).expect("second grant");
    let child = budget.open_child().expect("open second child");
    let mut child = child.bind(&CONTINUE, work()).expect("bind second child");
    let output = TaskOutputBuffer::try_with_capacity(0, &mut child).expect("empty output");
    let detached = child.detach_result(output).expect("quiescent detach");
    drop(detached);
    drop(reservation);
}

#[test]
fn output_capacity_is_bounded_by_its_numeric_grant() {
    let resources = parent_resources(4096);
    let limits = grant_limits(8);
    let (reservation, budget) = resources.reserve_task_grant(limits).expect("reserve grant");
    let child = budget.open_child().expect("open child");
    let mut child = child.bind(&CONTINUE, work()).expect("bind child");

    let error = TaskOutputBuffer::try_with_capacity(9, &mut child)
        .err()
        .expect("over-capacity output must fail");
    assert!(matches!(
        error,
        ResourceError::LimitExceeded {
            limit_kind: jqf_resource::ResourceLimit::MemoryBytes,
            limit: 8,
            current: 0,
            requested_delta: 9,
        }
    ));

    let mut output = TaskOutputBuffer::try_with_capacity(4, &mut child).expect("bounded output");
    output
        .try_append_within_capacity(b"data")
        .expect("fill initial capacity");
    assert_eq!(output.as_slice(), b"data");
    assert_eq!(output.capacity(), 4);

    drop(output);
    drop(child);
    drop(reservation);
}

#[test]
fn an_over_capacity_append_leaves_the_output_unchanged() {
    let resources = parent_resources(4096);
    let (reservation, budget) = resources.reserve_task_grant(grant_limits(4)).expect("reserve grant");
    let child = budget.open_child().expect("open child");
    let mut child = child.bind(&CONTINUE, work()).expect("bind child");
    let mut output = TaskOutputBuffer::try_with_capacity(4, &mut child).expect("bounded output");
    output.try_append_within_capacity(b"ab").expect("initial append");

    assert_eq!(
        output.try_append_within_capacity(b"cdef"),
        Err(ResourceError::LimitExceeded {
            limit_kind: ResourceLimit::MemoryBytes,
            limit: 4,
            current: 4,
            requested_delta: 2,
        })
    );
    assert_eq!(output.as_slice(), b"ab");

    drop(output);
    drop(child);
    drop(reservation);
}

/// A detached result carries the receipt of the grant that made it, and a different grant's reservation must refuse it.
/// Adoption validates the receipt and the numeric envelope; the parent hold is released or kept by identity, never by
/// trusting a crossed grant.
#[test]
fn a_detached_result_cannot_cross_grant_identity() {
    let resources = parent_resources(16_384);
    let limits = grant_limits(64);
    let (reservation_a, budget_a) = resources.reserve_task_grant(limits).expect("grant a");
    let (reservation_b, _budget_b) = resources.reserve_task_grant(limits).expect("grant b");

    let child = budget_a.open_child().expect("open child");
    let mut child = child.bind(&CONTINUE, work()).expect("bind child");
    let mut output = TaskOutputBuffer::try_with_capacity(4, &mut child).expect("output buffer");
    output.try_append_within_capacity(b"data").expect("append");
    let detached = child.detach_result(output).expect("detach");

    let wrong_grant = reservation_b
        .adopt(detached)
        .err()
        .expect("grant identity mismatch must fail closed");
    assert_eq!(wrong_grant, ResourceError::AccountingInvariantViolation);

    drop(reservation_a);
}

/// The two ways a grant dies before it ever produces a result: the envelope cannot cover the child's own fixed ledger,
/// and the host cancels between opening the child and binding it. Both must refuse with their own typed error and leave
/// the reservation droppable — the coordinator half outlives the worker half by construction.
///
/// A retained partition below the fixed ledger still reserves against the parent (the envelope's aggregate is 24 bytes)
/// and refuses at `open_child`, which is where the child ledger is first built.
#[test]
fn a_grant_that_cannot_open_and_a_cancelled_bind_both_refuse_typed() {
    struct Cancel;
    impl Control for Cancel {
        fn check(&self) -> ControlOutcome {
            ControlOutcome::Cancelled
        }
    }

    let resources = parent_resources(4096);
    let impossible = TaskGrantLimits {
        retained_bytes: 0,
        working_bytes: 8,
        pending_io_bytes: 8,
        output_bytes: 8,
    };
    let (reservation, budget) = resources
        .reserve_task_grant(impossible)
        .expect("a 24-byte envelope fits the parent");
    let error = budget
        .open_child()
        .err()
        .expect("a retained partition below the fixed ledger cannot open");
    assert_eq!(
        error,
        ResourceError::LimitExceeded {
            limit_kind: ResourceLimit::MemoryBytes,
            limit: 0,
            current: 0,
            requested_delta: RequestAccount::minimum_memory_bytes(),
        }
    );
    drop(reservation);

    let (reservation, budget) = resources
        .reserve_task_grant(grant_limits(16))
        .expect("cancellation grant");
    let child = budget.open_child().expect("open child");
    let error = child
        .bind(&Cancel, work())
        .err()
        .expect("cancelled child must not bind");
    assert_eq!(error, ControlError::Cancelled);
    drop(reservation);
}

/// Cancellation observed after result allocation refuses subsequent control checks. Dropping the unpublished buffer,
/// child, then reservation completes without a double release.
#[test]
fn cancellation_after_allocation_refuses_and_still_tears_down() {
    use core::cell::Cell;

    struct ToggleCancel(Cell<bool>);
    impl Control for ToggleCancel {
        fn check(&self) -> ControlOutcome {
            if self.0.get() {
                ControlOutcome::Cancelled
            } else {
                ControlOutcome::Continue
            }
        }
    }

    let resources = parent_resources(8192);
    let (reservation, budget) = resources
        .reserve_task_grant(grant_limits(32))
        .expect("cancellation grant");
    let control = ToggleCancel(Cell::new(false));
    let child = budget.open_child().expect("open child");
    let mut child = child.bind(&control, work()).expect("bind child");
    let mut output = TaskOutputBuffer::try_with_capacity(32, &mut child).expect("output");
    control.0.set(true);
    assert_eq!(child.check_control(), Err(ControlError::Cancelled));
    assert_eq!(
        child.check_control(),
        Err(ControlError::Cancelled),
        "the publication check refuses too"
    );
    output
        .try_append_within_capacity(b"unpublished")
        .expect("the buffer itself is not control-gated");
    drop(output);
    drop(child);
    drop(reservation);
}

/// The parent-drop arm: a detached result and its reservation OUTLIVE the parent that issued them. Dropping the parent
/// first, then the buffer and the reservation, must release the buffer's allocation exactly once — the failure mode
/// this sequence guards against is a double-free on the later drops, which can only surface as a panic or abort (there
/// is no ledger left to observe once the parent is gone, so the test's assertion is that the sequence completes).
#[test]
fn detached_result_drops_cleanly_after_the_parent_is_gone() {
    let (reservation, detached) = {
        let resources = parent_resources(8192);
        let (reservation, budget) = resources
            .reserve_task_grant(grant_limits(32))
            .expect("parent-drop grant");
        let child = budget.open_child().expect("open child");
        let mut child = child.bind(&CONTINUE, work()).expect("bind child");
        let output = TaskOutputBuffer::try_with_capacity(32, &mut child).expect("output");
        let detached = child.detach_result(output).expect("detach");
        (reservation, detached)
    };
    drop(detached);
    drop(reservation);
}

/// Every order the grant state machine can be driven in — drop the budget, drop the detached result, or adopt it —
/// across initial capacities and growth targets. What each case pins is that the sequence completes and the adopted
/// bytes are exactly what the worker wrote; the ledger cannot be asserted here, because a plain test binary has no
/// counting allocator (the installed-allocator laws live in `tests/counting_alloc.rs` and `tests/worker_ambient.rs`).
#[test]
fn every_grant_teardown_order_completes_with_the_bytes_intact() {
    for case in 0_u8..64 {
        let resources = parent_resources(4096);
        let limits = grant_limits(32);
        let (reservation, budget) = resources
            .reserve_task_grant(limits)
            .expect("reserve state-machine grant");

        if case & 1 == 0 {
            drop(budget);
            drop(reservation);
        } else {
            let child = budget.open_child().expect("open child");
            let mut child = child.bind(&CONTINUE, work()).expect("bind child");
            let initial = usize::from((case >> 1) & 3);
            let target = usize::from((case >> 3) & 3) * 5;
            let mut output =
                TaskOutputBuffer::try_with_capacity(initial.max(target), &mut child).expect("output buffer");
            let written = &[case; 15][..target];
            output.try_append_within_capacity(written).expect("write output");
            let detached = child.detach_result(output).expect("detach");
            assert_eq!(detached.len(), target, "case {case}");

            if case & 32 == 0 {
                drop(detached);
                drop(reservation);
            } else {
                let adopted = reservation.adopt(detached).expect("adopt state-machine result");
                assert_eq!(
                    adopted.as_slice(),
                    written,
                    "adoption hands on the worker's own bytes, unchanged; case {case}"
                );
                drop(adopted);
            }
        }
    }
}

/// An output permit is authority, not publication: dropping it un-reserves every byte it held, so the next reservation
/// sees the full ceiling again and nothing was ever counted as published.
#[test]
fn dropping_an_output_permit_rolls_its_whole_reservation_back() {
    let limits = ResourceLimits::new(
        u64::MAX,
        64,
        RequestAccount::minimum_memory_bytes() + 4096,
        u64::MAX,
        64,
    );
    let resources =
        ResourceContext::new(RequestAccount::try_new(limits).expect("account"), &CONTINUE, work()).expect("resources");

    let permit = resources.reserve_output(64).expect("the whole ceiling");
    assert_eq!(permit.reserved_bytes(), 64);
    assert_eq!(resources.snapshot().output_reserved_bytes(), 64);
    resources
        .reserve_output(1)
        .expect_err("the ceiling counts outstanding reservations");

    drop(permit);
    assert_eq!(resources.snapshot().output_reserved_bytes(), 0);
    assert_eq!(
        resources.snapshot().output_bytes(),
        0,
        "a rolled-back permit published nothing"
    );
    let permit = resources
        .reserve_output(64)
        .expect("the rolled-back authority is spendable again");
    permit.commit(16).expect("commit the written prefix");
    assert_eq!(resources.snapshot().output_bytes(), 16);
    assert_eq!(
        resources.snapshot().output_reserved_bytes(),
        0,
        "commit releases the unwritten suffix"
    );
}

/// The permit's own boundary: committing more than was authorized is the caller lying about what it wrote, and it fails
/// with the permit's own error rather than the output-ceiling error — the ceiling was never consulted.
#[test]
fn committing_beyond_the_permit_refuses_with_the_permit_error() {
    let resources = parent_resources(4096);
    let permit = resources.reserve_output(64).expect("permit");
    let error = permit
        .commit(65)
        .expect_err("a permit cannot authorize more than it reserved");
    assert_eq!(
        error,
        ResourceError::OutputPermitExceeded {
            reserved: 64,
            published: 65,
        }
    );
    assert_eq!(
        resources.snapshot().output_bytes(),
        0,
        "a refused commit publishes nothing"
    );
}

/// The nesting ceiling is hard and the guards are what release it: entering past the configured depth refuses with the
/// depth dimension (levels, not bytes), and dropping a guard makes the level spendable again.
#[test]
fn entering_past_the_nesting_ceiling_refuses_and_a_dropped_guard_returns_it() {
    let limits = ResourceLimits::new(
        u64::MAX,
        u64::MAX,
        RequestAccount::minimum_memory_bytes() + 4096,
        u64::MAX,
        2,
    );
    let resources =
        ResourceContext::new(RequestAccount::try_new(limits).expect("account"), &CONTINUE, work()).expect("resources");

    let first = resources.enter_nesting().expect("level 1");
    let second = resources.enter_nesting().expect("level 2");
    assert_eq!(resources.snapshot().nesting_depth(), 2);
    let error = resources
        .enter_nesting()
        .expect_err("the third level crosses the ceiling");
    assert_eq!(
        error,
        ResourceError::LimitExceeded {
            limit_kind: ResourceLimit::NestingDepth,
            limit: 2,
            current: 2,
            requested_delta: 1,
        }
    );

    drop(second);
    assert_eq!(resources.snapshot().nesting_depth(), 1);
    let reentered = resources.enter_nesting().expect("a released level is spendable again");
    assert_eq!(resources.snapshot().nesting_peak(), 2);
    drop(reentered);
    drop(first);
    assert_eq!(resources.snapshot().nesting_depth(), 0);
}

/// Charge-through: the parent ceiling binds each envelope, and dropping the reservation returns the hold.
#[test]
fn reserve_task_grant_charges_the_parent_and_drop_releases() {
    let resources = parent_resources(8192);
    let baseline = resources.snapshot().memory_current_bytes();
    let limits = grant_limits(32);
    let total = limits.total_memory_bytes().expect("total");
    let (reservation, budget) = resources.reserve_task_grant(limits).expect("reserve");
    drop(budget);
    assert_eq!(
        resources.snapshot().memory_current_bytes(),
        baseline + total,
        "the envelope is held against the parent"
    );
    drop(reservation);
    assert_eq!(
        resources.snapshot().memory_current_bytes(),
        baseline,
        "dropping the reservation releases the hold"
    );
}

/// A parent that cannot afford the envelope refuses at reserve, so the caller can shrink the worker count instead of
/// dispatching.
#[test]
fn reserve_task_grant_refuses_when_the_parent_cannot_afford_the_envelope() {
    let resources = parent_resources(64);
    let huge = TaskGrantLimits {
        retained_bytes: RequestAccount::minimum_memory_bytes() + 128,
        working_bytes: 256,
        pending_io_bytes: 128,
        output_bytes: 10_000,
    };
    let error = resources
        .reserve_task_grant(huge)
        .err()
        .expect("the parent cannot afford this envelope");
    assert!(
        matches!(
            error,
            ResourceError::LimitExceeded {
                limit_kind: ResourceLimit::MemoryBytes,
                ..
            }
        ),
        "the refusal is the parent memory ceiling, got {error:?}"
    );
}
