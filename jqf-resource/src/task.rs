//! Sending one worker a memory budget and getting a result back.
//!
//! The parent reserves the envelope, the worker opens its own ledger from those numbers, and it may send a result only
//! when that ledger is quiet (nothing left but the result buffer). The budget can cross a thread; the reservation
//! cannot. Dropping the result on the parent must not steal the parent's Working count when the child's charge died
//! with the child ledger; a result charged on a still-live ambient ledger releases its
//! [`crate::MemoryCategory::PendingIo`] on drop instead.

use crate::account::AccountHandle;
use crate::allocation::RawAllocation;
use crate::{
    Control, ControlError, MemoryCategory, RequestAccount, ResourceContext, ResourceError, ResourceLimit,
    ResourceLimits, WorkMeter,
};
use core::alloc::Layout;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::ptr;
use core::sync::atomic::{AtomicU64, Ordering};
use std::rc::Rc;
use std::vec::Vec;

static NEXT_TASK_GRANT_ID: AtomicU64 = AtomicU64::new(1);

fn next_grant_id() -> Result<TaskGrantId, ResourceError> {
    NEXT_TASK_GRANT_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| current.checked_add(1))
        .map(TaskGrantId)
        .map_err(|_| ResourceError::ArithmeticOverflow)
}

/// Opaque identity for one worker task grant.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct TaskGrantId(u64);

/// How much memory one worker is allowed, reserved on the parent first.
///
/// All four numbers share the parent's one memory ceiling. The child ledger then splits retained / working /
/// pending-io; the result buffer is also capped by `output_bytes`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskGrantLimits {
    /// Child ledger, provider, schema, plan, and retained-value bytes.
    pub retained_bytes: u64,
    /// Parser, evaluator, encoder, and other transient working bytes.
    pub working_bytes: u64,
    /// In-progress source and sink buffer bytes excluding the result.
    pub pending_io_bytes: u64,
    /// Maximum exact capacity of the detachable result allocation.
    pub output_bytes: u64,
}

impl TaskGrantLimits {
    /// Returns the checked aggregate memory envelope.
    ///
    /// # Errors
    ///
    /// Returns an arithmetic error when the four components do not fit in `u64`.
    pub fn total_memory_bytes(self) -> Result<u64, ResourceError> {
        self.retained_bytes
            .checked_add(self.working_bytes)
            .and_then(|total| total.checked_add(self.pending_io_bytes))
            .and_then(|total| total.checked_add(self.output_bytes))
            .ok_or(ResourceError::ArithmeticOverflow)
    }
}

/// The parent's hold on one worker's budget.
///
/// Reserving charges the parent so in-flight workers cannot sum past the ceiling. Drop or adopt releases it. The worker
/// still charges its own allocations; this hold is just the reservation.
///
/// ```compile_fail use jqf_resource::task::TaskGrantReservation;
/// fn assert_send<T: Send>() {} assert_send::<TaskGrantReservation>();
/// ```
///
/// ```compile_fail use jqf_resource::task::{DetachedTaskBuffer, TaskGrantReservation};
/// fn adopt_twice(     reservation: TaskGrantReservation,
///     first: DetachedTaskBuffer,     second: DetachedTaskBuffer, ) {     let _ = reservation.adopt(first);
///     let _ = reservation.adopt(second); }
/// ```
#[must_use]
pub struct TaskGrantReservation {
    identity: TaskGrantId,
    limits: TaskGrantLimits,
    charged: u64,
    account: Option<AccountHandle>,
    request_local: PhantomData<Rc<()>>,
}

impl TaskGrantReservation {
    /// Adopts one matching detached allocation into the parent request.
    ///
    /// Grant identity and capacity mismatches fail closed before bytes become parent-owned. The admission hold shrinks
    /// to the adopted result's exact capacity; retained, working, and unused output components are released. The
    /// adopted buffer releases that remaining hold on drop.
    ///
    /// # Errors
    ///
    /// Returns an accounting invariant failure for a forged, crossed, or over-capacity detached result.
    pub fn adopt(mut self, mut detached: DetachedTaskBuffer) -> Result<AdoptedResultBuffer, ResourceError> {
        let capacity = u64::try_from(detached.bytes.capacity()).map_err(|_| ResourceError::ArithmeticOverflow)?;
        if self.identity != detached.receipt.identity
            || detached.receipt.capacity != capacity
            || capacity > self.limits.output_bytes
        {
            return Err(ResourceError::AccountingInvariantViolation);
        }

        let Some(account) = self.account.take() else {
            return Err(ResourceError::AccountingInvariantViolation);
        };
        let keep = capacity.min(self.charged);
        let release = self.charged.saturating_sub(keep);
        if release > 0 {
            account.release_residency(MemoryCategory::Working, release);
        }
        self.charged = 0;
        let charge_died_with_child = detached.charge_died_with_child;
        let bytes = std::mem::take(&mut detached.bytes);
        Ok(AdoptedResultBuffer {
            bytes,
            charged: keep,
            account: Some(account),
            charge_died_with_child,
            request_local: PhantomData,
        })
    }
}

impl Drop for TaskGrantReservation {
    fn drop(&mut self) {
        if let Some(account) = self.account.take()
            && self.charged > 0
        {
            account.release_residency(MemoryCategory::Working, self.charged);
        }
    }
}

/// Numbers a worker opens its own ledger from. `Send`, not `Clone`.
///
/// ```compile_fail use jqf_resource::task::TaskGrantBudget;
/// fn clone_budget(budget: &TaskGrantBudget) {     let _: TaskGrantBudget = budget.clone(); }
/// ```
///
/// ```compile_fail use jqf_resource::task::TaskGrantBudget;
/// fn assert_copy<T: Copy>() {} assert_copy::<TaskGrantBudget>();
/// ```
///
/// ```compile_fail use jqf_resource::task::TaskGrantBudget;
/// fn open_twice(budget: TaskGrantBudget) {     let _first = budget.open_child();
///     let _second = budget.open_child(); }
/// ```
#[must_use = "a task budget must be consumed by one worker or explicitly dropped"]
pub struct TaskGrantBudget {
    identity: TaskGrantId,
    limits: TaskGrantLimits,
    max_nesting_depth: u32,
}

impl TaskGrantBudget {
    /// Consumes the linear budget and creates an independent worker-local request ledger.
    ///
    /// The child retains only numeric limits and grant identity. It has no parent account pointer.
    ///
    /// # Errors
    ///
    /// Returns a resource error when the numeric envelope cannot represent or allocate the child ledger.
    pub fn open_child(self) -> Result<TaskChildAccount, ResourceError> {
        let ledger_bytes = RequestAccount::minimum_memory_bytes();
        if ledger_bytes > self.limits.retained_bytes {
            return Err(ResourceError::LimitExceeded {
                limit_kind: ResourceLimit::MemoryBytes,
                limit: self.limits.retained_bytes,
                current: 0,
                requested_delta: ledger_bytes,
            });
        }
        let total = self.limits.total_memory_bytes()?;
        let pending_and_output = self
            .limits
            .pending_io_bytes
            .checked_add(self.limits.output_bytes)
            .ok_or(ResourceError::ArithmeticOverflow)?;
        let limits = ResourceLimits::task(
            self.limits.retained_bytes,
            self.limits.working_bytes,
            pending_and_output,
            total,
            self.max_nesting_depth,
        );
        let account = RequestAccount::try_new(limits)?;
        Ok(TaskChildAccount {
            account,
            identity: self.identity,
            output_limit: self.limits.output_bytes,
            request_local: PhantomData,
        })
    }
}

/// The worker's own ledger, opened from a [`TaskGrantBudget`].
///
/// ```compile_fail use jqf_resource::task::TaskChildAccount;
/// fn assert_send<T: Send>() {} assert_send::<TaskChildAccount>();
/// ```
pub struct TaskChildAccount {
    account: RequestAccount,
    identity: TaskGrantId,
    output_limit: u64,
    request_local: PhantomData<Rc<()>>,
}

impl TaskChildAccount {
    /// Installs a SHARED handle to this child ledger as the current thread's ambient account for the returned guard's
    /// lifetime.
    ///
    /// The counting allocator charges every allocation this thread makes against the child ledger while the guard is
    /// live, so a worker installs its grant's child ledger at worker start and the memory ceiling binds its
    /// allocations. `self` keeps its own handle for [`Self::bind`]; both handles name the same ledger, so the ambient
    /// allocator and the worker's `ResourceContext` read one `AccountState`. The guard's drop restores the previous
    /// ambient account (none on a fresh thread) and drops the shared handle — the ledger dies with its charges when
    /// the last handle goes.
    ///
    /// # Errors
    ///
    /// Returns an arithmetic error if the shared account reference cannot be retained (practically infallible).
    pub fn install_ambient(&self) -> Result<crate::ScopeGuard, ResourceError> {
        Ok(crate::ambient::install(self.account.try_share()?))
    }

    /// Bind this ledger to worker-local control and work budget.
    ///
    /// Builds a fresh [`ResourceContext`]. Mismatch, strictness, sinks, seed, and env start at defaults.
    ///
    /// # Errors
    ///
    /// Returns a control error if cancellation, deadline expiry, or the physical-memory ceiling is already visible.
    pub fn bind(self, control: &dyn Control, work: WorkMeter) -> Result<TaskChildContext<'_>, ControlError> {
        let Self {
            account,
            identity,
            output_limit,
            request_local: _,
        } = self;
        let bound_account_address = account.account_address();
        Ok(TaskChildContext {
            resources: ResourceContext::new(account, control, work)?,
            identity,
            bound_account_address,
            output_authority: Some(TaskOutputAuthority { identity, output_limit }),
            request_local: PhantomData,
        })
    }
}

/// The worker's own [`ResourceContext`], plus the grant it was given.
///
/// Deref to the context. Same methods as the parent request, but knobs (mismatch, strictness, sinks, seed, env) start
/// at defaults — `bind` builds a fresh context.
///
/// ```compile_fail use jqf_resource::task::TaskChildContext;
/// fn assert_send<T: Send>() {} assert_send::<TaskChildContext<'static>>();
/// ```
///
/// ```compile_fail use jqf_resource::ResourceContext;
/// fn assert_send<T: Send>() {} assert_send::<ResourceContext<'static>>();
/// ```
pub struct TaskChildContext<'control> {
    resources: ResourceContext<'control>,
    identity: TaskGrantId,
    bound_account_address: usize,
    output_authority: Option<TaskOutputAuthority>,
    request_local: PhantomData<Rc<()>>,
}

impl<'control> Deref for TaskChildContext<'control> {
    type Target = ResourceContext<'control>;

    fn deref(&self) -> &Self::Target {
        &self.resources
    }
}

impl DerefMut for TaskChildContext<'_> {
    /// A `&mut` deref exposes `core::mem::replace` of the inner context — tolerated on purpose. Adoption is guarded
    /// by the grant RECEIPT, not by account identity: a swapped inner context charges its own account and the matching
    /// reservation still adopts. The contract is pinned by
    /// `adoption_is_guarded_by_the_grant_receipt_not_the_inner_account` in `tests/cases/grants.rs`.
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.resources
    }
}

impl TaskChildContext<'_> {
    /// Consumes a quiescent worker context and its sole result allocation, then returns a sendable detached carrier.
    ///
    /// Detachment is permitted only after every provider, document, owned value, reservation, depth guard, and scratch
    /// allocation has been dropped. The child ledger itself is dropped before this method returns, so the coordinator
    /// can never adopt a result while the worker can still spend the same numeric envelope.
    ///
    /// # Errors
    ///
    /// A trip latched on this child ledger, leftover output authority, a grant-id or capacity mismatch, or live memory
    /// above the ledger plus this result's capacity.
    ///
    /// The pre-checks are best-effort when the inner context was replaced through `DerefMut`: they then examine
    /// whatever ledger was swapped in. The binding check is the coordinator's receipt at `adopt`, which verifies the
    /// capacity against the reserved envelope.
    pub fn detach_result(self, output: TaskOutputBuffer) -> Result<DetachedTaskBuffer, ResourceError> {
        // A tripped worker's slice must not cross a thread: the emergency slab may have backed the last allocations,
        // and publishing those bytes would be a silent success past the envelope. The latch consulted is the CHILD
        // ledger's own — the refused charge lives there, and a trip latched by an unrelated account on this thread
        // must not poison this detach (nor the reverse).
        if let Some(latched) = self.account().state.latched_trip() {
            return Err(latched);
        }
        if self.output_authority.is_some() || output.authority.identity != self.identity {
            return Err(ResourceError::AccountingInvariantViolation);
        }
        let capacity = u64::try_from(output.capacity).map_err(|_| ResourceError::ArithmeticOverflow)?;
        if output.allocation.resident_bytes() != capacity {
            return Err(ResourceError::AccountingInvariantViolation);
        }
        let snapshot = self.snapshot();
        verify_child_quiescence(snapshot, capacity)?;

        let mut detached = output.detach_inner()?;
        // Compare the allocation-time ambient ledger identity with the ledger bound to this worker. The inner context
        // may be replaced through `DerefMut`, so its account is not the worker's identity.
        detached.charge_died_with_child = detached.charged_account.take() == Some(self.bound_account_address);
        // `detach_inner` builds a `Vec` from the live allocation; it does not deallocate, so the ledger is unchanged. A
        // second quiescence check would repeat the same snapshot.
        drop(self);
        Ok(detached)
    }
}

fn verify_child_quiescence(snapshot: crate::UsageSnapshot, output_capacity: u64) -> Result<(), ResourceError> {
    let ledger = RequestAccount::minimum_memory_bytes();
    // The counting allocator is the ONLY memory accountant, so the child ledger's numbers depend on whether a
    // `CountingAlloc` is the process allocator at all — in a plain test binary no allocation ever reaches the ledger.
    // The checks that survive in BOTH worlds are the zero buckets (a retained scratch allocation on a live ambient path
    // is exactly the leak this protocol exists to catch) and a TOTAL bound rather than an equality: the ledger may
    // legitimately hold up to `output_capacity` of `PendingIo` (the output buffer's ambient charge), and nothing else
    // may be live.
    if snapshot.output_reserved_bytes() != 0
        || snapshot.nesting_depth() != 0
        || snapshot.memory(MemoryCategory::Retained).current() != ledger
        || snapshot.memory(MemoryCategory::Working).current() != 0
        || snapshot.memory(MemoryCategory::Diagnostic).current() != 0
        || snapshot.memory_current_bytes()
            > ledger
                .checked_add(output_capacity)
                .ok_or(ResourceError::ArithmeticOverflow)?
    {
        return Err(ResourceError::AccountingInvariantViolation);
    }
    Ok(())
}

/// Bytes the worker built. Detach them to send back to the parent.
pub struct TaskOutputBuffer {
    allocation: RawAllocation,
    len: usize,
    capacity: usize,
    authority: TaskOutputAuthority,
    request_local: PhantomData<Rc<()>>,
}

struct TaskOutputAuthority {
    identity: TaskGrantId,
    output_limit: u64,
}

impl TaskOutputBuffer {
    /// Allocates exact initial result capacity under a child grant.
    ///
    /// # Errors
    ///
    /// Returns a resource error when the output component or aggregate child ledger rejects the exact capacity.
    pub fn try_with_capacity(capacity: usize, child: &mut TaskChildContext<'_>) -> Result<Self, ResourceError> {
        let authority = child
            .output_authority
            .take()
            .ok_or(ResourceError::AccountingInvariantViolation)?;
        let requested = u64::try_from(capacity).map_err(|_| ResourceError::ArithmeticOverflow)?;
        if requested > authority.output_limit {
            let output_limit = authority.output_limit;
            child.output_authority = Some(authority);
            return Err(output_limit_error(output_limit, 0, requested));
        }
        let allocation = match RawAllocation::try_for_array(capacity, MemoryCategory::PendingIo) {
            Ok(allocation) => allocation,
            Err(error) => {
                child.output_authority = Some(authority);
                return Err(error);
            }
        };
        Ok(Self {
            allocation,
            len: 0,
            capacity,
            authority,
            request_local: PhantomData,
        })
    }

    /// Returns the initialized result bytes.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: this owner initializes exactly `0..len`, and `len` never exceeds the allocation's exact capacity.
        unsafe { core::slice::from_raw_parts(self.allocation.pointer().as_ptr(), self.len) }
    }

    /// Returns the initialized byte count.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns whether no bytes are initialized.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns exact allocated capacity.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Append into space this buffer already owns. Does not talk to the ledger: the grant paid for this capacity in
    /// [`TaskOutputBuffer::try_with_capacity`], and the publish path has no context to charge against.
    ///
    /// # Errors
    ///
    /// Returns a limit error when the append would exceed the exact committed capacity — the grant's output component
    /// binding a worker's published bytes. The buffer is unchanged on that path.
    pub fn try_append_within_capacity(&mut self, bytes: &[u8]) -> Result<(), ResourceError> {
        let required = self
            .len
            .checked_add(bytes.len())
            .ok_or(ResourceError::ArithmeticOverflow)?;
        if required > self.capacity {
            let capacity = u64::try_from(self.capacity).map_err(|_| ResourceError::ArithmeticOverflow)?;
            let required = u64::try_from(required).map_err(|_| ResourceError::ArithmeticOverflow)?;
            return Err(output_limit_error(capacity, capacity, required));
        }
        // SAFETY: the committed capacity covers the complete destination range.
        unsafe { self.write_initialized(bytes, required) };
        Ok(())
    }

    /// Copies `bytes` into the initialized tail and advances `len` to `required`. The destination range must already be
    /// owned capacity.
    ///
    /// # Safety
    ///
    /// `self.allocation` must cover `[self.len, required)` as writable, uniquely owned storage that does not overlap
    /// `bytes`.
    unsafe fn write_initialized(&mut self, bytes: &[u8], required: usize) {
        if bytes.is_empty() {
            return;
        }
        // SAFETY: the caller established the destination range and
        // non-overlap; the write initializes exactly the appended prefix.
        unsafe {
            ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                self.allocation.pointer().as_ptr().add(self.len),
                bytes.len(),
            );
        }
        self.len = required;
    }

    fn detach_inner(self) -> Result<DetachedTaskBuffer, ResourceError> {
        let Self {
            allocation,
            len,
            capacity,
            authority,
            request_local: _,
        } = self;
        let (bytes, charged_account) = if capacity == 0 {
            drop(allocation);
            (Vec::new(), None)
        } else {
            let expected = Layout::array::<u8>(capacity).map_err(|_| ResourceError::ArithmeticOverflow)?;
            let (pointer, layout, category, charged_account) = allocation.into_nonzero_parts()?;
            let actual_capacity = layout.size();
            if len > actual_capacity {
                // Dealloc releases the ambient charge in the bucket that admitted it (`PendingIo`), not the thread's
                // current category (the dealloc-side attribution law).
                let _category = crate::ambient::category_scope(category);
                // SAFETY: these parts still carry their original allocation
                // layout and have not been transferred to another owner.
                unsafe { std::alloc::dealloc(pointer.as_ptr(), layout) };
                return Err(ResourceError::AccountingInvariantViolation);
            }
            // SAFETY: `pointer` came from the global allocator with this
            // exact byte layout, and `0..len` is initialized. Constructing the temporary owner first also makes every
            // invariant-failure arm deallocate before releasing the ambient charge.
            let mut bytes = unsafe { Vec::from_raw_parts(pointer.as_ptr(), len, actual_capacity) };
            if layout != expected || category != MemoryCategory::PendingIo {
                let _category = crate::ambient::category_scope(category);
                drop(bytes);
                return Err(ResourceError::AccountingInvariantViolation);
            }
            // Reservation stays the grant ceiling; charge the published prefix only. shrink_to goes through GlobalAlloc
            // (realloc or alloc+copy+dealloc). Those ops must land in PendingIo: the default Working bucket clamps a
            // free that was never charged there, and the unused tail leaks.
            {
                let _category = crate::ambient::category_scope(MemoryCategory::PendingIo);
                if bytes.capacity() > bytes.len() {
                    bytes.shrink_to(bytes.len());
                }
            }
            (bytes, charged_account)
        };
        let capacity = u64::try_from(bytes.capacity()).map_err(|_| ResourceError::ArithmeticOverflow)?;
        Ok(DetachedTaskBuffer {
            bytes,
            receipt: ExactCapacityReceipt {
                identity: authority.identity,
                capacity,
            },
            charged_account,
            charge_died_with_child: false,
        })
    }
}

/// The refusal a task-output allocation hits when the exact grant capacity rejects it.
///
/// Deliberately reports [`ResourceLimit::MemoryBytes`]: the result capacity is a memory-envelope component, so the
/// refusal uses the aggregate memory class.
fn output_limit_error(limit: u64, current: u64, required: u64) -> ResourceError {
    ResourceError::LimitExceeded {
        limit_kind: ResourceLimit::MemoryBytes,
        limit,
        current,
        requested_delta: required.saturating_sub(current),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ExactCapacityReceipt {
    identity: TaskGrantId,
    capacity: u64,
}

/// Result bytes on their way back to the parent.
///
/// Readable after [`TaskGrantReservation::adopt`]. The worker's allocator already charged them; the parent does not
/// charge them again.
pub struct DetachedTaskBuffer {
    bytes: Vec<u8>,
    receipt: ExactCapacityReceipt,
    charged_account: Option<usize>,
    /// The ambient charge for `bytes` was on the (now dead) child ledger. Only then may drop suppress the ambient
    /// release; a result charged on a still-live ledger must release, or its [`crate::MemoryCategory::PendingIo`]
    /// leaks.
    charge_died_with_child: bool,
}

impl DetachedTaskBuffer {
    /// Returns the initialized byte count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Returns whether no bytes are initialized.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Returns exact allocation capacity covered by the private receipt.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.bytes.capacity()
    }
}

/// Adopted worker result, now owned by the parent.
///
/// ```compile_fail use jqf_resource::task::AdoptedResultBuffer;
/// fn assert_send<T: Send>() {} assert_send::<AdoptedResultBuffer>();
/// ```
pub struct AdoptedResultBuffer {
    bytes: Vec<u8>,
    charged: u64,
    account: Option<AccountHandle>,
    /// Carried over from the detached buffer; see [`DetachedTaskBuffer::charge_died_with_child`].
    charge_died_with_child: bool,
    request_local: PhantomData<Rc<()>>,
}

impl AdoptedResultBuffer {
    /// Returns initialized adopted bytes.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    /// Returns the initialized byte count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Returns whether no bytes are initialized.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Returns exact allocation capacity.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.bytes.capacity()
    }
}

/// Drops the result bytes with the ambient ledger handled per arm: a charge that died with the child ledger is
/// suppressed (releasing it here would steal the landing ledger's bytes), while a charge still on a live ledger
/// releases in the bucket that admitted it (the dealloc-side attribution law).
fn drop_result_bytes(bytes: Vec<u8>, charge_died_with_child: bool) {
    if charge_died_with_child {
        crate::ambient::without_ledger_release(|| drop(bytes));
    } else {
        let _category = crate::ambient::category_scope(MemoryCategory::PendingIo);
        drop(bytes);
    }
}

impl Drop for DetachedTaskBuffer {
    fn drop(&mut self) {
        if self.bytes.capacity() == 0 {
            return;
        }
        let bytes = std::mem::take(&mut self.bytes);
        drop_result_bytes(bytes, self.charge_died_with_child);
    }
}

impl Drop for AdoptedResultBuffer {
    fn drop(&mut self) {
        if self.bytes.capacity() > 0 {
            let bytes = std::mem::take(&mut self.bytes);
            drop_result_bytes(bytes, self.charge_died_with_child);
        }
        if let Some(account) = self.account.take()
            && self.charged > 0
        {
            account.release_residency(MemoryCategory::Working, self.charged);
        }
    }
}

impl ResourceContext<'_> {
    /// Mints one worker envelope before dispatch and returns its coordinator and worker halves.
    ///
    /// The envelope is charged against this account so the parent ceiling binds the sum of in-flight grants. A refusal
    /// means the parent cannot afford another worker: callers shrink the worker count or take the serial path. The
    /// child ledger the worker opens still enforces the same numbers on its own allocations.
    ///
    /// # Errors
    ///
    /// Returns a memory-limit error when the parent cannot afford the envelope, an arithmetic error when the four
    /// components do not sum in `u64`, or when the process-wide grant-identity counter is exhausted.
    pub fn reserve_task_grant(
        &self,
        limits: TaskGrantLimits,
    ) -> Result<(TaskGrantReservation, TaskGrantBudget), ResourceError> {
        let total = limits.total_memory_bytes()?;
        self.account.charge_residency(total, MemoryCategory::Working)?;
        let identity = match next_grant_id() {
            Ok(identity) => identity,
            Err(error) => {
                self.account.release_residency(MemoryCategory::Working, total);
                return Err(error);
            }
        };
        let account = match self.account.state.try_clone() {
            Ok(account) => account,
            Err(error) => {
                self.account.release_residency(MemoryCategory::Working, total);
                return Err(error);
            }
        };

        Ok((
            TaskGrantReservation {
                identity,
                limits,
                charged: total,
                account: Some(account),
                request_local: PhantomData,
            },
            TaskGrantBudget {
                identity,
                limits,
                max_nesting_depth: self.limits().max_nesting_depth(),
            },
        ))
    }
}
