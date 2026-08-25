//! One megabyte of static memory for allocations that just tripped the ceiling.
//!
//! A refused `Vec::push` would otherwise abort. The first thread that trips owns this bump; later threads get null.
//! Regions are never freed while live: the slab exists so a request that tripped the ceiling can unwind politely. Once
//! no ambient scope or slab-backed allocation remains, the bump rewinds at scope teardown or the next outermost
//! install. Fresh BSS is zeros. [`reset`] only rewinds the bump; [`alloc_zeroed`] zeros a new region. Exhaustion is a
//! null.

use std::alloc::Layout;
use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The slab size. BSS, so it costs no heap and no RSS until a refusal touches it.
const SLAB_BYTES: usize = 1 << 20;

/// Bytes the emergency slab can cover. A refused layout larger than this is a true allocation failure (null), never a
/// real-heap bypass.
pub(crate) const fn capacity() -> usize {
    SLAB_BYTES
}

/// The slab. Wrapped for interior mutability: a plain `static [u8; N]` is immutable and lives in read-only memory,
/// where a bump write SIGBUSes. The `unsafe impl Sync` is sound because every access is a raw-pointer bump guarded by
/// the atomic [`BUMP`] — the bytes are never read through a shared reference.
struct Slab(UnsafeCell<[u8; SLAB_BYTES]>);

// SAFETY: the only access to the slab is the atomic-guarded raw-pointer bump
// in [`alloc`]; no reference to the bytes ever escapes, so shared access is sound.
unsafe impl Sync for Slab {}

static SLAB: Slab = Slab(UnsafeCell::new([0; SLAB_BYTES]));
/// The next free byte, as an offset from [`SLAB`]'s base.
static BUMP: AtomicUsize = AtomicUsize::new(0);
/// Logical allocations still owned by callers.
static LIVE_BLOCKS: AtomicUsize = AtomicUsize::new(0);

/// Bump-allocates `layout` from the slab, or returns null when the slab cannot fit it.
///
/// The returned pointer satisfies the layout's size and alignment. It is not zeroed: after a rewind the prefix may
/// still hold the previous generation. [`alloc_zeroed`] zeros. The allocation never recurses into the ledger: it is a
/// static array bump and nothing else.
pub(crate) fn alloc(layout: Layout) -> *mut u8 {
    let size = layout.size();
    let align = layout.align();
    // A zero-size charge cannot cross a ceiling (`current + 0 > limit` is impossible while refusals keep `current` at
    // or under it), so this path never sees size 0. A layout longer than the slab can never fit, and neither can one
    // aligned more strictly than the slab is long — nothing bounds a `Layout`'s alignment by its size, so the
    // alignment is refused explicitly here rather than assumed away below. All three are the null fallback.
    if size == 0 || size > SLAB_BYTES || align > SLAB_BYTES {
        return core::ptr::null_mut();
    }
    // Align against the ABSOLUTE address: the slab base is only word-aligned, so offset alignment would misalign the
    // result for any layout aligned beyond the base. `align` is a power of two, so the mask rounds up; the two
    // additions are checked because the operands are addresses, and an address arithmetic that cannot be completed is a
    // layout that could never have fit anyway.
    let base = SLAB.0.get() as usize;
    loop {
        let offset = BUMP.load(Ordering::Relaxed);
        let start = base + offset;
        let Some(aligned) = start.checked_add(align - 1).map(|raised| raised & !(align - 1)) else {
            return core::ptr::null_mut();
        };
        let Some(end) = aligned.checked_add(size) else {
            return core::ptr::null_mut();
        };
        if end > base + SLAB_BYTES {
            return core::ptr::null_mut();
        }
        let new_offset = aligned - base;
        if BUMP
            .compare_exchange_weak(offset, new_offset + size, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            LIVE_BLOCKS.fetch_add(1, Ordering::Release);
            return aligned as *mut u8;
        }
    }
}

/// Same as [`alloc`], then zeros the region.
pub(crate) fn alloc_zeroed(layout: Layout) -> *mut u8 {
    let pointer = alloc(layout);
    if !pointer.is_null() {
        // SAFETY: `alloc` returned a unique region of `layout.size()` bytes.
        unsafe { pointer.write_bytes(0, layout.size()) };
    }
    pointer
}

/// Whether `pointer` was bump-allocated from the slab.
///
/// `dealloc`/`realloc` must recognize slab regions so they never forward a slab pointer to the wrapped allocator (which
/// would free memory it does not own) and never release ledger bytes the slab never charged.
pub(crate) fn owns(pointer: *mut u8) -> bool {
    let address = pointer as usize;
    let base = SLAB.0.get() as usize;
    address >= base && address < base + SLAB_BYTES
}

pub(crate) fn dealloc(pointer: *mut u8) {
    debug_assert!(owns(pointer), "deallocating a non-slab pointer");
    let previous = LIVE_BLOCKS.fetch_sub(1, Ordering::AcqRel);
    debug_assert!(previous > 0, "slab block deallocated twice");
}

/// Moves the region at `pointer` to a fresh bump region of `new_size`, preserving `min(layout.size(), new_size)` bytes,
/// and returns the new pointer (or null when the slab cannot fit it).
///
/// An old slab region remains in the bump; the caller frees a transferred real-heap source after this copy succeeds.
pub(crate) fn realloc(pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    let Ok(new_layout) = Layout::from_size_align(new_size, layout.align()) else {
        return core::ptr::null_mut();
    };
    let fresh = alloc(new_layout);
    if fresh.is_null() {
        return core::ptr::null_mut();
    }
    let copy = layout.size().min(new_size);
    // SAFETY: `fresh` is a fresh, writable slab region of `new_size` bytes
    // and `pointer` is a valid region of `layout.size()` bytes (the caller's realloc contract); the copy is bounded by
    // both, and the two regions are disjoint allocations.
    unsafe { core::ptr::copy_nonoverlapping(pointer, fresh, copy) };
    if owns(pointer) {
        dealloc(pointer);
    }
    fresh
}

/// Rewinds the bump to offset 0. Does not touch the bytes.
///
/// Callers must not hold a slab pointer: this runs when the last ambient scope in the process has dropped, or from
/// tests that have already dropped every region they allocated.
pub(crate) fn reset() -> bool {
    if LIVE_BLOCKS.load(Ordering::Acquire) != 0 {
        return false;
    }
    BUMP.store(0, Ordering::Release);
    true
}

#[cfg(test)]
pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bump_allocations_are_aligned_and_distinct() {
        let _lock = test_lock();
        assert!(reset());
        let base = SLAB.0.get() as usize;
        let first = alloc(Layout::from_size_align(8, 8).expect("layout"));
        let second = alloc(Layout::from_size_align(64, 64).expect("layout"));
        assert!(!first.is_null());
        assert!(!second.is_null());
        assert_eq!(first as usize % 8, 0);
        assert_eq!(second as usize % 64, 0);
        assert_ne!(first, second);
        assert!(owns(first) && owns(second));
        assert!(first as usize >= base && second as usize >= base);
        dealloc(first);
        dealloc(second);
        assert!(reset());
    }

    #[test]
    fn realloc_preserves_contents() {
        let _lock = test_lock();
        assert!(reset());
        let layout = Layout::from_size_align(16, 8).expect("layout");
        let old = alloc(layout);
        assert!(!old.is_null());
        // SAFETY: `old` is a valid 16-byte region.
        unsafe { core::ptr::write_bytes(old, 0x5a, 16) };
        let fresh = realloc(old, layout, 32);
        assert!(!fresh.is_null());
        assert!(owns(fresh));
        // SAFETY: `fresh` is a valid 32-byte region with the first 16 copied.
        unsafe {
            for index in 0..16 {
                assert_eq!(*fresh.add(index), 0x5a);
            }
        }
        dealloc(fresh);
        assert!(reset());
    }

    #[test]
    fn failed_realloc_preserves_the_original_block() {
        let _lock = test_lock();
        assert!(reset());
        let layout = Layout::from_size_align(16, 8).expect("layout");
        let pointer = alloc(layout);
        assert!(!pointer.is_null());
        // SAFETY: `pointer` is a valid 16-byte region.
        unsafe { core::ptr::write_bytes(pointer, 0x5a, 16) };

        assert!(realloc(pointer, layout, SLAB_BYTES + 1).is_null());
        // SAFETY: failed realloc leaves the original region live.
        unsafe {
            for index in 0..16 {
                assert_eq!(*pointer.add(index), 0x5a);
            }
        }

        dealloc(pointer);
        assert!(reset());
    }

    #[test]
    fn a_layout_larger_than_the_slab_is_a_true_oom() {
        let _lock = test_lock();
        assert!(reset());
        let layout = Layout::from_size_align(SLAB_BYTES + 1, 1).expect("layout");
        assert!(alloc(layout).is_null());
    }

    /// A `Layout`'s alignment is not bounded by its size, so a small, wildly over-aligned layout reaches this path: it
    /// can never be satisfied inside the slab, and the refusal must be the true-OOM null rather than address arithmetic
    /// that runs off the end.
    #[test]
    fn a_layout_aligned_beyond_the_slab_is_a_true_oom() {
        let _lock = test_lock();
        assert!(reset());
        let layout = Layout::from_size_align(8, SLAB_BYTES * 2).expect("layout");
        assert!(alloc(layout).is_null());
    }

    #[test]
    fn reset_rewinds_the_bump_to_the_first_offset() {
        let _lock = test_lock();
        assert!(reset());
        let layout = Layout::from_size_align(64, 8).expect("layout");
        let first = alloc(layout);
        assert!(!first.is_null());
        let second = alloc(Layout::from_size_align(1024, 1).expect("layout"));
        assert!(!second.is_null());
        assert_ne!(first, second);
        dealloc(first);
        dealloc(second);
        assert!(reset());
        let again = alloc(layout);
        assert_eq!(again, first, "the next allocation after reset starts at offset 0");
        dealloc(again);
        assert!(reset());
    }

    #[test]
    fn reset_does_not_reuse_a_live_block() {
        let _lock = test_lock();
        assert!(reset());
        let layout = Layout::from_size_align(64, 8).expect("layout");
        let first = alloc(layout);
        assert!(!first.is_null());
        let used = BUMP.load(Ordering::Relaxed);
        assert!(!reset());
        assert_eq!(
            BUMP.load(Ordering::Relaxed),
            used,
            "a live slab block keeps the bump from rewinding"
        );
        dealloc(first);
        assert!(reset());
    }

    #[test]
    fn reset_leaves_bytes_and_alloc_zeroed_clears_them() {
        let _lock = test_lock();
        assert!(reset());
        let layout = Layout::from_size_align(16, 1).expect("layout");
        let pointer = alloc(layout);
        assert!(!pointer.is_null());
        // SAFETY: `pointer` is a live 16-byte slab region.
        unsafe { core::ptr::write_bytes(pointer, 0x5a, 16) };
        dealloc(pointer);
        assert!(reset());
        let reused = alloc(layout);
        assert_eq!(reused, pointer);
        // SAFETY: rewind does not memset; the previous generation is still there.
        unsafe {
            for index in 0..16 {
                assert_eq!(*reused.add(index), 0x5a);
            }
        }
        dealloc(reused);
        assert!(reset());
        let zeroed = alloc_zeroed(layout);
        assert_eq!(zeroed, pointer);
        // SAFETY: `alloc_zeroed` zeros the new region.
        unsafe {
            for index in 0..16 {
                assert_eq!(*zeroed.add(index), 0);
            }
        }
        dealloc(zeroed);
        assert!(reset());
    }
}
