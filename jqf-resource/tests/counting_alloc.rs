//! Counting-allocator binary: this target installs `CountingAlloc` as the process allocator. `tests/all.rs` cannot do
//! that — a grant test that does not install the wrapper would then silently charge nothing. `worker_ambient.rs` is
//! the same kind of binary (threads + the wrapper).

use std::cell::RefCell;
use std::sync::{Mutex, MutexGuard, PoisonError};

use jqf_resource::diag::{DiagnosticBuffer, DiagnosticRecord, DiagnosticSink, codes};
use jqf_resource::task::{TaskGrantLimits, TaskOutputBuffer};
use jqf_resource::{
    ContinueControl, CountingAlloc, MemoryCategory, RequestAccount, ResourceContext, ResourceError, ResourceLimit,
    ResourceLimits, WorkMeter, ceiling_enforced, install, tripped, with_account,
};

#[global_allocator]
static GLOBAL: CountingAlloc<std::alloc::System> = CountingAlloc(std::alloc::System);

#[derive(Default)]
struct RetainingSink {
    payloads: RefCell<Vec<String>>,
}

impl DiagnosticSink for RetainingSink {
    fn record(&self, record: DiagnosticRecord<'_>) {
        self.payloads
            .borrow_mut()
            .push(record.payload.unwrap_or_default().to_owned());
    }
}

fn account(memory: u64) -> RequestAccount {
    RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, memory, u64::MAX, u32::MAX)).expect("account")
}

/// The emergency slab and live-scope count are process-wide. Parallel tests that install a scope keep the count above
/// zero, so a tripper's drop would skip the rewind and the next infallible push would abort. One lock for the binary.
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

fn fill_ceiling(bytes: u64) -> Vec<u8> {
    let current = with_account(RequestAccount::snapshot)
        .expect("in scope")
        .memory_current_bytes();
    let spendable = usize::try_from(bytes - current).expect("headroom fits in usize");
    Vec::with_capacity(spendable)
}

#[test]
fn the_installed_allocator_reports_itself() {
    let _serial = serial();
    assert!(ceiling_enforced());
}

/// `ceiling_enforced` answers from a latch an EARLIER allocation may already have set, so the answer is only worth
/// anything while its own probe allocation actually reaches the allocator. An optimizer that proves the probe buffer
/// unused deletes it (the elision is an optimized-build effect, so this test's teeth are in a release profile), and the
/// function then reports the link it never tested. The ledger is where the probe becomes observable: on a fresh thread
/// inside a fresh scope nothing else allocates between the two snapshots, so the peak moves only if the probe ran.
#[test]
fn the_enforcement_probe_reaches_the_allocator() {
    let _serial = serial();
    std::thread::spawn(|| {
        let _guard = install(account(u64::MAX));
        {
            let before = jqf_resource::with_account(RequestAccount::snapshot).expect("scope");
            assert!(ceiling_enforced());
            let after = jqf_resource::with_account(RequestAccount::snapshot).expect("scope");
            assert!(
                after.memory_peak_bytes() > before.memory_peak_bytes(),
                "an elided probe latches nothing and charges nothing"
            );
        }
    })
    .join()
    .expect("probe thread joins");
}

#[test]
fn allocation_inside_a_scope_is_counted() {
    let _serial = serial();
    let _guard = install(account(u64::MAX));
    {
        let before = jqf_resource::with_account(RequestAccount::snapshot).expect("scope");
        let held: Vec<u8> = Vec::with_capacity(64 * 1024);
        let after = jqf_resource::with_account(RequestAccount::snapshot).expect("scope");
        assert!(
            after.memory_current_bytes() >= before.memory_current_bytes() + 64 * 1024,
            "a 64 KiB reservation must appear in the ledger"
        );
        drop(held);
    }
}

#[test]
fn deallocation_returns_the_bytes() {
    let _serial = serial();
    let _guard = install(account(u64::MAX));
    {
        let baseline = jqf_resource::with_account(RequestAccount::snapshot)
            .expect("scope")
            .memory_current_bytes();
        {
            let _held: Vec<u8> = Vec::with_capacity(64 * 1024);
        }
        let settled = jqf_resource::with_account(RequestAccount::snapshot)
            .expect("scope")
            .memory_current_bytes();
        assert_eq!(settled, baseline, "drop must release exactly what alloc charged");
    }
}

#[test]
fn retained_diagnostics_use_the_diagnostic_category() {
    static CONTROL: ContinueControl = ContinueControl;
    let _serial = serial();
    let payload = "x".repeat(4096);
    let _guard = install(account(u64::MAX));
    {
        let diagnostics = DiagnosticBuffer::with_cap(1);
        let shared = with_account(RequestAccount::try_share).expect("scope").expect("share");
        let resources = ResourceContext::new(shared, &CONTROL, WorkMeter::try_new_v1(4096).expect("work"))
            .expect("resources")
            .with_diagnostics(&diagnostics);
        let before = resources.snapshot();
        let mut record = DiagnosticRecord::new_registered(codes::RAISE_ITERATE);
        record.payload = Some(&payload);
        resources.record_diagnostic(record);
        let after = resources.snapshot();
        assert_eq!(
            after.memory(MemoryCategory::Working).current(),
            before.memory(MemoryCategory::Working).current()
        );
        assert!(
            after.memory(MemoryCategory::Diagnostic).current() > before.memory(MemoryCategory::Diagnostic).current(),
            "retained sink storage is diagnostic residency"
        );
        diagnostics.clear();
        let settled = resources.snapshot();
        assert_eq!(
            settled.memory(MemoryCategory::Working).current(),
            before.memory(MemoryCategory::Working).current()
        );
        assert!(
            settled.memory(MemoryCategory::Diagnostic).current() < after.memory(MemoryCategory::Diagnostic).current(),
            "clear drops record payloads while retaining reusable ring capacity"
        );
        let mut record = DiagnosticRecord::new_registered(codes::RAISE_ITERATE);
        record.payload = Some(&payload);
        resources.record_diagnostic(record);
        drop(resources);
        drop(diagnostics);
        let dropped = with_account(RequestAccount::snapshot).expect("scope");
        assert_eq!(
            dropped.memory(MemoryCategory::Working).current(),
            before.memory(MemoryCategory::Working).current()
        );
        assert_eq!(
            dropped.memory(MemoryCategory::Diagnostic).current(),
            before.memory(MemoryCategory::Diagnostic).current()
        );
    }
}

#[test]
fn replacing_a_full_diagnostic_ring_reuses_the_evicted_payload_headroom() {
    let _serial = serial();
    let limit = RequestAccount::minimum_memory_bytes() + 16 * 1024;
    let payload = "x".repeat(4096);
    let _guard = install(account(limit));
    {
        let diagnostics = DiagnosticBuffer::with_cap(1);
        let mut first = DiagnosticRecord::new_registered(codes::RAISE_ITERATE);
        first.payload = Some(&payload);
        diagnostics.record(first);
        let _ceiling = fill_ceiling(limit);
        let before = with_account(RequestAccount::snapshot).expect("scope");
        assert_eq!(before.memory_current_bytes(), limit);

        let mut replacement = DiagnosticRecord::new_registered(codes::RAISE_INDEX);
        replacement.payload = Some(&payload);
        diagnostics.record(replacement);

        assert!(!tripped(), "the evicted payload funds its replacement");
        let after = with_account(RequestAccount::snapshot).expect("scope");
        assert_eq!(after.memory_current_bytes(), before.memory_current_bytes());
        assert_eq!(diagnostics.records()[0].code, codes::RAISE_INDEX);
    }
}

#[test]
fn custom_diagnostic_sinks_keep_their_own_allocation_category() {
    static CONTROL: ContinueControl = ContinueControl;
    let _serial = serial();
    let _guard = install(account(u64::MAX));
    let payload = "x".repeat(2048);
    let before = with_account(RequestAccount::snapshot).expect("scope");
    let sink = RetainingSink::default();
    let resources = ResourceContext::new(
        with_account(RequestAccount::try_share).expect("scope").expect("share"),
        &CONTROL,
        WorkMeter::try_new_v1(4096).expect("work"),
    )
    .expect("context")
    .with_diagnostics(&sink);
    let mut record = DiagnosticRecord::new_registered(codes::RAISE_ITERATE);
    record.payload = Some(&payload);
    resources.record_diagnostic(record);
    let held = with_account(RequestAccount::snapshot).expect("scope");
    assert_eq!(
        held.memory(MemoryCategory::Diagnostic).current(),
        before.memory(MemoryCategory::Diagnostic).current()
    );
    assert!(held.memory(MemoryCategory::Working).current() > before.memory(MemoryCategory::Working).current());
    drop(resources);
    drop(sink);
    let settled = with_account(RequestAccount::snapshot).expect("scope");
    assert_eq!(
        settled.memory(MemoryCategory::Working).current(),
        before.memory(MemoryCategory::Working).current()
    );
    assert_eq!(
        settled.memory(MemoryCategory::Diagnostic).current(),
        before.memory(MemoryCategory::Diagnostic).current()
    );
}

#[test]
fn detach_result_refuses_a_tripped_child_ledger() {
    static CONTROL: ContinueControl = ContinueControl;
    let _serial = serial();
    let parent =
        ResourceContext::new(account(u64::MAX), &CONTROL, WorkMeter::try_new_v1(4096).expect("work")).expect("parent");
    let limits = TaskGrantLimits {
        retained_bytes: RequestAccount::minimum_memory_bytes() + 128,
        working_bytes: 256,
        pending_io_bytes: 128,
        output_bytes: 32,
    };
    let (reservation, budget) = parent.reserve_task_grant(limits).expect("reserve");
    let child = budget.open_child().expect("child");
    let _ambient = child.install_ambient().expect("ambient");
    let mut child = child
        .bind(&CONTROL, WorkMeter::try_new_v1(4096).expect("work"))
        .expect("bind");
    let output = TaskOutputBuffer::try_with_capacity(0, &mut child).expect("output");
    let mut allocation = Vec::<u8>::new();
    assert!(allocation.try_reserve(2 * 1024 * 1024).is_err());
    let error = child.detach_result(output).err().expect("tripped child cannot publish");
    assert!(matches!(
        error,
        ResourceError::LimitExceeded {
            limit_kind: ResourceLimit::MemoryBytes,
            ..
        }
    ));
    drop(reservation);
}

#[test]
fn detach_result_ignores_an_unrelated_account_trip() {
    static CONTROL: ContinueControl = ContinueControl;
    let _serial = serial();
    let parent =
        ResourceContext::new(account(u64::MAX), &CONTROL, WorkMeter::try_new_v1(4096).expect("work")).expect("parent");
    let limits = TaskGrantLimits {
        retained_bytes: RequestAccount::minimum_memory_bytes() + 128,
        working_bytes: 256,
        pending_io_bytes: 128,
        output_bytes: 32,
    };
    let (reservation, budget) = parent.reserve_task_grant(limits).expect("reserve");
    let child = budget.open_child().expect("child");
    let mut child = child
        .bind(&CONTROL, WorkMeter::try_new_v1(4096).expect("work"))
        .expect("bind");
    let output = TaskOutputBuffer::try_with_capacity(0, &mut child).expect("output");
    let unrelated = account(RequestAccount::minimum_memory_bytes());
    let _ambient = install(unrelated);
    let mut allocation = Vec::<u8>::new();
    assert!(allocation.try_reserve(2 * 1024 * 1024).is_err());
    let detached = child
        .detach_result(output)
        .expect("unrelated trip does not poison child");
    drop(detached);
    drop(reservation);
}

#[test]
fn try_reserve_gets_a_clean_error_at_the_ceiling() {
    let _serial = serial();
    let _guard = install(account(1 << 20));
    {
        let mut buffer: Vec<u8> = Vec::new();
        let outcome = buffer.try_reserve(64 << 20);
        assert!(
            outcome.is_err(),
            "a 64 MiB reservation under a 1 MiB ceiling must fail, not abort"
        );
    }
}

#[test]
fn allocation_outside_a_scope_is_unmetered() {
    let _serial = serial();
    let held: Vec<u8> = Vec::with_capacity(1 << 20);
    assert_eq!(held.capacity(), 1 << 20, "no scope means no ceiling");
}

/// An infallible allocation past the ceiling uses the emergency slab so the next cooperative check can surface the
/// typed refusal.
#[test]
#[allow(
    clippy::vec_init_then_push,
    reason = "the push is the point: an INFALLIBLE allocation past the ceiling is exactly the path that used to abort"
)]
fn a_refused_infallible_allocation_is_served_from_the_slab() {
    let _serial = serial();
    let _guard = install(account(4096));
    {
        let _ceiling = fill_ceiling(4096);
        // `Vec::push` exercises the infallible allocation path.
        let mut buffer: Vec<u8> = Vec::new();
        buffer.push(7);
        assert_eq!(buffer[0], 7, "the slab region is usable");
        assert!(jqf_resource::tripped(), "the refusal latched the trip");
        assert!(
            jqf_resource::cooperative_refusal().is_err(),
            "the next cooperative check surfaces the typed refusal"
        );
        drop(buffer);
    }
}

/// The realloc half of the same regression: a `Vec` growing past the ceiling reallocates infallibly, and the refused
/// growth must move to the slab with its contents preserved (never a null, never an abort).
#[test]
fn a_refused_infallible_realloc_preserves_contents_from_the_slab() {
    let _serial = serial();
    let _guard = install(account(4096));
    {
        let _ceiling = fill_ceiling(4096);
        let mut buffer: Vec<u64> = Vec::new();
        for index in 0..64u64 {
            buffer.push(index);
        }
        for (index, &value) in buffer.iter().enumerate() {
            assert_eq!(value, index as u64, "contents survive slab realloc");
        }
        assert!(jqf_resource::tripped(), "the refusal latched the trip");
        assert!(jqf_resource::cooperative_refusal().is_err());
        drop(buffer);
    }
}

/// Two threads trip via a fallible reservation after filling the ceiling. The first claims the 1 MiB slab; the sibling
/// gets null (typed `try_reserve` failure), never a booked real-heap block.
#[test]
fn a_sibling_fallible_allocation_does_not_consume_the_slab() {
    use std::sync::{Arc, Barrier};
    let _serial = serial();
    let barrier = Arc::new(Barrier::new(2));
    let run = |barrier: Arc<Barrier>| -> bool {
        let _guard = install(account(4096));
        {
            let _ceiling = fill_ceiling(4096);
            barrier.wait();
            let mut buffer = Vec::<u8>::new();
            let served = buffer.try_reserve(600 * 1024).is_ok();
            if served {
                buffer.resize(600 * 1024, 0);
                assert_eq!(buffer.len(), 600 * 1024, "the zeroed region is usable");
            }
            assert!(jqf_resource::tripped(), "the refusal latched the trip");
            served
        }
    };
    let first = {
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || run(barrier))
    };
    let second = std::thread::spawn(move || run(barrier));
    let first_served = first.join().expect("first thread joins without abort");
    let second_served = second.join().expect("second thread joins without abort");
    assert_ne!(
        first_served, second_served,
        "exactly one tripper owns the slab; the sibling is null"
    );
    assert!(first_served || second_served, "the owner was served");
}

/// A slab-served refusal is never charged, so drop must not steal Working.
#[test]
fn a_slab_served_allocation_balances_on_drop() {
    let _serial = serial();
    let _guard = install(account(4096));
    {
        let _ceiling = fill_ceiling(4096);
        let before = with_account(RequestAccount::snapshot)
            .expect("in scope")
            .memory_current_bytes();
        {
            let buffer = vec![0u8; 64 * 1024];
            assert_eq!(buffer.len(), 64 * 1024);
            assert!(jqf_resource::tripped());
        }
        let after = with_account(RequestAccount::snapshot)
            .expect("in scope")
            .memory_current_bytes();
        assert_eq!(
            after, before,
            "drop of a refused-then-served allocation must not steal Working"
        );
    }
}

/// Two sequential in-process accounts must not inherit a consumed bump: the first request fills the slab, and the next
/// starts at offset 0.
#[test]
fn sequential_accounts_do_not_inherit_a_full_slab() {
    let _serial = serial();
    {
        let _guard = install(account(4096));
        let _ceiling = fill_ceiling(4096);
        let mut buffers = Vec::new();
        loop {
            let mut buffer = Vec::<u8>::new();
            if buffer.try_reserve(256 * 1024).is_err() {
                break;
            }
            buffer.push(1);
            buffers.push(buffer);
        }
        assert!(!buffers.is_empty(), "the first request was served from the slab");
        assert!(jqf_resource::tripped());
        drop(buffers);
    }
    {
        let _guard = install(account(4096));
        let _ceiling = fill_ceiling(4096);
        let mut buffer = Vec::<u8>::new();
        assert!(
            buffer.try_reserve(512 * 1024).is_ok(),
            "a later request starts at offset 0"
        );
        buffer.push(1);
        assert_eq!(buffer[0], 1);
        assert!(jqf_resource::tripped());
    }
}

#[test]
fn a_live_slab_block_delays_reuse_until_after_deallocation() {
    let _serial = serial();
    let (first_pointer, live) = {
        let _guard = install(account(4096));
        let _ceiling = fill_ceiling(4096);
        let mut buffer = Vec::<u8>::new();
        buffer.try_reserve(512 * 1024).expect("first slab allocation");
        buffer.push(7);
        (buffer.as_ptr() as usize, buffer)
    };
    assert_eq!(live[0], 7, "the block remains valid after its scope");

    {
        let _guard = install(account(4096));
        let _ceiling = fill_ceiling(4096);
        let mut blocked = Vec::<u8>::new();
        assert!(
            blocked.try_reserve(512 * 1024).is_err(),
            "a new scope cannot reclaim a live slab block"
        );
        assert_eq!(live[0], 7, "the failed reclaim did not overwrite the live block");
    }

    drop(live);
    {
        let _guard = install(account(4096));
        let _ceiling = fill_ceiling(4096);
        let mut reused = Vec::<u8>::new();
        reused.try_reserve(512 * 1024).expect("slab reusable after free");
        reused.push(9);
        assert_eq!(reused.as_ptr() as usize, first_pointer);
        assert_eq!(reused[0], 9);
    }
}

/// Adopt+drop of a worker result on the parent must not steal the parent's Working holds. The Vec was charged on the
/// child ledger; parent Drop of it is a no-op on the ambient ledger, and only the grant hold is released.
#[test]
fn adopted_result_drop_does_not_steal_parent_working() {
    static CONTROL: ContinueControl = ContinueControl;
    let _serial = serial();
    let _guard = install(account(u64::MAX));
    {
        let shared = with_account(RequestAccount::try_share).expect("scope").expect("share");
        let resources =
            ResourceContext::new(shared, &CONTROL, WorkMeter::try_new_v1(4096).expect("work")).expect("parent context");
        let baseline = with_account(RequestAccount::snapshot)
            .expect("scope")
            .memory(MemoryCategory::Working)
            .current();
        let limits = TaskGrantLimits {
            retained_bytes: RequestAccount::minimum_memory_bytes() + 128,
            working_bytes: 256,
            pending_io_bytes: 128,
            output_bytes: 64,
        };
        let (reservation, budget) = resources.reserve_task_grant(limits).expect("reserve");
        let after_reserve = with_account(RequestAccount::snapshot)
            .expect("scope")
            .memory(MemoryCategory::Working)
            .current();
        assert!(after_reserve > baseline, "the grant hold is Working");
        let child = budget.open_child().expect("child");
        let after_open = with_account(RequestAccount::snapshot)
            .expect("scope")
            .memory(MemoryCategory::Working)
            .current();
        assert_eq!(
            after_open, after_reserve,
            "opening a child must not charge the parent's Working for the child ledger"
        );
        let child_ambient = child.install_ambient().expect("child ambient");
        let mut child = child
            .bind(&CONTROL, WorkMeter::try_new_v1(4096).expect("work"))
            .expect("bind");
        let mut output = TaskOutputBuffer::try_with_capacity(64, &mut child).expect("output");
        output.try_append_within_capacity(b"adopt-drop").expect("write");
        let detached = child.detach_result(output).expect("detach");
        drop(child_ambient);
        let adopted = reservation.adopt(detached).expect("adopt");
        drop(adopted);
        let settled = with_account(RequestAccount::snapshot)
            .expect("scope")
            .memory(MemoryCategory::Working)
            .current();
        assert_eq!(settled, baseline, "adopt+drop must release only the grant hold");
    }
}

/// The result's ambient charge belongs to the ledger bound to the worker, even if the dereferenced context is replaced
/// before detachment. Seeded parent `PendingIo` makes an erroneous release against the restored ambient account
/// observable.
#[test]
fn detached_result_uses_the_bind_time_child_identity_after_context_swap() {
    static CONTROL: ContinueControl = ContinueControl;
    let _serial = serial();
    let _guard = install(account(u64::MAX));
    {
        let shared = with_account(RequestAccount::try_share).expect("scope").expect("share");
        let resources =
            ResourceContext::new(shared, &CONTROL, WorkMeter::try_new_v1(4096).expect("work")).expect("parent context");
        resources
            .account()
            .charge_residency(64, MemoryCategory::PendingIo)
            .expect("seed parent pending io");
        let baseline = with_account(RequestAccount::snapshot)
            .expect("scope")
            .memory(MemoryCategory::PendingIo)
            .current();
        let limits = TaskGrantLimits {
            retained_bytes: RequestAccount::minimum_memory_bytes() + 128,
            working_bytes: 256,
            pending_io_bytes: 128,
            output_bytes: 64,
        };
        let (reservation, budget) = resources.reserve_task_grant(limits).expect("reserve");
        let child = budget.open_child().expect("child");
        let child_ambient = child.install_ambient().expect("child ambient");
        let mut child = child
            .bind(&CONTROL, WorkMeter::try_new_v1(4096).expect("work"))
            .expect("bind");
        let foreign = ResourceContext::new(
            account(u64::MAX),
            &CONTROL,
            WorkMeter::try_new_v1(4096).expect("foreign work"),
        )
        .expect("foreign context");
        let original = core::mem::replace(&mut *child, foreign);
        let mut output = TaskOutputBuffer::try_with_capacity(32, &mut child).expect("output");
        output.try_append_within_capacity(b"bind-time-identity").expect("write");
        let detached = child.detach_result(output).expect("detach");
        drop(child_ambient);
        let adopted = reservation.adopt(detached).expect("adopt");
        drop(adopted);
        let settled = with_account(RequestAccount::snapshot)
            .expect("scope")
            .memory(MemoryCategory::PendingIo)
            .current();
        assert_eq!(
            settled, baseline,
            "dropping the adopted result must not release parent PendingIo"
        );
        drop(original);
        resources.account().release_residency(MemoryCategory::PendingIo, 64);
    }
}

/// The not-transferred arm: no child ambient is installed, so the result buffer's charge lands on the ledger that IS
/// ambient (here the parent's own). The parent-side drops must release that `PendingIo` charge instead of suppressing
/// it — a suppressed release would leak it for the life of the request. Both terminal owners are exercised: the
/// adopted result and a detached result that is never adopted.
#[test]
fn a_result_charged_on_the_parent_releases_pending_io_on_drop() {
    static CONTROL: ContinueControl = ContinueControl;
    let _serial = serial();
    let _guard = install(account(u64::MAX));
    {
        let shared = with_account(RequestAccount::try_share).expect("scope").expect("share");
        let resources =
            ResourceContext::new(shared, &CONTROL, WorkMeter::try_new_v1(4096).expect("work")).expect("parent context");
        let limits = TaskGrantLimits {
            retained_bytes: RequestAccount::minimum_memory_bytes() + 128,
            working_bytes: 256,
            pending_io_bytes: 128,
            output_bytes: 64,
        };
        let baseline = with_account(RequestAccount::snapshot)
            .expect("scope")
            .memory(MemoryCategory::PendingIo)
            .current();

        let (reservation, budget) = resources.reserve_task_grant(limits).expect("reserve");
        let child = budget.open_child().expect("child");
        let mut child = child
            .bind(&CONTROL, WorkMeter::try_new_v1(4096).expect("work"))
            .expect("bind");
        let mut output = TaskOutputBuffer::try_with_capacity(64, &mut child).expect("output");
        output.try_append_within_capacity(b"parent-charged").expect("write");
        let detached = child.detach_result(output).expect("detach");
        let adopted = reservation.adopt(detached).expect("adopt");
        drop(adopted);
        let settled = with_account(RequestAccount::snapshot)
            .expect("scope")
            .memory(MemoryCategory::PendingIo)
            .current();
        assert_eq!(
            settled, baseline,
            "an adopted result charged on the parent must return its PendingIo"
        );

        let (reservation, budget) = resources.reserve_task_grant(limits).expect("reserve");
        let child = budget.open_child().expect("child");
        let mut child = child
            .bind(&CONTROL, WorkMeter::try_new_v1(4096).expect("work"))
            .expect("bind");
        let output = TaskOutputBuffer::try_with_capacity(64, &mut child).expect("output");
        let detached = child.detach_result(output).expect("detach");
        drop(detached);
        drop(reservation);
        let settled = with_account(RequestAccount::snapshot)
            .expect("scope")
            .memory(MemoryCategory::PendingIo)
            .current();
        assert_eq!(
            settled, baseline,
            "a dropped detached result charged on the parent must return its PendingIo"
        );
    }
}
