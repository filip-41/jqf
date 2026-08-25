//! Opt-in allocation accounting for a worker's one-shot benchmark operations.
//!
//! The worker must install [`MeasuringAllocator`] as its `#[global_allocator]`.
//! Allocation runs are intentionally separate from timing runs because the
//! provenance tracker and atomic counters would otherwise alter the timed path.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    collections::HashMap,
    hint::black_box,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
};

static MEASURING: AtomicBool = AtomicBool::new(false);
static CURRENT_EPOCH: AtomicU64 = AtomicU64::new(0);
static NEXT_EPOCH: AtomicU64 = AtomicU64::new(0);
static ALLOCATION_CALLS: AtomicUsize = AtomicUsize::new(0);
static REALLOCATION_CALLS: AtomicUsize = AtomicUsize::new(0);
static REQUESTED_BYTES: AtomicUsize = AtomicUsize::new(0);
static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
static PEAK_LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
static PROVENANCE: OnceLock<Mutex<HashMap<usize, AllocationRecord>>> = OnceLock::new();

thread_local! {
    static MEASUREMENT_OWNER: Cell<bool> = const { Cell::new(false) };
    static TRACKING_PROVENANCE: Cell<bool> = const { Cell::new(false) };
}

#[derive(Clone, Copy)]
struct AllocationRecord {
    size: usize,
    epoch: u64,
}

/// Global allocator wrapper that records successful heap operations while a
/// [`measure`] closure is active.
pub struct MeasuringAllocator;

// SAFETY: every operation delegates to `System` with the caller-provided
// pointer and layout. The provenance registry only observes successful
// allocation changes and bypasses its own allocator traffic.
unsafe impl GlobalAlloc for MeasuringAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: `layout` comes from the `GlobalAlloc` caller.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            observe_allocation(pointer, layout.size());
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: `layout` comes from the `GlobalAlloc` caller.
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            observe_allocation(pointer, layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        observe_deallocation(pointer);
        // SAFETY: pointer and layout come from the `GlobalAlloc` caller.
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: pointer, layout, and new size come from the `GlobalAlloc` caller.
        let new_pointer = unsafe { System.realloc(pointer, layout, new_size) };
        if !new_pointer.is_null() {
            observe_reallocation(pointer, new_pointer, new_size);
        }
        new_pointer
    }
}

/// Allocation totals observed while one complete operation ran.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AllocationStats {
    /// Successful allocation calls.
    pub allocation_calls: usize,
    /// Successful reallocation calls.
    pub reallocation_calls: usize,
    /// Total successful allocation and reallocation request volume.
    pub requested_bytes: usize,
    /// Maximum concurrently live requested bytes from measured allocations.
    pub peak_live_bytes: usize,
    /// Requested bytes from measured allocations still live when the closure returned.
    pub retained_bytes: usize,
}

/// Measure the allocation activity of one complete operation.
///
/// The harness is single-threaded and measurements cannot be nested. The
/// allocator records pointer provenance outside the measurement as well as
/// inside it, so dropping a fixture allocated before the closure cannot erase
/// retained bytes from allocations made by the closure. A successful
/// reallocation during the closure creates a measured allocation at its new
/// size, even when it resizes pre-existing fixture storage.
///
/// This returns accurate allocator observations only when the worker declares
/// [`MeasuringAllocator`] as its process global allocator.
///
/// # Panics
///
/// Panics when allocation measurements are nested.
pub fn measure<T>(operation: impl FnOnce() -> T) -> (T, AllocationStats) {
    assert!(
        MEASURING
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok(),
        "allocation measurement cannot be nested"
    );
    MEASUREMENT_OWNER.with(|owner| owner.set(true));
    let epoch = NEXT_EPOCH.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    CURRENT_EPOCH.store(epoch, Ordering::Relaxed);
    reset();
    let guard = MeasurementGuard;
    let value = operation();
    drop(guard);
    (value, snapshot())
}

/// Verify that the worker installed [`MeasuringAllocator`] as its process
/// global allocator.
///
/// The check performs exact allocation, growth, shrink, and deallocation
/// operations. It does not leave retained bytes behind.
///
/// # Errors
///
/// Returns an explanatory error when the active global allocator does not
/// report the exact expected accounting receipt.
///
/// # Panics
///
/// Panics only if the fixed self-check layouts are unexpectedly invalid.
pub fn verify_global_allocator() -> Result<(), &'static str> {
    let initial = Layout::from_size_align(64, 8).expect("fixed self-check layout is valid");
    let grown = Layout::from_size_align(128, 8).expect("fixed self-check layout is valid");
    let shrunk = Layout::from_size_align(32, 8).expect("fixed self-check layout is valid");
    let ((), statistics) = measure(|| {
        // SAFETY: every successful allocation is checked, resized with its
        // current layout, and deallocated exactly once with its final layout.
        unsafe {
            let pointer = std::alloc::alloc(initial);
            if pointer.is_null() {
                return;
            }
            black_box(pointer);
            let grown_pointer = std::alloc::realloc(pointer, initial, grown.size());
            if grown_pointer.is_null() {
                std::alloc::dealloc(pointer, initial);
                return;
            }
            black_box(grown_pointer);
            let shrunk_pointer = std::alloc::realloc(grown_pointer, grown, shrunk.size());
            if shrunk_pointer.is_null() {
                std::alloc::dealloc(grown_pointer, grown);
                return;
            }
            black_box(shrunk_pointer);
            std::alloc::dealloc(shrunk_pointer, shrunk);
        }
    });
    let expected = AllocationStats {
        allocation_calls: 1,
        reallocation_calls: 2,
        requested_bytes: 224,
        peak_live_bytes: 128,
        retained_bytes: 0,
    };
    (statistics == expected)
        .then_some(())
        .ok_or("the active global allocator did not produce the benchmark accounting receipt")
}

struct MeasurementGuard;

impl Drop for MeasurementGuard {
    fn drop(&mut self) {
        CURRENT_EPOCH.store(0, Ordering::Relaxed);
        MEASUREMENT_OWNER.with(|owner| owner.set(false));
        MEASURING.store(false, Ordering::Relaxed);
    }
}

fn observe_allocation(pointer: *mut u8, size: usize) {
    let epoch = measurement_epoch().unwrap_or(0);
    let observed = with_provenance(|provenance| {
        provenance.insert(pointer.addr(), AllocationRecord { size, epoch });
    });
    if observed.is_some() && epoch != 0 {
        record_allocation(size);
    }
}

fn observe_deallocation(pointer: *mut u8) {
    let record = with_provenance(|provenance| provenance.remove(&pointer.addr())).flatten();
    if let (Some(record), Some(epoch)) = (record, measurement_epoch())
        && record.epoch == epoch
    {
        subtract_live(record.size);
    }
}

fn observe_reallocation(pointer: *mut u8, new_pointer: *mut u8, new_size: usize) {
    let epoch = measurement_epoch().unwrap_or(0);
    let observation = with_provenance(|provenance| {
        let previous = provenance.remove(&pointer.addr());
        provenance.insert(new_pointer.addr(), AllocationRecord { size: new_size, epoch });
        previous
    });
    if let Some(previous) = observation
        && epoch != 0
    {
        record_reallocation(previous.filter(|record| record.epoch == epoch), new_size);
    }
}

fn measurement_epoch() -> Option<u64> {
    if !MEASURING.load(Ordering::Relaxed) {
        return None;
    }
    MEASUREMENT_OWNER
        .with(Cell::get)
        .then(|| CURRENT_EPOCH.load(Ordering::Relaxed))
        .filter(|epoch| *epoch != 0)
}

fn with_provenance<T>(operation: impl FnOnce(&mut HashMap<usize, AllocationRecord>) -> T) -> Option<T> {
    TRACKING_PROVENANCE.with(|tracking| {
        if tracking.replace(true) {
            return None;
        }
        let _guard = TrackingGuard { tracking };
        let provenance = PROVENANCE.get_or_init(|| Mutex::new(HashMap::new()));
        let mut provenance = provenance.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        Some(operation(&mut provenance))
    })
}

struct TrackingGuard<'a> {
    tracking: &'a Cell<bool>,
}

impl Drop for TrackingGuard<'_> {
    fn drop(&mut self) {
        self.tracking.set(false);
    }
}

fn reset() {
    ALLOCATION_CALLS.store(0, Ordering::Relaxed);
    REALLOCATION_CALLS.store(0, Ordering::Relaxed);
    REQUESTED_BYTES.store(0, Ordering::Relaxed);
    LIVE_BYTES.store(0, Ordering::Relaxed);
    PEAK_LIVE_BYTES.store(0, Ordering::Relaxed);
}

fn snapshot() -> AllocationStats {
    AllocationStats {
        allocation_calls: ALLOCATION_CALLS.load(Ordering::Relaxed),
        reallocation_calls: REALLOCATION_CALLS.load(Ordering::Relaxed),
        requested_bytes: REQUESTED_BYTES.load(Ordering::Relaxed),
        peak_live_bytes: PEAK_LIVE_BYTES.load(Ordering::Relaxed),
        retained_bytes: LIVE_BYTES.load(Ordering::Relaxed),
    }
}

fn record_allocation(bytes: usize) {
    ALLOCATION_CALLS.fetch_add(1, Ordering::Relaxed);
    REQUESTED_BYTES.fetch_add(bytes, Ordering::Relaxed);
    add_live(bytes);
}

fn record_reallocation(previous: Option<AllocationRecord>, new_size: usize) {
    REALLOCATION_CALLS.fetch_add(1, Ordering::Relaxed);
    REQUESTED_BYTES.fetch_add(new_size, Ordering::Relaxed);
    if let Some(previous) = previous {
        subtract_live(previous.size);
    }
    add_live(new_size);
}

fn add_live(bytes: usize) {
    let live = LIVE_BYTES.fetch_add(bytes, Ordering::Relaxed).saturating_add(bytes);
    PEAK_LIVE_BYTES.fetch_max(live, Ordering::Relaxed);
}

fn subtract_live(bytes: usize) {
    LIVE_BYTES
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |live| {
            Some(live.saturating_sub(bytes))
        })
        .expect("the update closure always returns a value");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn counters_separate_request_volume_from_live_bytes() {
        reset();
        record_allocation(64);
        record_reallocation(Some(AllocationRecord { size: 64, epoch: 1 }), 128);
        record_reallocation(Some(AllocationRecord { size: 128, epoch: 1 }), 32);
        assert_eq!(
            snapshot(),
            AllocationStats {
                allocation_calls: 1,
                reallocation_calls: 2,
                requested_bytes: 224,
                peak_live_bytes: 128,
                retained_bytes: 32,
            }
        );
        subtract_live(32);
        assert_eq!(snapshot().retained_bytes, 0);
    }

    #[test]
    fn fixture_deallocation_does_not_hide_measured_retained_storage() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let fixture = Layout::from_size_align(1_024, 8).unwrap();
        let retained = Layout::from_size_align(64, 8).unwrap();
        // SAFETY: the allocation is checked and deallocated exactly once.
        let fixture_pointer = unsafe { std::alloc::alloc(fixture) };
        assert!(!fixture_pointer.is_null());
        let (retained_pointer, statistics) = measure(|| unsafe {
            // SAFETY: the allocation is checked and intentionally retained until
            // after the observed measurement.
            let retained_pointer = std::alloc::alloc(retained);
            assert!(!retained_pointer.is_null());
            // SAFETY: this is the pre-existing allocation with its original layout.
            std::alloc::dealloc(fixture_pointer, fixture);
            retained_pointer
        });
        assert_eq!(
            statistics,
            AllocationStats {
                allocation_calls: 1,
                reallocation_calls: 0,
                requested_bytes: 64,
                peak_live_bytes: 64,
                retained_bytes: 64,
            }
        );
        // SAFETY: the retained allocation is released with its original layout.
        unsafe { std::alloc::dealloc(retained_pointer, retained) };
    }

    #[test]
    fn fixture_reallocation_counts_the_new_measured_storage() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let initial = Layout::from_size_align(64, 8).unwrap();
        let grown = Layout::from_size_align(128, 8).unwrap();
        // SAFETY: the allocation is checked and reallocated once.
        let fixture_pointer = unsafe { std::alloc::alloc(initial) };
        assert!(!fixture_pointer.is_null());
        let (grown_pointer, statistics) = measure(|| unsafe {
            // SAFETY: the pointer has the matching current layout.
            std::alloc::realloc(fixture_pointer, initial, grown.size())
        });
        assert!(!grown_pointer.is_null());
        assert_eq!(
            statistics,
            AllocationStats {
                allocation_calls: 0,
                reallocation_calls: 1,
                requested_bytes: 128,
                peak_live_bytes: 128,
                retained_bytes: 128,
            }
        );
        // SAFETY: the successful reallocation has the grown layout.
        unsafe { std::alloc::dealloc(grown_pointer, grown) };
    }
}
