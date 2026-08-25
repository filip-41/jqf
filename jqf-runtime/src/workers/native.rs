//! Scoped native threads backed by portable session permits.
//!
//! [`NativeWorkerHost`] never reads the environment or owns a process-global pool. A scope joins every thread before
//! return. Dropping a live task cancels the whole scope. The worker receives a numeric grant, never the coordinator
//! permit. Sibling of [`super::budget`] and [`super::ordered`].

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::{self, Scope, ScopedJoinHandle, Thread};

use super::{WorkerBudget, WorkerPermit};
use jqf_resource::task::{TaskChildAccount, TaskGrantBudget};
use jqf_resource::{Control, ControlError, ControlOutcome, CooperativeError, ResourceError, tripped};

/// Native session worker host with an explicit permit ceiling.
///
/// The host never reads environment variables or creates a process-global pool. Each [`NativeWorkerHost::scope`]
/// borrows this session-owned budget and joins every native thread before returning.
#[derive(Debug)]
pub struct NativeWorkerHost {
    budget: WorkerBudget,
}

impl NativeWorkerHost {
    /// Creates a native host with exactly `worker_permits` concurrent slots.
    ///
    /// Zero is a valid serial-only configuration.
    #[must_use]
    pub const fn new(worker_permits: usize) -> Self {
        Self {
            budget: WorkerBudget::new(worker_permits),
        }
    }

    /// Runs one native worker scope.
    ///
    /// The operation may lend multiple permits and spawn workers that borrow request-lifetime immutable data. Returning
    /// or unwinding cancels the scope; task handles join before releasing their permits, and Rust's native scope joins
    /// any remaining thread before this method returns.
    pub fn scope<'environment, Output, Operation>(&'environment self, operation: Operation) -> Output
    where
        Operation: for<'scope> FnOnce(&NativeWorkerScope<'scope, 'environment>) -> Output,
    {
        thread::scope(|scope| {
            let workers = NativeWorkerScope {
                scope,
                budget: &self.budget,
                control: NativeWorkerControl::new(),
                wake: FrontierWake::for_current(),
            };
            operation(&workers)
        })
    }
}

/// One native scope borrowing a session's worker permits.
pub struct NativeWorkerScope<'scope, 'environment> {
    scope: &'scope Scope<'scope, 'environment>,
    budget: &'environment WorkerBudget,
    control: NativeWorkerControl,
    wake: Arc<FrontierWake>,
}

impl NativeWorkerScope<'_, '_> {
    /// Returns the permits not currently retained by live native tasks.
    #[must_use]
    pub fn available_permits(&self) -> usize {
        self.budget.available()
    }

    /// Returns whether cancellation has been requested for this scope.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.control.is_cancelled()
    }

    /// Requests cancellation for every worker in this scope.
    pub fn cancel(&self) {
        self.control.cancel();
    }

    /// Latches a typed refusal and cancels every worker in this scope.
    pub fn refuse(&self, error: CooperativeError) {
        self.control.refuse(error);
    }

    pub(crate) fn control(&self) -> NativeWorkerControl {
        self.control.clone()
    }

    pub(crate) fn frontier_wake(&self) -> Arc<FrontierWake> {
        Arc::clone(&self.wake)
    }
}

impl<'scope, 'environment> NativeWorkerScope<'scope, 'environment> {
    /// Borrows one native worker slot without waiting.
    ///
    /// A cancelled scope and an exhausted budget both decline dispatch.
    #[must_use]
    pub fn try_acquire(&self) -> Option<NativeWorkerPermit<'scope, 'environment>> {
        if self.control.is_cancelled() {
            return None;
        }
        Some(NativeWorkerPermit {
            scope: self.scope,
            permit: self.budget.try_acquire()?,
            control: self.control.clone(),
            wake: Arc::clone(&self.wake),
        })
    }
}

impl Drop for NativeWorkerScope<'_, '_> {
    fn drop(&mut self) {
        self.control.cancel();
    }
}

/// One unstarted native worker slot.
///
/// The coordinator acquires this value before moving a linear numeric task grant into the worker. Dropping it without
/// spawning returns the portable permit immediately.
pub struct NativeWorkerPermit<'scope, 'environment> {
    scope: &'scope Scope<'scope, 'environment>,
    permit: WorkerPermit<'environment>,
    control: NativeWorkerControl,
    wake: Arc<FrontierWake>,
}

impl<'scope, 'environment> NativeWorkerPermit<'scope, 'environment> {
    /// Spawns one scoped native worker under `grant`.
    ///
    /// The worker receives only the child ledger opened from its linear numeric grant and a native cancellation
    /// observer. Request accounts, documents, providers, encoders, and sinks remain coordinator- or worker-local.
    ///
    /// # Worker ambient accounting
    ///
    /// The spawn closure opens the child ledger from the grant and installs it as this worker thread's ambient account
    /// BEFORE running the worker body, so the counting allocator charges every allocation the worker makes against the
    /// child ledger and the grant's numeric envelope actually binds. The ambient guard drops when the body returns,
    /// restoring the fresh thread's previous (absent) account; the child ledger then dies with its charges — the
    /// envelope bounded them, and there is nothing to fold into the parent (the parent never reserved the envelope).
    ///
    /// # Errors
    ///
    /// Returns the native operating-system thread creation error. The unstarted grant and permit are dropped locally on
    /// that path.
    ///
    /// # Panics
    ///
    /// This method does not panic. A grant whose retained envelope cannot hold the child ledger, or a ledger that
    /// cannot be installed as the thread's ambient account, returns `Err(ResourceError)` from the spawned closure. The
    /// coordinator maps that `Err` to a child-ledger refusal and yields the request to serial.
    pub fn spawn<Output, Worker>(
        self,
        grant: TaskGrantBudget,
        worker: Worker,
    ) -> io::Result<NativeWorkerTask<'scope, 'environment, Result<Output, ResourceError>>>
    where
        Output: Send + 'scope,
        Worker: FnOnce(TaskChildAccount, &NativeWorkerControl) -> Output + Send + 'scope,
    {
        let Self {
            scope,
            permit,
            control,
            wake,
        } = self;
        let worker_control = control.clone();
        let completed = Arc::new(AtomicBool::new(false));
        let notify = NotifyOnDrop {
            completed: Arc::clone(&completed),
            wake,
        };
        // The child ledger is opened ON the worker thread (it is not `Send`), and a grant whose retained envelope
        // cannot host the fixed child ledger is reported as a worker-level failure — the coordinator's "too small"
        // signal, which yields the request to serial — never a panic (a panic inside the closure would take down the
        // join and, with `panic=abort` policies, the process).
        //
        // `NotifyOnDrop` sets `completed` and unparks the coordinator AFTER the body returns or panics, still before
        // the OS thread is marked finished. `is_finished` therefore consults the flag so a park cannot miss the wakeup.
        let handle = thread::Builder::new().spawn_scoped(scope, move || {
            let _notify = notify;
            let child = grant.open_child()?;
            let _ambient = child.install_ambient()?;
            Ok(worker(child, &worker_control))
        })?;
        Ok(NativeWorkerTask {
            handle: Some(handle),
            permit: Some(permit),
            control,
            completed,
        })
    }
}

/// Park/unpark handoff from workers to the coordinator's ordered frontier.
///
/// `thread::unpark` remembers one token, so a worker that finishes before the coordinator parks does not lose the
/// wakeup. The `signaled` flag covers the remaining race: `is_finished` reads it so poll does not park after a body
/// that has returned but whose OS thread has not yet exited.
pub(crate) struct FrontierWake {
    thread: Thread,
    signaled: AtomicBool,
}

impl FrontierWake {
    fn for_current() -> Arc<Self> {
        Arc::new(Self {
            thread: thread::current(),
            signaled: AtomicBool::new(false),
        })
    }

    pub(crate) fn park(&self) {
        while !self.signaled.swap(false, Ordering::Acquire) {
            thread::park();
        }
    }

    fn unpark(&self) {
        self.signaled.store(true, Ordering::Release);
        self.thread.unpark();
    }
}

struct NotifyOnDrop {
    completed: Arc<AtomicBool>,
    wake: Arc<FrontierWake>,
}

impl Drop for NotifyOnDrop {
    fn drop(&mut self) {
        self.completed.store(true, Ordering::Release);
        self.wake.unpark();
    }
}

/// Join handle for one scoped native worker.
///
/// Dropping a live handle first cancels the complete worker scope, then joins this task, and only then returns its
/// session permit.
#[must_use = "a native worker task must be joined or dropped to cancel and join"]
pub struct NativeWorkerTask<'scope, 'environment, Output> {
    handle: Option<ScopedJoinHandle<'scope, Output>>,
    permit: Option<WorkerPermit<'environment>>,
    control: NativeWorkerControl,
    completed: Arc<AtomicBool>,
}

impl<Output> NativeWorkerTask<'_, '_, Output> {
    /// Returns whether the native worker has completed without joining it.
    ///
    /// The coordinator still owns this task's permit and must call [`Self::join`] or drop the task before the permit is
    /// released.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.completed.load(Ordering::Acquire) || self.handle.as_ref().is_none_or(ScopedJoinHandle::is_finished)
    }

    /// Joins the worker without cancelling its sibling tasks.
    ///
    /// A worker panic is returned through the native thread result. The permit is released only after the join
    /// finishes.
    ///
    /// # Errors
    ///
    /// Returns the worker's panic payload when native execution panicked.
    ///
    /// # Panics
    ///
    /// Panics only if the runtime's private one-handle-per-task invariant is violated.
    pub fn join(mut self) -> thread::Result<Output> {
        let result = self.handle.take().expect("a native task owns one join handle").join();
        drop(self.permit.take());
        result
    }
}

impl<Output> Drop for NativeWorkerTask<'_, '_, Output> {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.control.cancel();
            let _ = handle.join();
        }
        drop(self.permit.take());
    }
}

/// Cloneable native cancellation and refusal observer shared by one worker scope.
#[derive(Clone)]
pub struct NativeWorkerControl {
    cancelled: Arc<AtomicBool>,
    refused: Arc<OnceLock<CooperativeError>>,
}

impl core::fmt::Debug for NativeWorkerControl {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("NativeWorkerControl")
            .field("cancelled", &self.is_cancelled())
            .field("refused", &self.refused())
            .finish()
    }
}

impl Default for NativeWorkerControl {
    fn default() -> Self {
        Self::new()
    }
}

impl NativeWorkerControl {
    /// Creates an observer that has not been cancelled.
    #[must_use]
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            refused: Arc::new(OnceLock::new()),
        }
    }

    /// Requests idempotent cancellation.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Latches a typed refusal and cancels the scope.
    ///
    /// The first refusal wins. Later calls still cancel so in-flight workers stop at the next cooperative pause.
    pub fn refuse(&self, error: CooperativeError) {
        let _ = self.refused.set(error);
        self.cancel();
    }

    /// Returns the latched typed refusal, if any.
    #[must_use]
    pub fn refused(&self) -> Option<CooperativeError> {
        self.refused.get().copied()
    }

    /// Returns whether cancellation is currently visible.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub(crate) fn same_scope(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.cancelled, &other.cancelled)
    }
}

impl Control for NativeWorkerControl {
    fn check(&self) -> ControlOutcome {
        if let Some(error) = self.refused() {
            return match error {
                CooperativeError::Control(ControlError::DeadlineExceeded) => ControlOutcome::DeadlineExceeded,
                CooperativeError::Control(ControlError::Cancelled) => ControlOutcome::Cancelled,
                CooperativeError::Control(ControlError::MemoryExceeded) | CooperativeError::Memory(_) => {
                    ControlOutcome::MemoryExceeded
                }
            };
        }
        if self.is_cancelled() {
            return ControlOutcome::Cancelled;
        }
        if tripped() {
            return ControlOutcome::MemoryExceeded;
        }
        ControlOutcome::Continue
    }
}
