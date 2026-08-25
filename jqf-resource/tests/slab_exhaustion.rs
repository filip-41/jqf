//! Slab-exhaustion binary: once the owner has consumed the emergency slab, further growth is a typed `try_reserve`
//! failure (null), never a booked real-heap fall-through and never an abort. Its own target: the slab claim is
//! process-wide while a scope is live.

use std::sync::{Mutex, MutexGuard, PoisonError};

use jqf_resource::{CountingAlloc, RequestAccount, ResourceLimits, ceiling_enforced, install, with_account};

#[global_allocator]
static GLOBAL: CountingAlloc<std::alloc::System> = CountingAlloc(std::alloc::System);

fn account(memory: u64) -> RequestAccount {
    RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, memory, u64::MAX, u32::MAX)).expect("account")
}

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
fn the_slab_owner_gets_typed_refusal_once_the_slab_is_exhausted() {
    let _serial = serial();
    let _guard = install(account(4096));
    {
        let _ceiling = fill_ceiling(4096);
        let before = with_account(RequestAccount::snapshot)
            .expect("in scope")
            .memory_current_bytes();
        let mut served = 0usize;
        let mut refused = 0usize;
        let mut buffers = Vec::new();
        for _ in 0..4 {
            let mut buffer = Vec::<u8>::new();
            match buffer.try_reserve(512 * 1024) {
                Ok(()) => {
                    buffer.push(1);
                    buffers.push(buffer);
                    served += 1;
                }
                Err(_) => refused += 1,
            }
        }
        assert!(served >= 1, "the owner was served from the slab");
        assert!(
            refused >= 1,
            "exhaustion is null: typed try_reserve failure, not a heap fall-through"
        );
        assert!(jqf_resource::tripped(), "the refusal latched the trip");
        let mut grower = Vec::<u8>::new();
        if grower.try_reserve(32).is_ok() {
            grower.extend_from_slice(&[1, 2, 3]);
        }
        assert!(
            grower.try_reserve(2 * 1024 * 1024).is_err(),
            "a grow past the slab is null, not booked heap"
        );
        drop(buffers);
        drop(grower);
        let after = with_account(RequestAccount::snapshot)
            .expect("in scope")
            .memory_current_bytes();
        assert_eq!(
            after, before,
            "refused growth was not booked, so drop does not steal Working"
        );
    }
}

#[test]
fn the_allocator_probe_is_fallible_after_slab_exhaustion() {
    let _serial = serial();
    let mut buffers = Vec::with_capacity(20_000);
    let _guard = install(account(4096));
    let _ceiling = fill_ceiling(4096);
    for size in [256 * 1024, 64] {
        loop {
            let mut buffer = Vec::<u8>::new();
            if buffer.try_reserve(size).is_err() {
                break;
            }
            buffers.push(buffer);
        }
    }
    assert!(jqf_resource::tripped());
    assert!(ceiling_enforced());
    drop(buffers);
}
