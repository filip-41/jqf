//! Cross-account deallocation behavior through the counting allocator.
//!
//! `GlobalAlloc::dealloc` carries no allocation origin. Ordinary storage transferred across ambient accounts therefore
//! leaves a phantom charge on its origin and clamps its release against the landing account. Task result buffers carry
//! the separate release policy used by supported transfers.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use jqf_resource::{
    CountingAlloc, MemoryCategory, RequestAccount, ResourceLimits, ceiling_enforced, install, warm_ambient_mutex,
    with_account,
};

#[global_allocator]
static GLOBAL: CountingAlloc<std::alloc::System> = CountingAlloc(std::alloc::System);

const ORIGIN_BYTES: usize = 64 * 1024;
const LANDING_BYTES: usize = 32 * 1024;
const READY: u8 = 1;
const RELEASED: u8 = 2;

fn account() -> RequestAccount {
    RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, 1 << 30, u64::MAX, u32::MAX)).expect("account")
}

fn working_current() -> u64 {
    with_account(RequestAccount::snapshot)
        .expect("ambient account")
        .memory(MemoryCategory::Working)
        .current()
}

#[test]
fn a_foreign_free_is_bounded_by_the_landing_account() {
    assert!(ceiling_enforced());
    warm_ambient_mutex();
    let allocation = Arc::new(Mutex::new(None));
    drop(allocation.lock().unwrap_or_else(PoisonError::into_inner));
    let step = Arc::new(AtomicU8::new(0));
    let worker_allocation = Arc::clone(&allocation);
    let worker_step = Arc::clone(&step);
    let worker = std::thread::spawn(move || {
        let _guard = install(account());
        let baseline = working_current();
        let allocation = Vec::<u8>::with_capacity(ORIGIN_BYTES);
        let charged = working_current();
        assert!(charged >= baseline + ORIGIN_BYTES as u64);
        *worker_allocation.lock().unwrap_or_else(PoisonError::into_inner) = Some(allocation);
        worker_step.store(READY, Ordering::Release);
        while worker_step.load(Ordering::Acquire) != RELEASED {
            std::hint::spin_loop();
        }
        assert_eq!(
            working_current(),
            charged,
            "a foreign free cannot reach the origin ledger"
        );
    });

    while step.load(Ordering::Acquire) != READY {
        std::hint::spin_loop();
    }
    {
        let guard = install(account());
        let landing = Vec::<u8>::with_capacity(LANDING_BYTES);
        let before = working_current();
        let foreign = allocation
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .expect("foreign allocation");
        let foreign_capacity = foreign.capacity() as u64;
        drop(foreign);
        let after = working_current();
        assert!(after <= before);
        assert!(before - after <= foreign_capacity);
        step.store(RELEASED, Ordering::Release);
        drop(landing);
        drop(guard);
    }
    worker.join().expect("worker joins");
}
