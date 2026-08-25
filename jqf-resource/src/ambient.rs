//! The thread-local account the counting allocator reads.
//!
//! A global allocator is only handed a size. The request to charge is the account in this thread's slot: [`install`]
//! it, allocate, drop the [`ScopeGuard`]. Workers each have their own slot. The counting allocator is the caller; a
//! refused allocation takes the emergency slab.
//!
//! Past the ceiling, allocator charging trips and the allocator serves the slab. Bookkeeping uses `try_with` because it
//! also runs while the thread is tearing down — a panic there would abort. No slot means no ceiling.

use std::cell::{Cell, UnsafeCell};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use crate::{MemoryCategory, RequestAccount, ResourceError, ResourceLimit};

/// High bit of [`LIVE_SCOPES`]: the slab was claimed and still needs reset. Sharing one atomic makes claim and
/// healthy-scope admission race-free.
const RESET_PENDING: usize = 1 << (usize::BITS - 1);

/// Outermost ambient installs in the low bits; [`RESET_PENDING`] in the high bit.
static LIVE_SCOPES: AtomicUsize = AtomicUsize::new(0);

/// Serializes slab reset with installs that observe [`RESET_PENDING`].
static QUIESCE: Mutex<()> = Mutex::new(());

#[inline]
const fn live_count(state: usize) -> usize {
    state & !RESET_PENDING
}

/// One thread-local for every ambient slot the allocator reads. A charge or release is then one `try_with`, not a
/// lookup per field.
struct AmbientSlots {
    /// `UnsafeCell`, not `RefCell`: `RequestAccount` is Cell-only interior mutability, so a charge that re-enters while
    /// [`with_account`] holds a shared reference is aliasing of Cells, not a second exclusive borrow. `RefCell` would
    /// panic inside the allocator on that path. Install/drop still extract the `Option` by pointer and must not overlap
    /// a live shared reference.
    account: UnsafeCell<Option<RequestAccount>>,
    /// `Working` is the default because an allocation nobody attributed is transient by assumption; a retained one is
    /// worth a [`category_scope`].
    category: Cell<MemoryCategory>,
    /// Latched until the scope ends. The error, not a flag: a ceiling refusal and an arithmetic or invariant defect
    /// share this path, and [`cooperative_refusal`] must tell them apart.
    tripped: Cell<Option<ResourceError>>,
    /// Re-entrancy latch. See [`without_ledger_release`].
    charging: Cell<bool>,
    /// Live [`with_account`] closures. Install/drop must not replace the `Option` while this is non-zero.
    viewing: Cell<u32>,
    /// This thread claimed the process-wide emergency slab.
    owns_slab: Cell<bool>,
}

thread_local! {
    static SLOTS: AmbientSlots = const {
        AmbientSlots {
            account: UnsafeCell::new(None),
            category: Cell::new(MemoryCategory::Working),
            tripped: Cell::new(None),
            charging: Cell::new(false),
            viewing: Cell::new(0),
            owns_slab: Cell::new(false),
        }
    };
}

/// Restores the previous thread account when dropped.
///
/// Nested installs restore the outer account and must not rewind the process slab. An inert guard means [`install`]
/// never mutated TLS: [`with_account`] was live, or TLS was gone. Dropping an inert guard still releases the pending
/// account without charging whoever is ambient — see the drop impl.
#[must_use = "installing the scope and dropping the guard immediately restores the previous account"]
pub struct ScopeGuard {
    previous: Option<RequestAccount>,
    previously_tripped: Option<ResourceError>,
    /// [`install`] did not mutate TLS; the pending account is in `previous`.
    inert: bool,
    /// This install found an empty thread slot.
    outermost: bool,
}

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        if self.inert {
            // The install never ran, so the pending account would ride out as a plain field drop — deallocating
            // through the global allocator against whatever account is ambient, whose release clamps and steals
            // headroom it never owned. Wrap it exactly like the normal path below so every terminal path releases
            // symmetrically.
            without_ledger_release(|| drop(self.previous.take()));
            return;
        }
        // Extract the installed account and drop it OUTSIDE the slot write: its AccountBox deallocation re-enters
        // `release`.
        let restore = SLOTS.try_with(|slots| {
            if slots.viewing.get() != 0 {
                // A drop inside a live `with_account` view: replacing the Option would free the account the view still
                // borrows — safe-code use-after-free. Leave the slot untouched and leak the installed account
                // instead; a leak is the sound degradation.
                return Err(());
            }
            // SAFETY: drop is not on the allocator path except via the
            // extracted account's later dealloc, which runs after this replace. `try_with` is the TLS-teardown law.
            // `viewing` is 0, so no shared reference to the Option is live.
            Ok(unsafe { std::mem::replace(&mut *slots.account.get(), self.previous.take()) })
        });
        let installed = match restore {
            // TLS is tearing down; the slot writes below will fail too and there is nothing to extract.
            Err(_) => None,
            // Drop inside a live `with_account` view: the account was left in place above (the leak is the sound
            // degradation). The slot still serves it and its blocks may sit in the claimed slab, so the rewind below
            // must not run even when this was the outermost scope — RESET_PENDING stays latched for the rest of the
            // process, which is benign (the slab just is not recycled).
            Ok(Err(())) => return,
            Ok(Ok(installed)) => installed,
        };
        let _ = SLOTS.try_with(|slots| {
            slots.tripped.set(self.previously_tripped);
            if self.outermost {
                slots.owns_slab.set(false);
            }
        });
        // The extracted account's last handle may deallocate its AccountBox. Ambient is already the previous account;
        // that dealloc must not `release` against it.
        without_ledger_release(|| drop(installed));
        let last_pending = if self.outermost {
            let old = LIVE_SCOPES.fetch_sub(1, Ordering::AcqRel);
            live_count(old) == 1 && old & RESET_PENDING != 0
        } else {
            false
        };
        if last_pending {
            rewind_slab();
        }
    }
}

/// Locks and unlocks the process-wide ambient quiesce mutex.
///
/// `std::sync::Mutex`'s first lock allocates a 64-byte pthread mutex box (`OnceBox` + `Box<sys::Mutex>`). Healthy
/// outermost installs skip this lock. A pending install or slab rewind still takes it; if the parent did not warm the
/// lock, its box can be charged to a child and prevent detach.
pub fn warm_ambient_mutex() {
    let _quiesce = QUIESCE.lock().unwrap_or_else(PoisonError::into_inner);
}

/// Install `account` as this thread's account until the guard drops.
///
/// If [`with_account`] is live, or TLS is tearing down, this does not mutate the slot and returns an inert guard so a
/// live borrow is not replaced.
pub fn install(account: RequestAccount) -> ScopeGuard {
    let mut pending = Some(account);
    match SLOTS.try_with(|slots| {
        if slots.viewing.get() != 0 {
            return None;
        }
        // SAFETY: install is not on the allocator path. `viewing` is 0.
        let account = pending.take()?;
        let previous = unsafe { (*slots.account.get()).replace(account) };
        let outermost = previous.is_none();
        if outermost {
            join_outermost_scope();
        }
        let previously_tripped = slots.tripped.replace(None);
        Some(ScopeGuard {
            previous,
            previously_tripped,
            inert: false,
            outermost,
        })
    }) {
        Ok(Some(guard)) => guard,
        Ok(None) | Err(_) => ScopeGuard {
            previous: pending,
            previously_tripped: None,
            inert: true,
            outermost: false,
        },
    }
}

/// Publishes one outermost scope without racing slab claim or reset.
///
/// The fast path increments only while [`RESET_PENDING`] is clear. The slow path locks, resets only the current
/// zero-scope generation, then increments under the lock; no pre-lock count is reused across slab generations.
fn join_outermost_scope() {
    loop {
        let current = LIVE_SCOPES.load(Ordering::Acquire);
        if current & RESET_PENDING == 0 {
            debug_assert!(current < RESET_PENDING - 1, "too many live ambient scopes");
            if LIVE_SCOPES
                .compare_exchange_weak(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
            continue;
        }

        let _quiesce = QUIESCE.lock().unwrap_or_else(PoisonError::into_inner);
        let current = LIVE_SCOPES.load(Ordering::Acquire);
        if current & RESET_PENDING != 0 && live_count(current) == 0 {
            reset_pending_slab();
        }
        debug_assert!(
            live_count(LIVE_SCOPES.load(Ordering::Relaxed)) < RESET_PENDING - 1,
            "too many live ambient scopes"
        );
        LIVE_SCOPES.fetch_add(1, Ordering::AcqRel);
        return;
    }
}

/// Rewinds the emergency slab when no outermost ambient scope remains. A concurrent install that landed after the
/// last-drop subtract skips the rewind: that request still owns any live pointers.
fn rewind_slab() {
    let _quiesce = QUIESCE.lock().unwrap_or_else(PoisonError::into_inner);
    if live_count(LIVE_SCOPES.load(Ordering::Acquire)) != 0 {
        return;
    }
    reset_pending_slab();
}

fn reset_pending_slab() {
    #[cfg(test)]
    let _lock = crate::slab::test_lock();
    if crate::slab::reset() {
        LIVE_SCOPES.fetch_and(!RESET_PENDING, Ordering::Release);
    }
}

/// Decrements the `with_account` viewing latch.
struct ViewingGuard;

impl Drop for ViewingGuard {
    fn drop(&mut self) {
        let _ = SLOTS.try_with(|slots| {
            slots.viewing.set(slots.viewing.get().saturating_sub(1));
        });
    }
}

/// Reads the ambient account, or `None` when no scope is installed.
pub fn with_account<R>(f: impl FnOnce(&RequestAccount) -> R) -> Option<R> {
    SLOTS
        .try_with(|slots| {
            slots.viewing.set(slots.viewing.get().saturating_add(1));
            let _view = ViewingGuard;
            // SAFETY: shared read of a Cell-only account. `viewing` is held
            // so install/drop will not replace the Option. Allocation inside `f` is legal and re-enters `charge`.
            unsafe { (*slots.account.get()).as_ref().map(f) }
        })
        .ok()
        .flatten()
}

/// The RAII category selector. Bytes charged while it is alive are attributed to its category.
#[must_use = "the category applies for the guard's lifetime; dropping it immediately restores the previous category"]
pub(crate) struct CategoryGuard(MemoryCategory);

impl Drop for CategoryGuard {
    fn drop(&mut self) {
        let _ = SLOTS.try_with(|slots| slots.category.set(self.0));
    }
}

/// Attributes every subsequent charge on this thread to `category` until the returned guard drops.
pub(crate) fn category_scope(category: MemoryCategory) -> CategoryGuard {
    CategoryGuard(
        SLOTS
            .try_with(|slots| slots.category.replace(category))
            .unwrap_or(category),
    )
}

/// Sets the re-entrancy latch for the guard's lifetime. See [`without_ledger_release`].
struct ChargingGuard {
    armed: bool,
}

impl ChargingGuard {
    /// `None` if the latch was already held or TLS is gone.
    fn try_enter() -> Option<Self> {
        SLOTS
            .try_with(|slots| {
                if slots.charging.get() {
                    None
                } else {
                    slots.charging.set(true);
                    Some(Self { armed: true })
                }
            })
            .ok()
            .flatten()
    }

    /// Holds the latch if we can; if it is already held, runs without arming.
    fn enter() -> Self {
        Self::try_enter().unwrap_or(Self { armed: false })
    }
}

impl Drop for ChargingGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = SLOTS.try_with(|slots| slots.charging.set(false));
        }
    }
}

/// Clears the re-entrancy latch when a charge/release already holds `SLOTS`.
struct ChargingHold<'a>(&'a Cell<bool>);

impl Drop for ChargingHold<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

/// Runs `f` with ambient charge and release suppressed.
///
/// The hold is the same `charging` latch the allocator uses, so nested allocator charge/release calls are no-ops. Both
/// directions are load-bearing: a `Vec` whose charge lived on a worker account (or was never charged) must be dropped
/// this way so dealloc does not steal Working, and an `AccountBox` alloc/dealloc must run this way so an account's own
/// storage is never charged to (or released against) whoever is ambient.
pub(crate) fn without_ledger_release<R>(f: impl FnOnce() -> R) -> R {
    let _hold = ChargingGuard::enter();
    f()
}

/// Allocator-boundary charge: `true` if the bytes were booked (or there is no account), `false` if a trip was latched.
#[inline]
pub(crate) fn charge_bytes(bytes: usize) -> bool {
    if bytes == 0 {
        return true;
    }
    SLOTS
        .try_with(|slots| {
            if slots.charging.get() {
                return true;
            }
            slots.charging.set(true);
            let _hold = ChargingHold(&slots.charging);
            let category = slots.category.get();
            // SAFETY: the charging latch is set; install/drop cannot run.
            // `RequestAccount` mutates only through Cells.
            let outcome = match unsafe { (*slots.account.get()).as_ref() } {
                None => Ok(()),
                Some(account) => {
                    let outcome = account.charge_residency(bytes as u64, category);
                    if let Err(error) = outcome {
                        account.state.note_trip(error);
                    }
                    outcome
                }
            };
            if let Err(error) = outcome {
                slots.tripped.set(Some(error));
                false
            } else {
                true
            }
        })
        .unwrap_or(true)
}

/// Give back `bytes` previously charged. No-op if nothing is installed.
#[inline]
pub(crate) fn release(bytes: usize) {
    if bytes == 0 {
        return;
    }
    let _ = SLOTS.try_with(|slots| {
        if slots.charging.get() {
            return;
        }
        slots.charging.set(true);
        let _hold = ChargingHold(&slots.charging);
        let category = slots.category.get();
        // SAFETY: the charging latch is set; install/drop cannot run.
        if let Some(account) = unsafe { (*slots.account.get()).as_ref() } {
            // Clamp here, once: an allocation made before the scope was installed (or freed after it dropped, or on
            // another thread's account) has no matching charge. The account then subtracts without a second min.
            let charged = account.current_residency(category);
            let bytes = (bytes as u64).min(charged);
            if bytes > 0 {
                account.release_residency(category, bytes);
            }
        }
    });
}

/// True if a charge already failed in this scope (ceiling or otherwise).
#[must_use]
pub fn tripped() -> bool {
    SLOTS.try_with(|slots| slots.tripped.get().is_some()).unwrap_or(false)
}

/// First tripper in this request owns the emergency slab; later threads get null. The last scope drop releases the
/// claim when no block remains; otherwise the first outermost install after the final free releases it.
pub(crate) fn claim_emergency_slab() -> bool {
    SLOTS
        .try_with(|slots| {
            if slots.owns_slab.get() {
                return true;
            }
            let old = LIVE_SCOPES.fetch_or(RESET_PENDING, Ordering::AcqRel);
            if old & RESET_PENDING == 0 {
                slots.owns_slab.set(true);
                true
            } else {
                false
            }
        })
        .unwrap_or(false)
}

/// `Err` if a charge already tripped in this scope, `Ok(())` otherwise.
///
/// Call this at a work-slice pause instead of allocating again (that would abort). A ceiling hit is re-read from the
/// live counters so the numbers are current. Any other latched failure (overflow, a bug) is returned as-is.
///
/// # Errors
///
/// Returns [`ResourceError::LimitExceeded`] when a ceiling refusal is latched, carrying the account's live ceiling and
/// current usage and the charge that tripped it; otherwise the latched error unchanged.
pub fn cooperative_refusal() -> Result<(), ResourceError> {
    SLOTS
        .try_with(|slots| {
            let Some(latched) = slots.tripped.get() else {
                return Ok(());
            };
            let ResourceError::LimitExceeded { requested_delta, .. } = latched else {
                return Err(latched);
            };
            // SAFETY: not on the allocator path.
            let (limit, current) = match unsafe { (*slots.account.get()).as_ref() } {
                None => (0, 0),
                Some(account) => (
                    account.limits().max_memory_bytes(),
                    account.snapshot().memory_current_bytes(),
                ),
            };
            Err(ResourceError::LimitExceeded {
                limit_kind: ResourceLimit::MemoryBytes,
                limit,
                current,
                requested_delta,
            })
        })
        .unwrap_or(Ok(()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MemoryCategory, RequestAccount, ResourceLimits};

    fn charge(bytes: usize) -> Result<(), ResourceError> {
        if charge_bytes(bytes) {
            Ok(())
        } else {
            cooperative_refusal()
        }
    }

    fn account(memory: u64) -> RequestAccount {
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, memory, u64::MAX, u32::MAX)).expect("account")
    }

    /// The account commits its own fixed ledger allocation beneath the memory ceiling at creation, so a charge test
    /// measures against the baseline it observes inside the scope, not against the raw ceiling.
    fn headroom(ceiling: u64) -> usize {
        usize::try_from(
            ceiling
                - with_account(RequestAccount::snapshot)
                    .expect("in scope")
                    .memory_current_bytes(),
        )
        .expect("headroom fits in usize")
    }

    fn clear_leaked_scope() {
        let installed = SLOTS.with(|slots| {
            assert_eq!(slots.viewing.get(), 0);
            // SAFETY: the test has left every `with_account` view.
            unsafe { (*slots.account.get()).take() }
        });
        without_ledger_release(|| drop(installed));
        let old = LIVE_SCOPES.fetch_sub(1, Ordering::AcqRel);
        if live_count(old) == 1 && old & RESET_PENDING != 0 {
            rewind_slab();
        }
    }

    /// [`with_account`] holds a shared reference; allocation inside it re-enters [`charge`]. That must not panic inside
    /// the allocator.
    #[test]
    fn with_account_may_allocate() {
        let _guard = install(account(u64::MAX));
        {
            with_account(|_| {
                let held: Vec<u8> = Vec::with_capacity(64 * 1024);
                assert!(held.capacity() >= 64 * 1024);
            })
            .expect("scope");
        }
    }

    /// No installed account means [`charge`] succeeds and does not trip.
    #[test]
    fn charge_outside_a_scope_is_a_no_op() {
        assert!(charge(1024).is_ok(), "no scope installed means no ceiling to cross");
        assert!(!tripped());
    }

    /// Charges fill remaining headroom then refuse; [`tripped`] is then true.
    #[test]
    fn charge_inside_a_scope_accumulates_and_refuses_past_the_ceiling() {
        let _guard = install(account(4096));
        {
            let spendable = headroom(4096);
            let first = spendable / 2;
            let second = spendable - first;
            assert!(charge(first).is_ok());
            assert!(charge(second).is_ok());
            assert!(charge(1).is_err(), "the ceiling is hard");
            assert!(tripped());
        }
    }

    /// Pins [`cooperative_refusal`]: untripped is a no-op; a ceiling trip returns live numbers. Overflow keeps its own
    /// class — see [`a_non_ceiling_charge_failure_surfaces_as_itself`].
    #[test]
    fn cooperative_refusal_returns_the_typed_ceiling_error_once_tripped() {
        let _guard = install(account(4096));
        {
            assert!(cooperative_refusal().is_ok(), "untripped check is a no-op");
            let spendable = headroom(4096);
            assert!(charge(spendable).is_ok());
            assert!(charge(1).is_err(), "the ceiling is hard");
            let error = cooperative_refusal().expect_err("tripped check refuses");
            match error {
                ResourceError::LimitExceeded {
                    limit_kind,
                    limit,
                    current,
                    requested_delta,
                } => {
                    assert_eq!(limit_kind, ResourceLimit::MemoryBytes);
                    assert_eq!(limit, 4096, "the ceiling is reported live");
                    assert!(current > 0, "current usage is reported live");
                    assert_eq!(requested_delta, 1, "the refused charge is the delta");
                }
                other => panic!("expected the typed memory-limit error, got: {other:?}"),
            }
        }
    }

    /// Overflow is latched as itself, not rewritten as a ceiling miss. The ceiling row is
    /// [`cooperative_refusal_returns_the_typed_ceiling_error_once_tripped`].
    #[test]
    #[cfg(target_pointer_width = "64")]
    fn a_non_ceiling_charge_failure_surfaces_as_itself() {
        let _guard = install(account(u64::MAX));
        {
            assert_eq!(
                charge(usize::MAX),
                Err(ResourceError::ArithmeticOverflow),
                "the accumulation overflows before the ceiling is consulted"
            );
            assert!(tripped(), "any refused charge latches the trip");
            assert_eq!(
                cooperative_refusal(),
                Err(ResourceError::ArithmeticOverflow),
                "an overflow is a defect signal, not a ceiling report"
            );
        }
    }

    /// A nested scope must not erase the outer scope's trip: the outer scope resumes tripped AND keeps the charge that
    /// tripped it, so its refusal still reports the delta it was refused rather than zero.
    #[test]
    fn a_nested_scope_restores_the_outer_trip_and_its_delta() {
        let _outer = install(account(4096));
        {
            assert!(charge(headroom(4096)).is_ok());
            assert!(charge(7).is_err(), "the ceiling is hard");
            let inner = install(account(4096));
            {
                assert!(!tripped(), "a fresh scope starts untripped");
            }
            drop(inner);
            assert!(tripped(), "the outer scope resumes tripped");
            match cooperative_refusal().expect_err("the outer refusal survives") {
                ResourceError::LimitExceeded { requested_delta, .. } => {
                    assert_eq!(requested_delta, 7, "the outer scope's own refused charge");
                }
                other => panic!("expected the typed memory-limit error, got: {other:?}"),
            }
        }
    }

    /// Released bytes are spendable again under the same ceiling.
    #[test]
    fn release_returns_headroom() {
        let _guard = install(account(4096));
        {
            let spendable = headroom(4096);
            assert!(charge(spendable).is_ok());
            release(spendable / 2);
            assert!(charge(spendable / 2).is_ok(), "released bytes are spendable again");
        }
    }

    /// Dropping a scope restores the next install to its own baseline.
    #[test]
    fn scopes_do_not_leak_across_calls() {
        let first = install(account(4096));
        {
            assert!(charge(headroom(4096)).is_ok());
        }
        drop(first);
        let second = install(account(4096));
        {
            assert!(charge(headroom(4096)).is_ok(), "a fresh scope starts at its baseline");
        }
        drop(second);
    }

    /// A drop inside a live [`with_account`] must not free the slot the view still borrows. The sibling misuse is
    /// [`an_install_inside_a_live_view_hands_back_an_inert_guard`].
    #[test]
    fn a_guard_dropped_inside_a_live_view_leaks_instead_of_freeing_it() {
        let guard = install(account(u64::MAX));
        with_account(|_| drop(guard));
        assert!(
            with_account(|_| ()).is_some(),
            "the leaked account still serves the slot"
        );
        clear_leaked_scope();
    }

    /// An install while [`with_account`] is live cannot mutate the slot; its inert guard is safe to drop. The sibling
    /// misuse is [`a_guard_dropped_inside_a_live_view_leaks_instead_of_freeing_it`].
    #[test]
    fn an_install_inside_a_live_view_hands_back_an_inert_guard() {
        let outer = install(account(u64::MAX));
        {
            with_account(|_| {
                let inert = install(account(u64::MAX));
                drop(inert);
            })
            .expect("scope");
        }
        drop(outer);
        let guard = install(account(u64::MAX));
        drop(guard);
    }

    /// Nested [`category_scope`] restores the outer category on drop.
    #[test]
    fn the_category_guard_restores_the_previous_category_on_drop() {
        let _guard = install(account(u64::MAX));
        {
            let baseline = with_account(RequestAccount::snapshot)
                .expect("in scope")
                .memory(MemoryCategory::Retained)
                .current();
            let outer = category_scope(MemoryCategory::Retained);
            {
                let _inner = category_scope(MemoryCategory::PendingIo);
                charge(64).expect("charge");
            }
            charge(64).expect("charge");
            drop(outer);
            let snapshot = with_account(RequestAccount::snapshot).expect("in scope");
            assert_eq!(snapshot.memory(MemoryCategory::PendingIo).current(), 64);
            assert_eq!(snapshot.memory(MemoryCategory::Retained).current(), baseline + 64);
        }
    }
}
