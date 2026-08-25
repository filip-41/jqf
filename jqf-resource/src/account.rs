//! The request ledger: live counters and the ceilings they sit under.
//!
//! Creating an account allocates the ledger and counts that allocation against itself. A charge either succeeds as a
//! whole or leaves the counters unchanged. Output permits and nesting guards borrow the ledger and release on drop.

use crate::limits::TaskMemoryGroup;
use crate::{DepthGuard, MemoryCategory, MemoryUsage, ResourceError, ResourceLimit, ResourceLimits, UsageSnapshot};
use core::cell::Cell;
use core::fmt;
use core::marker::PhantomData;
use core::ops::Deref;
use core::ptr::{self, NonNull};
use std::alloc::{Layout, alloc, dealloc};
use std::rc::Rc;

/// One request's usage ledger.
///
/// Not `Clone`. Share it with [`Self::try_share`] when the allocator needs a handle on this thread. Everyday use goes
/// through [`crate::ResourceContext`].
///
/// ```compile_fail use jqf_resource::RequestAccount; fn assert_send<T: Send>() {} assert_send::<RequestAccount>();
/// ```
pub struct RequestAccount {
    pub(crate) state: AccountHandle,
}

impl fmt::Debug for RequestAccount {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RequestAccount")
            .field("limits", &self.limits())
            .field("usage", &self.snapshot())
            .finish()
    }
}

impl RequestAccount {
    /// How many bytes the ledger itself occupies. Every account charges this against its memory ceiling.
    #[must_use]
    pub fn minimum_memory_bytes() -> u64 {
        u64::try_from(Layout::new::<AccountBox>().size()).unwrap_or(u64::MAX)
    }

    /// Open a ledger under `limits`. The ledger allocation itself counts against the memory ceiling.
    ///
    /// # Errors
    ///
    /// The memory ceiling, an unrepresentable size, or a failed allocation.
    pub fn try_new(limits: ResourceLimits) -> Result<Self, ResourceError> {
        Ok(Self {
            state: AccountHandle::try_new(limits)?,
        })
    }

    /// A second handle to the same ledger.
    ///
    /// [`crate::install`] puts an owned account in this thread's slot so the counting allocator knows whom to charge.
    /// This method clones the handle so the context can keep one copy and the slot can hold another. Heap charges and
    /// grant holds then hit the same counters. The last handle dropped frees the ledger.
    ///
    /// # Errors
    ///
    /// An arithmetic error if the shared reference cannot be retained (practically infallible).
    pub fn try_share(&self) -> Result<Self, ResourceError> {
        Ok(Self {
            state: self.state.try_clone()?,
        })
    }

    /// The limits this account was opened with.
    #[must_use]
    pub fn limits(&self) -> ResourceLimits {
        self.state.limits
    }

    /// A copy of the counters
    #[must_use]
    pub fn snapshot(&self) -> UsageSnapshot {
        self.state.snapshot()
    }

    /// Returns the ledger allocation's identity token without dereferencing it.
    #[inline]
    pub(crate) fn account_address(&self) -> usize {
        self.state.account_address()
    }

    /// Charge `bytes` of live memory in `category`.
    ///
    /// One step: admit and commit. The allocator already knows the exact size, so there is nothing to reserve first.
    ///
    /// # Errors
    ///
    /// The memory ceiling, overflow, or an accounting bug.
    #[inline]
    pub fn charge_residency(&self, bytes: u64, category: MemoryCategory) -> Result<(), ResourceError> {
        self.state.charge_residency(category, bytes)
    }

    /// Give back `bytes` previously charged in `category`.
    ///
    /// Over-release is an accounting bug: debug builds panic and release builds clamp to the category's current
    /// residency.
    pub fn release_residency(&self, category: MemoryCategory, bytes: u64) {
        self.state.release_residency(category, bytes);
    }

    /// Reads the current committed bytes in `category` without building a snapshot. The ambient release clamp is the
    /// only caller.
    #[inline]
    pub(crate) fn current_residency(&self, category: MemoryCategory) -> u64 {
        self.state.current_residency(category)
    }

    /// Records `bytes` of input the request accepted. Input bytes are a logical count, not a memory admission, so the
    /// probe switch does not silence it.
    ///
    /// # Errors
    ///
    /// Returns a resource error when the input ceiling is exhausted, or an accounting invariant failure on a worker
    /// child (the parent owns input).
    #[inline]
    pub(crate) fn charge_input(&self, bytes: u64) -> Result<(), ResourceError> {
        if self.state.limits.is_task() {
            return Err(ResourceError::AccountingInvariantViolation);
        }
        self.state.charge_logical(ResourceLimit::InputBytes, bytes)
    }

    /// Count spill-file bytes. A `0` ceiling is unset — the charge still records. The probe switch skips this charge.
    ///
    /// # Errors
    ///
    /// Returns a resource error when the disk ceiling is set and exhausted, or an accounting invariant failure on a
    /// worker child.
    #[inline]
    pub(crate) fn charge_spill_disk(&self, bytes: u64) -> Result<(), ResourceError> {
        if self.state.limits.is_task() {
            return Err(ResourceError::AccountingInvariantViolation);
        }
        if self.state.limits.probe_unaccounted() {
            return Ok(());
        }
        self.state.charge_logical(ResourceLimit::SpillDiskBytes, bytes)
    }
}

/// Handle to one heap-allocated ledger. Cloning adds a reference; the last drop frees it.
pub(crate) struct AccountHandle {
    pointer: NonNull<AccountBox>,
    request_local: PhantomData<Rc<()>>,
}

/// A reservation to publish some output. Drop it to give the bytes back; [`OutputPermit::commit`] to keep the prefix
/// that actually went out.
///
/// ```compile_fail use jqf_resource::OutputPermit; fn assert_send<T: Send>() {} assert_send::<OutputPermit>();
/// ```
#[must_use = "dropping an output permit rolls back its unpublished authority"]
pub struct OutputPermit<'a> {
    state: Option<&'a AccountState>,
    bytes: u64,
}

impl fmt::Debug for OutputPermit<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OutputPermit")
            .field("reserved_bytes", &self.bytes)
            .field("armed", &self.state.is_some())
            .finish()
    }
}

impl<'a> OutputPermit<'a> {
    #[inline]
    pub(crate) const fn new(state: &'a AccountState, bytes: u64) -> Self {
        Self {
            state: Some(state),
            bytes,
        }
    }

    /// A permit that does not touch the ledger: probe and task accounts cannot charge output, so constructing a handle
    /// and incrementing reserved bytes is wasted work on every published item.
    #[inline]
    pub(crate) const fn inert(bytes: u64) -> Self {
        Self { state: None, bytes }
    }

    /// How many bytes this permit covers.
    #[must_use]
    #[inline]
    pub const fn reserved_bytes(&self) -> u64 {
        self.bytes
    }

    /// Keep `published_bytes` and give the rest of the reservation back.
    ///
    /// An inert permit (probe or task) still refuses a publish past its reserved prefix — that is a caller bug, not a
    /// mode difference — and otherwise commits as a no-op.
    ///
    /// # Errors
    ///
    /// Returns an accounting error if `published_bytes` exceeds the reserved prefix or the request ledger rejects the
    /// checked transition.
    #[inline]
    pub fn commit(mut self, published_bytes: u64) -> Result<(), ResourceError> {
        if published_bytes > self.bytes {
            return Err(ResourceError::OutputPermitExceeded {
                reserved: self.bytes,
                published: published_bytes,
            });
        }
        let Some(state) = self.state.take() else {
            return Ok(());
        };
        state.commit_output_reservation(self.bytes, published_bytes)
    }
}

impl Drop for OutputPermit<'_> {
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            state.release_output_reservation(self.bytes);
        }
    }
}

impl fmt::Debug for AccountHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("AccountHandle").field("state", &**self).finish()
    }
}

impl AccountHandle {
    /// Allocate a ledger under `limits` and book that allocation as Retained on the new ledger, not on whoever is
    /// already ambient.
    fn try_new(limits: ResourceLimits) -> Result<Self, ResourceError> {
        let bytes = Self::allocation_bytes()?;
        Self::admit_allocation(limits, bytes)?;
        let pointer = Self::allocate_box(limits, Self::usage_with_retained(bytes))?;
        Ok(Self {
            pointer,
            request_local: PhantomData,
        })
    }

    fn allocation_bytes() -> Result<u64, ResourceError> {
        u64::try_from(Layout::new::<AccountBox>().size()).map_err(|_| ResourceError::ArithmeticOverflow)
    }

    fn admit_allocation(limits: ResourceLimits, bytes: u64) -> Result<(), ResourceError> {
        if bytes > limits.max_memory_bytes() {
            return Err(ResourceError::LimitExceeded {
                limit_kind: ResourceLimit::MemoryBytes,
                limit: limits.max_memory_bytes(),
                current: 0,
                requested_delta: bytes,
            });
        }
        if let Some((_, task_limit)) = limits.task_memory_group(MemoryCategory::Retained)
            && bytes > task_limit
        {
            return Err(ResourceError::LimitExceeded {
                limit_kind: ResourceLimit::MemoryBytes,
                limit: task_limit,
                current: 0,
                requested_delta: bytes,
            });
        }
        Ok(())
    }

    fn usage_with_retained(bytes: u64) -> UsageCells {
        let usage = UsageCells::default();
        let retained = MemoryCategory::Retained.index();
        usage.memory[retained].current.set(bytes);
        usage.memory[retained].peak.set(bytes);
        usage.memory_current_bytes.set(bytes);
        usage.memory_peak_bytes.set(bytes);
        usage
    }

    fn allocate_box(limits: ResourceLimits, usage: UsageCells) -> Result<NonNull<AccountBox>, ResourceError> {
        let layout = Layout::new::<AccountBox>();
        // SAFETY: `layout` is non-zero and valid for `AccountBox`. A non-null allocation is
        // initialized exactly once below and deallocated with the same layout when the intrusive reference count
        // reaches zero. The new ledger books these bytes as Retained itself; the ambient charge would land on whatever
        // account is already installed (the parent, when a worker opens a child).
        let raw = crate::ambient::without_ledger_release(|| unsafe { alloc(layout) });
        #[expect(
            clippy::cast_ptr_alignment,
            reason = "alloc returns storage aligned for the supplied AccountBox layout"
        )]
        let typed = raw.cast::<AccountBox>();
        let Some(pointer) = NonNull::new(typed) else {
            return Err(ResourceError::AllocationFailed);
        };
        // SAFETY: `pointer` names uniquely owned, properly aligned storage for one `AccountBox` and
        // has not yet been initialized.
        unsafe {
            pointer.as_ptr().write(AccountBox {
                references: Cell::new(1),
                state: AccountState {
                    limits,
                    usage,
                    tripped: Cell::new(None),
                },
            });
        };
        Ok(pointer)
    }

    /// Add one handle. The last drop frees the allocation.
    #[inline]
    pub(crate) fn try_clone(&self) -> Result<Self, ResourceError> {
        let current = self.box_ref().references.get();
        let next = current.checked_add(1).ok_or(ResourceError::ArithmeticOverflow)?;
        self.box_ref().references.set(next);
        Ok(Self {
            pointer: self.pointer,
            request_local: PhantomData,
        })
    }

    /// Address of the ledger allocation, for identity compares without reading cells.
    #[inline]
    pub(crate) fn account_address(&self) -> usize {
        self.pointer.as_ptr() as usize
    }

    #[inline]
    fn box_ref(&self) -> &AccountBox {
        // SAFETY: every live handle contributes one intrusive reference, so the allocation remains
        // initialized for this borrow.
        unsafe { self.pointer.as_ref() }
    }
}

impl Deref for AccountHandle {
    type Target = AccountState;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.box_ref().state
    }
}

impl Drop for AccountHandle {
    fn drop(&mut self) {
        let current = self.box_ref().references.get();
        // A live handle always holds one intrusive reference; a zero count is a prior accounting bug and the
        // checked_sub below would silently leak the allocation in release. Trip the debug build instead.
        debug_assert!(current >= 1, "account handle dropped with zero references");
        let Some(next) = current.checked_sub(1) else {
            return;
        };
        if next != 0 {
            self.box_ref().references.set(next);
            return;
        }
        let layout = Layout::new::<AccountBox>();
        // SAFETY: this is the final intrusive handle. The allocation was initialized by `try_new`,
        // no references remain, and deallocation uses the identical layout. The Retained charge lives on this ledger,
        // which is dying; do not `release` against the restored ambient account.
        crate::ambient::without_ledger_release(|| unsafe {
            ptr::drop_in_place(self.pointer.as_ptr());
            dealloc(self.pointer.as_ptr().cast::<u8>(), layout);
        });
    }
}

/// The cells and policy one ledger owns.
pub(crate) struct AccountState {
    limits: ResourceLimits,
    usage: UsageCells,
    /// The error a refused ambient charge latched, terminal for this ledger's life. Allocator admission writes it; a
    /// worker's detach reads it, so the trip travels with the ledger, not the thread.
    tripped: Cell<Option<ResourceError>>,
}

impl fmt::Debug for AccountState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AccountState")
            .field("limits", &self.limits)
            .field("usage", &self.snapshot())
            .field("probe_unaccounted", &self.limits.probe_unaccounted())
            .finish_non_exhaustive()
    }
}

#[inline]
const fn clamped_residency_after_release(category_current: u64, total_current: u64, bytes: u64) -> (u64, u64) {
    let released = if bytes < category_current {
        bytes
    } else {
        category_current
    };
    (category_current - released, total_current - released)
}

impl AccountState {
    #[inline]
    fn charge_residency(&self, category: MemoryCategory, requested_delta: u64) -> Result<(), ResourceError> {
        if self.limits.probe_unaccounted() {
            return Ok(());
        }
        let current = self.usage.memory_current_bytes.get();
        let attempted = current
            .checked_add(requested_delta)
            .ok_or(ResourceError::ArithmeticOverflow)?;
        let limit = self.limits.max_memory_bytes();
        if attempted > limit {
            return Err(ResourceError::LimitExceeded {
                limit_kind: ResourceLimit::MemoryBytes,
                limit,
                current,
                requested_delta,
            });
        }
        self.admit_task_memory_group(category, requested_delta)?;
        let index = category.index();
        let category_current = self.usage.memory[index]
            .current
            .get()
            .checked_add(requested_delta)
            .ok_or(ResourceError::AccountingInvariantViolation)?;
        self.commit_current_and_peak(index, category_current, attempted);
        Ok(())
    }

    #[inline]
    pub(crate) fn release_residency(&self, category: MemoryCategory, bytes: u64) {
        if self.limits.probe_unaccounted() {
            return;
        }
        let index = category.index();
        // The profile-dependent over-release contract is on `RequestAccount::release_residency`.
        debug_assert!(self.usage.memory[index].current.get() >= bytes);
        debug_assert!(self.usage.memory_current_bytes.get() >= bytes);
        let category_current = self.usage.memory[index].current.get();
        let total_current = self.usage.memory_current_bytes.get();
        // One clamp for both cells — the same clamp the ambient release applies before calling here. A raw
        // saturating_sub on the TOTAL would let an over-release drive it below the ledger's own booked bytes and hand
        // the request phantom ceiling headroom.
        let (category_current, total_current) = clamped_residency_after_release(category_current, total_current, bytes);
        self.usage.memory[index].current.set(category_current);
        self.usage.memory_current_bytes.set(total_current);
    }

    /// Latches a refused ambient charge on this ledger. Nothing clears it: a tripped ledger is terminal.
    #[inline]
    pub(crate) fn note_trip(&self, error: ResourceError) {
        self.tripped.set(Some(error));
    }

    /// The trip latched on this ledger, if any.
    #[inline]
    pub(crate) fn latched_trip(&self) -> Option<ResourceError> {
        self.tripped.get()
    }

    #[inline]
    pub(crate) fn current_residency(&self, category: MemoryCategory) -> u64 {
        self.usage.memory[category.index()].current.get()
    }

    #[inline]
    fn admit_task_memory_group(&self, category: MemoryCategory, requested_delta: u64) -> Result<(), ResourceError> {
        let Some((group, limit)) = self.limits.task_memory_group(category) else {
            return Ok(());
        };
        let category_usage = |category: MemoryCategory| self.usage.memory[category.index()].current.get();
        let current = match group {
            TaskMemoryGroup::Retained => category_usage(MemoryCategory::Retained),
            TaskMemoryGroup::Working => {
                // Diagnostic residency is usually empty on the publish path; skip the checked add when it holds
                // nothing.
                let working = category_usage(MemoryCategory::Working);
                let diagnostic = category_usage(MemoryCategory::Diagnostic);
                if diagnostic == 0 {
                    working
                } else {
                    working
                        .checked_add(diagnostic)
                        .ok_or(ResourceError::ArithmeticOverflow)?
                }
            }
            TaskMemoryGroup::PendingIo => category_usage(MemoryCategory::PendingIo),
        };
        let attempted = current
            .checked_add(requested_delta)
            .ok_or(ResourceError::ArithmeticOverflow)?;
        if attempted > limit {
            return Err(ResourceError::LimitExceeded {
                limit_kind: ResourceLimit::MemoryBytes,
                limit,
                current,
                requested_delta,
            });
        }
        Ok(())
    }

    /// Writes one committed category and aggregate current/peak. Callers have already validated every checked result
    /// into locals.
    #[inline]
    fn commit_current_and_peak(&self, index: usize, category_current: u64, total_current: u64) {
        self.usage.memory[index].current.set(category_current);
        let peak = &self.usage.memory[index].peak;
        if category_current > peak.get() {
            peak.set(category_current);
        }
        self.usage.memory_current_bytes.set(total_current);
        let peak = &self.usage.memory_peak_bytes;
        if total_current > peak.get() {
            peak.set(total_current);
        }
    }

    #[inline]
    fn charge_logical(&self, limit_kind: ResourceLimit, requested_delta: u64) -> Result<(), ResourceError> {
        let (cell, limit) = match limit_kind {
            ResourceLimit::InputBytes => (&self.usage.input_bytes, self.limits.max_input_bytes()),
            ResourceLimit::SpillDiskBytes => (
                &self.usage.spill_disk_bytes,
                // A `0` disk ceiling is UNSET (the default), not a refusal of every byte: the request never configured
                // one, so the cumulative footprint is still recorded and the admission always succeeds. Every other
                // dimension reads 0 as a hard zero, which is why the exception lives in this arm alone.
                match self.limits.max_spill_disk_bytes() {
                    0 => u64::MAX,
                    ceiling => ceiling,
                },
            ),
            ResourceLimit::OutputBytes | ResourceLimit::MemoryBytes | ResourceLimit::NestingDepth => {
                // The wrong entry point: these dimensions have their own admission paths, so reaching the generic
                // logical charger is an internal contract violation.
                return Err(ResourceError::AccountingInvariantViolation);
            }
        };
        let current = cell.get();
        let attempted = current
            .checked_add(requested_delta)
            .ok_or(ResourceError::ArithmeticOverflow)?;
        if attempted > limit {
            return Err(ResourceError::LimitExceeded {
                limit_kind,
                limit,
                current,
                requested_delta,
            });
        }
        cell.set(attempted);
        Ok(())
    }

    #[inline]
    pub(crate) fn reserve_output(
        handle: &AccountHandle,
        requested_delta: u64,
    ) -> Result<OutputPermit<'_>, ResourceError> {
        let this = &**handle;
        // A worker child's publishable bytes are bounded by its grant's output authority (the task output buffer), and
        // the publish path reserves a permit on whatever context drives it — so a task account issues an inert
        // permit, exactly like probe mode. Refusing here fails every parallel morsel at its first published byte and
        // silently degrades the request to the serial rerun.
        if this.limits.is_task() || this.limits.probe_unaccounted() {
            return Ok(OutputPermit::inert(requested_delta));
        }
        let current = this
            .usage
            .output_bytes
            .get()
            .checked_add(this.usage.output_reserved_bytes.get())
            .ok_or(ResourceError::ArithmeticOverflow)?;
        let attempted = current
            .checked_add(requested_delta)
            .ok_or(ResourceError::ArithmeticOverflow)?;
        let limit = this.limits.max_output_bytes();
        if attempted > limit {
            return Err(ResourceError::LimitExceeded {
                limit_kind: ResourceLimit::OutputBytes,
                limit,
                current,
                requested_delta,
            });
        }
        let reserved = this
            .usage
            .output_reserved_bytes
            .get()
            .checked_add(requested_delta)
            .ok_or(ResourceError::ArithmeticOverflow)?;
        this.usage.output_reserved_bytes.set(reserved);
        Ok(OutputPermit::new(this, requested_delta))
    }

    #[inline]
    pub(crate) fn release_output_reservation(&self, bytes: u64) {
        // Probe mode and task accounts issue permits without charging the ledger, so release is a deliberate no-op
        // there, exactly like its reservation and residency siblings.
        if self.limits.is_task() || self.limits.probe_unaccounted() {
            return;
        }
        // Release is infallible in release by design (commit is the fallible direction); a double-release must trip the
        // debug build instead of silently freezing the ledger over-counted.
        debug_assert!(self.usage.output_reserved_bytes.get() >= bytes);
        let Some(reserved) = self.usage.output_reserved_bytes.get().checked_sub(bytes) else {
            return;
        };
        self.usage.output_reserved_bytes.set(reserved);
    }

    #[inline]
    pub(crate) fn commit_output_reservation(&self, reserved: u64, published: u64) -> Result<(), ResourceError> {
        // The permit invariant holds in every mode: publishing more than was reserved is the caller lying about what it
        // wrote, and probe runs must surface that bug, not hide it.
        if published > reserved {
            return Err(ResourceError::OutputPermitExceeded { reserved, published });
        }
        // Probe mode and task accounts issue permits without charging the ledger, so commit is a deliberate no-op
        // there, exactly like its reservation and residency siblings (such a permit carries its reserved bytes while
        // `output_reserved_bytes` never grew, so the checked subtraction below would otherwise underflow into an
        // accounting violation).
        if self.limits.is_task() || self.limits.probe_unaccounted() {
            return Ok(());
        }
        let remaining_reserved = self
            .usage
            .output_reserved_bytes
            .get()
            .checked_sub(reserved)
            .ok_or(ResourceError::AccountingInvariantViolation)?;
        let total_published = self
            .usage
            .output_bytes
            .get()
            .checked_add(published)
            .ok_or(ResourceError::ArithmeticOverflow)?;
        self.usage.output_reserved_bytes.set(remaining_reserved);
        self.usage.output_bytes.set(total_published);
        Ok(())
    }

    #[inline]
    pub(crate) fn enter_depth(handle: &AccountHandle) -> Result<DepthGuard<'_>, ResourceError> {
        handle.increment_depth()?;
        Ok(DepthGuard::new(handle))
    }

    pub(crate) fn enter_depth_owned(handle: &AccountHandle) -> Result<crate::OwnedDepthGuard, ResourceError> {
        let guard_handle = handle.try_clone()?;
        handle.increment_depth()?;
        Ok(crate::OwnedDepthGuard::new(guard_handle))
    }

    fn increment_depth(&self) -> Result<(), ResourceError> {
        let current = self.usage.nesting_depth.get();
        let depth = current.checked_add(1).ok_or(ResourceError::ArithmeticOverflow)?;
        let limit = self.limits.max_nesting_depth();
        if depth > limit {
            return Err(ResourceError::LimitExceeded {
                limit_kind: ResourceLimit::NestingDepth,
                limit: u64::from(limit),
                current: u64::from(current),
                requested_delta: 1,
            });
        }
        self.usage.nesting_depth.set(depth);
        let peak = &self.usage.nesting_peak;
        if depth > peak.get() {
            peak.set(depth);
        }
        Ok(())
    }

    #[inline]
    pub(crate) fn leave_depth(&self) {
        // Release is infallible in release by design (commit is the fallible direction); leaving an un-entered depth
        // must trip the debug build instead of silently ignoring the imbalance.
        debug_assert!(
            self.usage.nesting_depth.get() >= 1,
            "leave_depth without a matching enter_depth"
        );
        let Some(depth) = self.usage.nesting_depth.get().checked_sub(1) else {
            return;
        };
        self.usage.nesting_depth.set(depth);
    }

    fn snapshot(&self) -> UsageSnapshot {
        let usage = &self.usage;
        let memory = core::array::from_fn(|index| MemoryUsage {
            current: usage.memory[index].current.get(),
            peak: usage.memory[index].peak.get(),
        });
        UsageSnapshot {
            input_bytes: usage.input_bytes.get(),
            output_bytes: usage.output_bytes.get(),
            output_reserved_bytes: usage.output_reserved_bytes.get(),
            spill_disk_bytes: usage.spill_disk_bytes.get(),
            memory,
            memory_current_bytes: usage.memory_current_bytes.get(),
            memory_peak_bytes: usage.memory_peak_bytes.get(),
            nesting_depth: usage.nesting_depth.get(),
            nesting_peak: usage.nesting_peak.get(),
        }
    }
}

struct AccountBox {
    references: Cell<usize>,
    state: AccountState,
}

/// Per-field ledger cells so a charge touches only the words it writes.
///
/// Multi-field updates compute every checked result into locals first and write cells only after all checks pass.
/// Current and peak for one category sit in one pair so a charge/peak update is one cache line.
#[derive(Default)]
struct CategoryCells {
    current: Cell<u64>,
    peak: Cell<u64>,
}

#[derive(Default)]
struct UsageCells {
    memory_current_bytes: Cell<u64>,
    memory_peak_bytes: Cell<u64>,
    memory: [CategoryCells; MemoryCategory::COUNT],
    input_bytes: Cell<u64>,
    output_bytes: Cell<u64>,
    output_reserved_bytes: Cell<u64>,
    spill_disk_bytes: Cell<u64>,
    nesting_depth: Cell<u32>,
    nesting_peak: Cell<u32>,
}

#[cfg(test)]
mod tests {
    use super::{AccountState, RequestAccount, clamped_residency_after_release};
    use crate::{MemoryCategory, ResourceError, ResourceLimit, ResourceLimits};

    impl RequestAccount {
        fn force_residency(&self, bytes: u64, category: MemoryCategory) {
            self.state.force_residency(category, bytes);
        }
    }

    impl AccountState {
        fn force_residency(&self, category: MemoryCategory, requested_delta: u64) {
            if self.limits.probe_unaccounted() {
                return;
            }
            let index = category.index();
            let category_current = self.usage.memory[index].current.get().saturating_add(requested_delta);
            let total_current = self.usage.memory_current_bytes.get().saturating_add(requested_delta);
            self.commit_current_and_peak(index, category_current, total_current);
        }
    }

    fn account(memory: u64) -> RequestAccount {
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, memory, u64::MAX, u32::MAX)).expect("account")
    }

    #[test]
    fn a_new_account_books_its_own_ledger_as_retained() {
        let account = account(u64::MAX);
        let snapshot = account.snapshot();
        let ledger = RequestAccount::minimum_memory_bytes();
        assert_eq!(snapshot.memory(MemoryCategory::Retained).current(), ledger);
        assert_eq!(snapshot.memory_current_bytes(), ledger);
        assert_eq!(snapshot.memory(MemoryCategory::Working).current(), 0);
    }

    #[test]
    fn creating_an_account_past_the_memory_ceiling_refuses() {
        let error = RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, 8, u64::MAX, 1))
            .expect_err("8 bytes cannot hold the ledger");
        match error {
            ResourceError::LimitExceeded {
                limit_kind,
                limit,
                current,
                requested_delta,
            } => {
                assert_eq!(limit_kind, ResourceLimit::MemoryBytes);
                assert_eq!(limit, 8);
                assert_eq!(current, 0);
                assert_eq!(requested_delta, RequestAccount::minimum_memory_bytes());
            }
            other => panic!("expected a memory-limit error, got {other:?}"),
        }
    }

    #[test]
    fn a_shared_handle_is_the_same_ledger() {
        let account = account(u64::MAX);
        let share = account.try_share().expect("share");
        account.charge_residency(64, MemoryCategory::Working).expect("charge");
        assert_eq!(share.snapshot().memory(MemoryCategory::Working).current(), 64);
        share.release_residency(MemoryCategory::Working, 64);
        assert_eq!(account.snapshot().memory(MemoryCategory::Working).current(), 0);
    }

    #[test]
    fn charge_residency_refuses_past_the_ceiling_and_release_returns_headroom() {
        let ledger = RequestAccount::minimum_memory_bytes();
        let account = account(ledger + 64);
        account
            .charge_residency(64, MemoryCategory::Working)
            .expect("fills the ceiling");
        assert!(account.charge_residency(1, MemoryCategory::Working).is_err());
        account.release_residency(MemoryCategory::Working, 64);
        account
            .charge_residency(64, MemoryCategory::Working)
            .expect("released bytes are spendable again");
    }

    #[test]
    fn the_release_clamp_subtracts_the_same_booked_amount_from_both_totals() {
        let ledger = RequestAccount::minimum_memory_bytes();
        assert_eq!(clamped_residency_after_release(64, ledger + 64, 128), (0, ledger));
    }

    /// Pins [`RequestAccount::release_residency`]'s debug over-release contract.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "current.get()")]
    fn an_over_release_of_residency_trips_debug() {
        let account = account(u64::MAX);
        account.charge_residency(64, MemoryCategory::Working).expect("charge");
        account.release_residency(MemoryCategory::Working, 128);
    }

    #[test]
    #[cfg(not(debug_assertions))]
    fn an_over_release_of_residency_clamps_in_release() {
        let account = account(u64::MAX);
        account.charge_residency(64, MemoryCategory::Working).expect("charge");
        account.release_residency(MemoryCategory::Working, 128);
        let snapshot = account.snapshot();
        assert_eq!(snapshot.memory(MemoryCategory::Working).current(), 0);
        assert_eq!(snapshot.memory_current_bytes(), RequestAccount::minimum_memory_bytes());
    }

    #[test]
    fn force_residency_can_cross_the_ceiling() {
        let ledger = RequestAccount::minimum_memory_bytes();
        let account = account(ledger + 8);
        account.force_residency(64, MemoryCategory::Working);
        assert_eq!(account.snapshot().memory(MemoryCategory::Working).current(), 64);
        assert!(account.snapshot().memory_current_bytes() > ledger + 8);
    }

    #[test]
    fn probe_unaccounted_does_not_move_memory_counters() {
        let limits = ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX).with_probe_unaccounted();
        let account = RequestAccount::try_new(limits).expect("account");
        let before = account.snapshot().memory_current_bytes();
        account
            .charge_residency(1024, MemoryCategory::Working)
            .expect("probe charge");
        assert_eq!(account.snapshot().memory_current_bytes(), before);
        assert_eq!(account.snapshot().memory(MemoryCategory::Working).current(), 0);
    }

    #[test]
    fn a_task_account_refuses_parent_only_charges() {
        let limits = ResourceLimits::task(
            RequestAccount::minimum_memory_bytes() + 128,
            256,
            128,
            RequestAccount::minimum_memory_bytes() + 512,
            64,
        );
        let account = RequestAccount::try_new(limits).expect("task account");
        assert_eq!(
            account.charge_input(1),
            Err(ResourceError::AccountingInvariantViolation)
        );
        assert_eq!(
            account.charge_spill_disk(1),
            Err(ResourceError::AccountingInvariantViolation)
        );
    }

    /// Probe mode switches the ledger charges off, not the caller's own contract: a permit that publishes more than it
    /// reserved is a real accounting bug, and a probe run must surface it, not hide it.
    #[test]
    fn a_probe_permit_still_refuses_publishing_more_than_reserved() {
        let limits = ResourceLimits::new(u64::MAX, 64, u64::MAX, u64::MAX, u32::MAX).with_probe_unaccounted();
        let account = RequestAccount::try_new(limits).expect("account");
        let permit = super::AccountState::reserve_output(&account.state, 64).expect("probe permit");
        let error = permit.commit(65).expect_err("probe mode keeps the permit invariant");
        assert_eq!(
            error,
            ResourceError::OutputPermitExceeded {
                reserved: 64,
                published: 65,
            }
        );
    }

    #[test]
    fn a_task_account_issues_an_inert_output_permit() {
        let limits = ResourceLimits::task(
            RequestAccount::minimum_memory_bytes() + 128,
            256,
            128,
            RequestAccount::minimum_memory_bytes() + 512,
            64,
        );
        let account = RequestAccount::try_new(limits).expect("task account");
        let before = account.snapshot();
        let permit = super::AccountState::reserve_output(&account.state, 32).expect("inert");
        assert_eq!(permit.reserved_bytes(), 32);
        permit.commit(16).expect("inert commit");
        let after = account.snapshot();
        assert_eq!(before.output_bytes(), after.output_bytes());
        assert_eq!(before.output_reserved_bytes(), after.output_reserved_bytes());
    }

    #[test]
    fn an_unset_spill_disk_ceiling_admits_and_counts() {
        let account = account(u64::MAX);
        account.charge_spill_disk(1 << 20).expect("unset admits");
        account.charge_spill_disk(64).expect("unset admits");
        assert_eq!(account.snapshot().spill_disk_bytes(), (1 << 20) + 64);
    }
}
