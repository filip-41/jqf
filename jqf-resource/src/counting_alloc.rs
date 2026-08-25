//! The process allocator wrapper that counts every heap operation.
//!
//! Install this as `#[global_allocator]`. Each `alloc` / `realloc` / `dealloc` asks the thread's account first. Past
//! the ceiling it serves the emergency slab and trips the latch so the next work-slice pause can refuse with a typed
//! error. It returns null if the layout is larger than the slab, the slab is exhausted, or a sibling never claimed the
//! slab. The slab is a politeness buffer for the tripper's unwind; new growth after the trip is latched is not booked
//! on the real heap.

use std::alloc::{GlobalAlloc, Layout};

use crate::ambient;

/// Wraps any global allocator with per-request byte counting.
///
/// Install it in the binary that owns the link:
///
/// ```ignore
/// #[global_allocator] static GLOBAL: CountingAlloc<mimalloc::MiMalloc> = CountingAlloc(mimalloc::MiMalloc);
/// ```
pub struct CountingAlloc<A>(pub A);

// SAFETY: every method forwards to the wrapped allocator with the same
// pointer and layout it was given, and adds only ledger bookkeeping around it. The bookkeeping allocates nothing (the
// ambient charge path is latched against re-entry), so no method can recurse into itself.
unsafe impl<A: GlobalAlloc> GlobalAlloc for CountingAlloc<A> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if !ambient::charge_bytes(layout.size()) {
            return Self::serve_refused(layout, false);
        }
        // SAFETY: `layout` is the caller's, forwarded unchanged.
        let pointer = unsafe { self.0.alloc(layout) };
        if pointer.is_null() {
            ambient::release(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        if crate::slab::owns(pointer) {
            // A slab region was never charged and never handed to the wrapped allocator: freeing it is a no-op on both
            // ledgers.
            crate::slab::dealloc(pointer);
            return;
        }
        // SAFETY: `pointer`/`layout` are the caller's, forwarded unchanged.
        unsafe { self.0.dealloc(pointer, layout) };
        ambient::release(layout.size());
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // A slab region cannot be grown or shrunk in place and the wrapped allocator must never see it: move it to a
        // fresh slab region of the new size while it fits. Exhaustion is null — the trip is already latched and new
        // growth is not booked on the real heap. The old region stays (the bump never frees while live).
        if crate::slab::owns(pointer) {
            return crate::slab::realloc(pointer, layout, new_size);
        }
        if new_size > layout.size() && !ambient::charge_bytes(new_size - layout.size()) {
            return self.serve_refused_grow(pointer, layout, new_size);
        }
        // SAFETY: `pointer`/`layout`/`new_size` are the caller's, forwarded unchanged.
        let fresh = unsafe { self.0.realloc(pointer, layout, new_size) };
        if fresh.is_null() {
            if new_size > layout.size() {
                ambient::release(new_size - layout.size());
            }
        } else if new_size < layout.size() {
            ambient::release(layout.size() - new_size);
        }
        fresh
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if !ambient::charge_bytes(layout.size()) {
            return Self::serve_refused(layout, true);
        }
        // SAFETY: `layout` is the caller's, forwarded unchanged.
        let pointer = unsafe { self.0.alloc_zeroed(layout) };
        if pointer.is_null() {
            ambient::release(layout.size());
        }
        pointer
    }
}

impl<A: GlobalAlloc> CountingAlloc<A> {
    /// Serves a charge the ceiling refused: the first tripper takes the emergency slab, a layout larger than the slab
    /// or a slab miss is null. Shared by `alloc` and `alloc_zeroed`.
    fn serve_refused(layout: Layout, zeroed: bool) -> *mut u8 {
        if layout.size() > crate::slab::capacity() {
            return core::ptr::null_mut();
        }
        if !ambient::claim_emergency_slab() {
            return core::ptr::null_mut();
        }
        if zeroed {
            crate::slab::alloc_zeroed(layout)
        } else {
            crate::slab::alloc(layout)
        }
    }

    /// Grow half of the same path: the first tripper moves the old region onto the slab; exhaustion and siblings are
    /// null.
    fn serve_refused_grow(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if new_size > crate::slab::capacity() {
            return core::ptr::null_mut();
        }
        if !ambient::claim_emergency_slab() {
            return core::ptr::null_mut();
        }
        let fresh = crate::slab::realloc(pointer, layout, new_size);
        if !fresh.is_null() && !crate::slab::owns(pointer) {
            // The old heap block is no longer live. `dealloc` releases its original charge; the slab region was never
            // charged.
            // SAFETY: `pointer`/`layout` are the caller's pre-grow heap
            // allocation; the contents have been copied onto the slab.
            unsafe { self.dealloc(pointer, layout) };
        }
        fresh
    }
}

/// True if [`CountingAlloc`] is the process allocator.
///
/// If this is false, a memory ceiling cannot be enforced. Refuse the request instead of pretending the limit is on.
#[must_use]
pub fn ceiling_enforced() -> bool {
    // One-shot ledger probe: a Vec reservation moves `memory_current` only if this wrapper charged it. An
    // already-installed account is probed in place; otherwise a throwaway scope is installed so startup can still tell
    // the wrapper is in the link.
    fn current() -> u64 {
        crate::with_account(|account| account.snapshot().memory_current_bytes()).unwrap_or(0)
    }
    fn probe() -> bool {
        let before = current();
        let mut held: Vec<u8> = Vec::new();
        if held.try_reserve_exact(64).is_err() {
            return crate::tripped();
        }
        core::hint::black_box(held.as_ptr());
        let after = current();
        drop(held);
        if after > before {
            return true;
        }
        // A refused reservation is itself proof: only the counting allocator latches a trip, so a probe that cannot
        // move the counter but latched one is enforcing at a nearly-full ceiling.
        crate::tripped()
    }
    if crate::with_account(|_| ()).is_some() {
        return probe();
    }
    let Ok(account) = crate::RequestAccount::try_new(crate::ResourceLimits::new(
        u64::MAX,
        u64::MAX,
        u64::MAX,
        u64::MAX,
        u32::MAX,
    )) else {
        return false;
    };
    let _guard = crate::install(account);
    probe()
}
