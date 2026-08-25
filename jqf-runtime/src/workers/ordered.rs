//! Bounded native record scheduling with deterministic ordinal release.
//!
//! The coordinator is deliberately generic: an ORDINAL names one dispatched unit, and the caller decides what a unit
//! is. It dispatches MORSELS — contiguous record RANGES sized by bytes, never single records — so "record" below reads
//! as "one ordered unit", and `input_bytes` is the morsel's payload span.

use core::fmt;
use core::num::NonZeroU32;
use std::io;

use jqf_resource::task::{AdoptedResultBuffer, DetachedTaskBuffer, TaskGrantLimits, TaskGrantReservation};
use jqf_resource::{ControlError, CooperativeError, ResourceContext, ResourceError, ResourceLimit};

use super::{FrontierWake, NativeWorkerControl, NativeWorkerScope, NativeWorkerTask};

/// Internal scheduling bounds for one ordered record region.
///
/// These are derived from a selected route and request headroom. They are not public resource-limit knobs. Every
/// dimension is nonzero so a selected route can always make progress when one record fits its derived envelope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(
    clippy::struct_field_names,
    reason = "the architecture contract names all three scheduling ceilings with max prefixes"
)]
pub struct RecordConcurrencyWindow {
    max_in_flight_records: NonZeroU32,
    max_in_flight_input_bytes: u64,
    max_reorder_result_bytes: u64,
}

impl RecordConcurrencyWindow {
    /// Creates a nonempty scheduling window.
    #[must_use]
    pub const fn try_new(
        max_in_flight_records: NonZeroU32,
        max_in_flight_input_bytes: u64,
        max_reorder_result_bytes: u64,
    ) -> Option<Self> {
        if max_in_flight_input_bytes == 0 || max_reorder_result_bytes == 0 {
            None
        } else {
            Some(Self {
                max_in_flight_records,
                max_in_flight_input_bytes,
                max_reorder_result_bytes,
            })
        }
    }

    /// The window's ceiling on the sum of in-flight morsel input bytes.
    ///
    /// A single morsel larger than this can never be dispatched; the relay uses the ceiling to refuse it up front
    /// instead of retrying a permanently-full dispatch.
    #[must_use]
    pub const fn max_in_flight_input_bytes(self) -> u64 {
        self.max_in_flight_input_bytes
    }
}

/// Allocation-free validation hooks for caller-owned worker descriptors.
///
/// Runtime deliberately does not depend on an engine or codec descriptor vocabulary. These function pointers let the
/// owning layer validate every byte range against the adopted buffer before an outcome enters the ordinal ring.
pub struct OrderedRecordDescriptorContract<Record, Terminal> {
    validate_record: fn(&Record, u64) -> bool,
    validate_terminal: fn(&Terminal, Option<u64>) -> bool,
}

impl<Record, Terminal> Copy for OrderedRecordDescriptorContract<Record, Terminal> {}

impl<Record, Terminal> Clone for OrderedRecordDescriptorContract<Record, Terminal> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<Record, Terminal> OrderedRecordDescriptorContract<Record, Terminal> {
    /// Creates descriptor validation hooks.
    #[must_use]
    pub const fn new(
        validate_record: fn(&Record, u64) -> bool,
        validate_terminal: fn(&Terminal, Option<u64>) -> bool,
    ) -> Self {
        Self {
            validate_record,
            validate_terminal,
        }
    }
}

/// Result of a nonblocking ordered-record dispatch attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OrderedRecordDispatch {
    /// One native task started with this assigned ordinal.
    Started {
        /// Monotonic record ordinal owned by the coordinator.
        ordinal: u64,
    },
    /// A concurrency-window dimension has no remaining capacity.
    WindowFull,
    /// No native worker permit is currently available.
    NoWorkerPermit,
    /// An earlier output offer is awaiting acknowledgement.
    PausedForOutput,
    /// Dispatch has finished or a terminal/cancellation closed it.
    Closed,
}

/// Closed sendable output of one native record task.
///
/// Descriptors are caller-defined compact values. Dynamic bytes cross the worker boundary only in a detached task
/// buffer and remain unreadable until the coordinator adopts them with the matching reservation.
pub enum OrderedRecordTaskOutput<Record, Terminal> {
    /// One nonterminal record outcome.
    Record {
        /// Closed caller-defined outcome descriptor.
        descriptor: Record,
        /// Matching worker-detached output allocation.
        detached: DetachedTaskBuffer,
    },
    /// One terminal candidate, optionally with dynamic diagnostic bytes.
    Terminal {
        /// Closed caller-defined terminal descriptor.
        descriptor: Terminal,
        /// Optional matching worker-detached diagnostic allocation.
        detached: Option<DetachedTaskBuffer>,
    },
    /// Cooperative cancellation stopped the worker before an outcome existed.
    Cancelled,
    /// The worker breached its envelope or the host stopped the request.
    ///
    /// The reservation is dropped and the slice is discarded: a poisoned result must not be adopted.
    Refused(CooperativeError),
}

impl<Record, Terminal> OrderedRecordTaskOutput<Record, Terminal> {
    /// Creates one nonterminal record output.
    #[must_use]
    pub const fn record(descriptor: Record, detached: DetachedTaskBuffer) -> Self {
        Self::Record { descriptor, detached }
    }

    /// Creates one terminal output.
    #[must_use]
    pub const fn terminal(descriptor: Terminal, detached: Option<DetachedTaskBuffer>) -> Self {
        Self::Terminal { descriptor, detached }
    }

    /// Creates a cooperative-cancellation output.
    #[must_use]
    pub const fn cancelled() -> Self {
        Self::Cancelled
    }

    /// Creates a typed-refusal output. The coordinator drops the reservation and does not adopt any bytes.
    #[must_use]
    pub const fn refused(error: CooperativeError) -> Self {
        Self::Refused(error)
    }
}

/// Borrowed ordered output offered to the caller's sink.
///
/// The coordinator makes no further scheduling progress until the caller acknowledges this offer.
pub struct OrderedRecordReady<'coordinator, Record> {
    ordinal: u64,
    descriptor: &'coordinator Record,
    buffer: &'coordinator AdoptedResultBuffer,
}

impl<'coordinator, Record> OrderedRecordReady<'coordinator, Record> {
    /// Record ordinal of this offer.
    #[must_use]
    pub const fn ordinal(&self) -> u64 {
        self.ordinal
    }

    /// Compact worker outcome descriptor.
    #[must_use]
    pub const fn descriptor(&self) -> &'coordinator Record {
        self.descriptor
    }

    /// Parent-adopted dynamic or encoded bytes.
    #[must_use]
    pub const fn buffer(&self) -> &'coordinator AdoptedResultBuffer {
        self.buffer
    }
}

/// Owned terminal released at its ordered frontier.
///
/// All later native work has already been cancelled, joined, and drained when this value is returned.
pub struct OrderedRecordTerminal<Terminal> {
    ordinal: u64,
    descriptor: Terminal,
    buffer: Option<AdoptedResultBuffer>,
}

impl<Terminal> OrderedRecordTerminal<Terminal> {
    /// Record ordinal at which publication terminates.
    #[must_use]
    pub const fn ordinal(&self) -> u64 {
        self.ordinal
    }

    /// Closed terminal descriptor.
    #[must_use]
    pub const fn descriptor(&self) -> &Terminal {
        &self.descriptor
    }

    /// Optional parent-adopted diagnostic bytes.
    #[must_use]
    pub const fn buffer(&self) -> Option<&AdoptedResultBuffer> {
        self.buffer.as_ref()
    }
}

/// One nonblocking coordinator poll.
pub enum OrderedRecordPoll<'coordinator, Record, Terminal> {
    /// No next-ordinal outcome is ready yet.
    Pending,
    /// One next-ordinal nonterminal outcome is offered.
    Ready(OrderedRecordReady<'coordinator, Record>),
    /// The ordered terminal frontier has been reached and drained.
    Terminal(OrderedRecordTerminal<Terminal>),
    /// Dispatch was finished and every outcome was acknowledged.
    Complete,
}

/// Snapshot of bounded coordinator ownership.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OrderedRecordSnapshot {
    in_flight_records: u32,
    ready_results: u32,
    cancelled_worker_panics: u32,
}

impl OrderedRecordSnapshot {
    /// Dispatched or reorder-resident record count.
    #[must_use]
    pub const fn in_flight_records(self) -> u32 {
        self.in_flight_records
    }

    /// Joined outcomes retained by the ordinal ring.
    #[must_use]
    pub const fn ready_results(self) -> u32 {
        self.ready_results
    }

    /// Number of workers that panicked while being joined on a cancel path.
    ///
    /// `cancel_and_drain` and `release_terminal` discard a joined worker's output — the request yields to serial either
    /// way — but a cancelled worker's panic must stay observable where the primary path (`join_finished`) surfaces it
    /// as `WorkerPanicked`. The count is the tripwire: zero on every clean cancellation, nonzero the moment a worker
    /// panics mid-cancel.
    #[must_use]
    pub const fn cancelled_worker_panics(self) -> u32 {
        self.cancelled_worker_panics
    }
}

/// Failure of coordinator machinery rather than a record's ordered outcome.
#[derive(Debug)]
pub enum OrderedRecordCoordinatorError {
    /// The parent resource ledger rejected coordinator state or a task grant.
    Resource(ResourceError),
    /// The native host had no worker capacity when the coordinator was built.
    NoWorkerCapacity,
    /// A native operating-system thread could not be created.
    Spawn(io::Error),
    /// The worker's child ledger refused the grant (envelope too small, or the ambient install failed). Distinct from
    /// [`Self::Spawn`]: this is accounting, not thread creation, and it must not allocate a formatted string on the
    /// exhaustion path.
    ChildLedger(ResourceError),
    /// A native worker panicked.
    WorkerPanicked,
    /// A worker returned cancellation without an ordered terminal descriptor.
    WorkerCancelled,
    /// The host cancelled the request, the deadline expired, or the physical memory ceiling was exceeded.
    Control(ControlError),
    /// A private coordinator invariant was violated.
    InternalContract(&'static str),
}

impl fmt::Display for OrderedRecordCoordinatorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Resource(error) => write!(formatter, "{error}"),
            Self::NoWorkerCapacity => formatter.write_str("native worker capacity is zero"),
            Self::Spawn(error) => write!(formatter, "native worker spawn failed: {error}"),
            Self::ChildLedger(error) => write!(formatter, "worker child ledger: {error}"),
            Self::WorkerPanicked => formatter.write_str("native record worker panicked"),
            Self::WorkerCancelled => formatter.write_str("native record worker stopped by cancellation"),
            Self::Control(error) => write!(formatter, "{error}"),
            Self::InternalContract(contract) => {
                write!(formatter, "ordered record coordinator contract: {contract}")
            }
        }
    }
}

impl std::error::Error for OrderedRecordCoordinatorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Resource(error) | Self::ChildLedger(error) => Some(error),
            Self::Spawn(error) => Some(error),
            Self::Control(error) => Some(error),
            Self::NoWorkerCapacity | Self::WorkerPanicked | Self::WorkerCancelled | Self::InternalContract(_) => None,
        }
    }
}

impl From<ResourceError> for OrderedRecordCoordinatorError {
    fn from(error: ResourceError) -> Self {
        Self::Resource(error)
    }
}

impl From<ControlError> for OrderedRecordCoordinatorError {
    fn from(error: ControlError) -> Self {
        Self::Control(error)
    }
}

impl From<CooperativeError> for OrderedRecordCoordinatorError {
    fn from(error: CooperativeError) -> Self {
        match error {
            CooperativeError::Memory(error) => Self::Resource(error),
            CooperativeError::Control(error) => Self::Control(error),
        }
    }
}

struct ActiveTask<'scope, 'environment, Record, Terminal> {
    ordinal: u64,
    input_bytes: u64,
    result_grant_bytes: u64,
    reservation: TaskGrantReservation,
    task: NativeWorkerTask<
        'scope,
        'environment,
        Result<OrderedRecordTaskOutput<Record, Terminal>, jqf_resource::ResourceError>,
    >,
}

enum ReadyKind<Record, Terminal> {
    Record(Record),
    Terminal(Terminal),
}

struct ReadySlot<Record, Terminal> {
    ordinal: u64,
    result_grant_bytes: u64,
    kind: ReadyKind<Record, Terminal>,
    buffer: Option<AdoptedResultBuffer>,
}

/// Bounded native task coordinator with a preallocated ordinal ring.
///
/// It allocates exactly two fixed-capacity tracked slot arrays at construction: active task slots and ready outcome
/// slots. Dispatch and ordered insertion do not allocate per-record queue nodes. Worker result allocations are covered
/// by pre-reserved task grants and become readable only after adoption.
pub struct OrderedRecordCoordinator<'scope, 'environment, Record: Copy + Send + Sync, Terminal: Copy + Send + Sync> {
    window: RecordConcurrencyWindow,
    descriptor_contract: OrderedRecordDescriptorContract<Record, Terminal>,
    control: NativeWorkerControl,
    tasks: Vec<Option<ActiveTask<'scope, 'environment, Record, Terminal>>>,
    ready: Vec<Option<ReadySlot<Record, Terminal>>>,
    next_dispatch_ordinal: u64,
    next_release_ordinal: u64,
    in_flight_records: u32,
    in_flight_input_bytes: u64,
    reorder_result_bytes: u64,
    active_tasks: u32,
    ready_results: u32,
    offered_slot: Option<usize>,
    dispatch_finished: bool,
    dispatch_closed: bool,
    complete: bool,
    cancelled_worker_panics: u32,
    wake: std::sync::Arc<FrontierWake>,
}

impl<'scope, 'environment, Record: Copy + Send + Sync, Terminal: Copy + Send + Sync>
    OrderedRecordCoordinator<'scope, 'environment, Record, Terminal>
{
    /// Allocates the fixed task and ordinal rings under the parent request.
    ///
    /// # Errors
    ///
    /// Returns a resource failure when the fixed rings cannot be allocated, or `NoWorkerCapacity` for a serial-only
    /// native scope.
    pub fn try_new(
        scope: &NativeWorkerScope<'scope, 'environment>,
        window: RecordConcurrencyWindow,
        descriptor_contract: OrderedRecordDescriptorContract<Record, Terminal>,
    ) -> Result<Self, OrderedRecordCoordinatorError> {
        if scope.available_permits() == 0 {
            return Err(OrderedRecordCoordinatorError::NoWorkerCapacity);
        }
        let capacity =
            usize::try_from(window.max_in_flight_records.get()).map_err(|_| ResourceError::ArithmeticOverflow)?;
        let mut tasks = Vec::new();
        tasks
            .try_reserve_exact(capacity)
            .map_err(jqf_resource::ResourceError::from)?;
        let mut ready = Vec::new();
        ready
            .try_reserve_exact(capacity)
            .map_err(jqf_resource::ResourceError::from)?;
        for _ in 0..capacity {
            tasks.push(None);
            ready.push(None);
        }
        Ok(Self {
            window,
            descriptor_contract,
            control: scope.control(),
            tasks,
            ready,
            next_dispatch_ordinal: 0,
            next_release_ordinal: 0,
            in_flight_records: 0,
            in_flight_input_bytes: 0,
            reorder_result_bytes: 0,
            active_tasks: 0,
            ready_results: 0,
            offered_slot: None,
            dispatch_finished: false,
            dispatch_closed: false,
            complete: false,
            cancelled_worker_panics: 0,
            wake: scope.frontier_wake(),
        })
    }

    /// Parks the coordinator until a worker unparks the frontier.
    pub(crate) fn park_frontier(&self) {
        self.wake.park();
    }

    /// Returns current bounded ownership counters.
    #[must_use]
    pub const fn snapshot(&self) -> OrderedRecordSnapshot {
        OrderedRecordSnapshot {
            in_flight_records: self.in_flight_records,
            ready_results: self.ready_results,
            cancelled_worker_panics: self.cancelled_worker_panics,
        }
    }

    /// Marks source dispatch complete. Existing tasks and ready outcomes remain ordered and drain normally.
    pub fn finish_dispatch(&mut self) {
        self.dispatch_finished = true;
    }

    /// Starts one native task after reserving every bounded resource.
    ///
    /// # Errors
    ///
    /// Returns a resource, scope-contract, ordinal-overflow, or native thread creation failure. No worker is left
    /// unowned on an error path.
    pub fn try_dispatch<Worker>(
        &mut self,
        scope: &NativeWorkerScope<'scope, 'environment>,
        input_bytes: u64,
        grant_limits: TaskGrantLimits,
        resources: &ResourceContext<'_>,
        worker: Worker,
    ) -> Result<OrderedRecordDispatch, OrderedRecordCoordinatorError>
    where
        Record: 'scope,
        Terminal: 'scope,
        Worker: FnOnce(
                u64,
                jqf_resource::task::TaskChildAccount,
                &NativeWorkerControl,
            ) -> OrderedRecordTaskOutput<Record, Terminal>
            + Send
            + 'scope,
    {
        if !self.control.same_scope(&scope.control()) {
            return self.fail_error(OrderedRecordCoordinatorError::InternalContract(
                "dispatch scope differs from coordinator scope",
            ));
        }
        if self.dispatch_closed || self.dispatch_finished || self.complete || self.control.is_cancelled() {
            return Ok(OrderedRecordDispatch::Closed);
        }
        if self.offered_slot.is_some() {
            return Ok(OrderedRecordDispatch::PausedForOutput);
        }
        let Some(next_records) = self.in_flight_records.checked_add(1) else {
            return self.fail_error(ResourceError::ArithmeticOverflow.into());
        };
        let Some(next_input) = self.in_flight_input_bytes.checked_add(input_bytes) else {
            return self.fail_error(ResourceError::ArithmeticOverflow.into());
        };
        let Some(next_results) = self.reorder_result_bytes.checked_add(grant_limits.output_bytes) else {
            return self.fail_error(ResourceError::ArithmeticOverflow.into());
        };
        let Some(following_ordinal) = self.next_dispatch_ordinal.checked_add(1) else {
            return self.fail_error(ResourceError::ArithmeticOverflow.into());
        };
        let Some(next_active_tasks) = self.active_tasks.checked_add(1) else {
            return self.fail_error(ResourceError::ArithmeticOverflow.into());
        };
        if next_records > self.window.max_in_flight_records.get()
            || next_input > self.window.max_in_flight_input_bytes
            || next_results > self.window.max_reorder_result_bytes
        {
            return Ok(OrderedRecordDispatch::WindowFull);
        }
        let Some(task_slot) = self.tasks.as_slice().iter().position(Option::is_none) else {
            return self.fail_error(OrderedRecordCoordinatorError::InternalContract(
                "record window has no active-task slot",
            ));
        };
        let Some(permit) = scope.try_acquire() else {
            return Ok(OrderedRecordDispatch::NoWorkerPermit);
        };
        let (reservation, budget) = match resources.reserve_task_grant(grant_limits) {
            Ok(grant) => grant,
            Err(ResourceError::LimitExceeded {
                limit_kind: ResourceLimit::MemoryBytes,
                ..
            }) => return Ok(OrderedRecordDispatch::WindowFull),
            Err(error) => return self.fail_error(error.into()),
        };
        let ordinal = self.next_dispatch_ordinal;
        let task = match permit.spawn(budget, move |child, control| worker(ordinal, child, control)) {
            Ok(task) => task,
            Err(error) => {
                drop(reservation);
                return self.fail_error(OrderedRecordCoordinatorError::Spawn(error));
            }
        };
        self.tasks.as_mut_slice()[task_slot] = Some(ActiveTask {
            ordinal,
            input_bytes,
            result_grant_bytes: grant_limits.output_bytes,
            reservation,
            task,
        });
        self.next_dispatch_ordinal = following_ordinal;
        self.in_flight_records = next_records;
        self.in_flight_input_bytes = next_input;
        self.reorder_result_bytes = next_results;
        self.active_tasks = next_active_tasks;
        Ok(OrderedRecordDispatch::Started { ordinal })
    }

    /// Polls completed workers and offers only the next contiguous ordinal.
    ///
    /// # Errors
    ///
    /// Returns a worker panic/cancellation, adoption/accounting failure, or a detected coordinator contract violation.
    /// Every such failure cancels and drains all remaining tasks before returning.
    pub fn poll_next(&mut self) -> Result<OrderedRecordPoll<'_, Record, Terminal>, OrderedRecordCoordinatorError> {
        if let Some(slot) = self.offered_slot {
            if let Err(error) = self.validate_ready_offer(slot) {
                return self.fail_error(error);
            }
            return Ok(self.ready_offer(slot));
        }
        if self.complete {
            return Ok(OrderedRecordPoll::Complete);
        }

        for index in 0..self.tasks.len() {
            let finished = self.tasks.as_slice()[index]
                .as_ref()
                .is_some_and(|slot| slot.task.is_finished());
            if !finished {
                continue;
            }
            let Some(task) = self.tasks.as_mut_slice()[index].take() else {
                return self.fail_internal("finished task slot became empty");
            };
            if let Err(error) = self.join_finished(task) {
                self.cancel_and_drain();
                return Err(error);
            }
        }

        let next = match self.ring_index(self.next_release_ordinal) {
            Ok(next) => next,
            Err(error) => return self.fail_error(error),
        };
        let Some(slot) = self.ready.as_slice()[next].as_ref() else {
            if self.dispatch_finished && self.in_flight_records == 0 {
                self.complete = true;
                return Ok(OrderedRecordPoll::Complete);
            }
            return Ok(OrderedRecordPoll::Pending);
        };
        if slot.ordinal != self.next_release_ordinal {
            return self.fail_internal("ordinal ring contains a noncontiguous generation");
        }
        match slot.kind {
            ReadyKind::Record(_) => {
                self.offered_slot = Some(next);
                if let Err(error) = self.validate_ready_offer(next) {
                    return self.fail_error(error);
                }
                Ok(self.ready_offer(next))
            }
            ReadyKind::Terminal(_) => match self.release_terminal(next) {
                Ok(terminal) => Ok(OrderedRecordPoll::Terminal(terminal)),
                Err(error) => self.fail_error(error),
            },
        }
    }

    /// Acknowledges the currently offered record and releases its ring slot.
    ///
    /// # Errors
    ///
    /// Returns an account mismatch or a detected offer/ring contract violation. An absent offer is always a contract
    /// error.
    pub fn acknowledge_ready(&mut self, resources: &ResourceContext<'_>) -> Result<(), OrderedRecordCoordinatorError> {
        match self.acknowledge_ready_inner(resources) {
            Ok(()) => Ok(()),
            Err(error) => self.fail_error(error),
        }
    }

    fn acknowledge_ready_inner(
        &mut self,
        _resources: &ResourceContext<'_>,
    ) -> Result<(), OrderedRecordCoordinatorError> {
        let Some(index) = self.offered_slot else {
            return Err(OrderedRecordCoordinatorError::InternalContract(
                "record acknowledgement has no pending offer",
            ));
        };
        let Some(slot) = self.ready.as_slice()[index].as_ref() else {
            return Err(OrderedRecordCoordinatorError::InternalContract(
                "offered record slot is empty",
            ));
        };
        if !matches!(slot.kind, ReadyKind::Record(_)) {
            return Err(OrderedRecordCoordinatorError::InternalContract(
                "terminal slot was offered as a record",
            ));
        }
        let (next_records, next_results, next_ready, next_ordinal) =
            self.checked_release_state(slot.result_grant_bytes)?;

        let slot = self.ready.as_mut_slice()[index]
            .take()
            .ok_or(OrderedRecordCoordinatorError::InternalContract(
                "offered record disappeared before acknowledgement",
            ))?;
        self.offered_slot = None;
        self.in_flight_records = next_records;
        self.reorder_result_bytes = next_results;
        self.ready_results = next_ready;
        self.next_release_ordinal = next_ordinal;
        drop(slot);
        Ok(())
    }

    /// Cancels every active worker, joins it, and drops all unpublishable detached or adopted buffers. This operation
    /// is idempotent.
    pub fn cancel_and_drain(&mut self) {
        self.control.cancel();
        for slot in self.tasks.as_mut_slice() {
            let Some(active) = slot.take() else {
                continue;
            };
            let ActiveTask { reservation, task, .. } = active;
            if task.join().is_err() {
                self.cancelled_worker_panics += 1;
            }
            drop(reservation);
        }
        for slot in self.ready.as_mut_slice() {
            drop(slot.take());
        }
        self.in_flight_records = 0;
        self.in_flight_input_bytes = 0;
        self.reorder_result_bytes = 0;
        self.active_tasks = 0;
        self.ready_results = 0;
        self.offered_slot = None;
        self.dispatch_finished = true;
        self.dispatch_closed = true;
        self.complete = true;
    }

    fn join_finished(
        &mut self,
        active: ActiveTask<'scope, 'environment, Record, Terminal>,
    ) -> Result<(), OrderedRecordCoordinatorError> {
        let ActiveTask {
            ordinal,
            input_bytes,
            result_grant_bytes,
            reservation,
            task,
        } = active;
        let output = task
            .join()
            .map_err(|_| OrderedRecordCoordinatorError::WorkerPanicked)?
            .map_err(OrderedRecordCoordinatorError::ChildLedger)?;
        self.in_flight_input_bytes = self.in_flight_input_bytes.checked_sub(input_bytes).ok_or(
            OrderedRecordCoordinatorError::InternalContract("active input byte count underflow"),
        )?;
        self.active_tasks = self
            .active_tasks
            .checked_sub(1)
            .ok_or(OrderedRecordCoordinatorError::InternalContract(
                "active task count underflow",
            ))?;

        let (kind, buffer) = match output {
            OrderedRecordTaskOutput::Record { descriptor, detached } => {
                let buffer = reservation.adopt(detached)?;
                let buffer_len = u64::try_from(buffer.len()).map_err(|_| ResourceError::ArithmeticOverflow)?;
                if !(self.descriptor_contract.validate_record)(&descriptor, buffer_len) {
                    return Err(OrderedRecordCoordinatorError::InternalContract(
                        "record descriptor byte range exceeds its adopted buffer",
                    ));
                }
                (ReadyKind::Record(descriptor), Some(buffer))
            }
            OrderedRecordTaskOutput::Terminal { descriptor, detached } => {
                self.dispatch_closed = true;
                let buffer = if let Some(detached) = detached {
                    Some(reservation.adopt(detached)?)
                } else {
                    drop(reservation);
                    None
                };
                let buffer_len = buffer
                    .as_ref()
                    .map(AdoptedResultBuffer::len)
                    .map(u64::try_from)
                    .transpose()
                    .map_err(|_| ResourceError::ArithmeticOverflow)?;
                if !(self.descriptor_contract.validate_terminal)(&descriptor, buffer_len) {
                    return Err(OrderedRecordCoordinatorError::InternalContract(
                        "terminal descriptor byte range exceeds its adopted buffer",
                    ));
                }
                (ReadyKind::Terminal(descriptor), buffer)
            }
            OrderedRecordTaskOutput::Cancelled => {
                drop(reservation);
                return Err(OrderedRecordCoordinatorError::WorkerCancelled);
            }
            OrderedRecordTaskOutput::Refused(error) => {
                drop(reservation);
                return Err(error.into());
            }
        };
        self.insert_ready(ReadySlot {
            ordinal,
            result_grant_bytes,
            kind,
            buffer,
        })
    }

    fn insert_ready(&mut self, slot: ReadySlot<Record, Terminal>) -> Result<(), OrderedRecordCoordinatorError> {
        let index = self.ring_index(slot.ordinal)?;
        if self.ready.as_slice()[index].is_some() {
            return Err(OrderedRecordCoordinatorError::InternalContract(
                "ordinal ring slot collision",
            ));
        }
        let next_ready_results = self
            .ready_results
            .checked_add(1)
            .ok_or(ResourceError::ArithmeticOverflow)?;
        self.ready.as_mut_slice()[index] = Some(slot);
        self.ready_results = next_ready_results;
        Ok(())
    }

    fn validate_ready_offer(&self, index: usize) -> Result<(), OrderedRecordCoordinatorError> {
        let slot = self.ready.as_slice()[index]
            .as_ref()
            .ok_or(OrderedRecordCoordinatorError::InternalContract(
                "offered ordinal ring slot is empty",
            ))?;
        if !matches!(slot.kind, ReadyKind::Record(_)) {
            return Err(OrderedRecordCoordinatorError::InternalContract(
                "terminal ring slot entered record offer",
            ));
        }
        if slot.buffer.is_none() {
            return Err(OrderedRecordCoordinatorError::InternalContract(
                "record descriptor has no adopted bytes",
            ));
        }
        Ok(())
    }

    fn ready_offer(&self, index: usize) -> OrderedRecordPoll<'_, Record, Terminal> {
        let slot = self.ready.as_slice()[index]
            .as_ref()
            .expect("a validated record offer retains its ring slot");
        let ReadyKind::Record(descriptor) = &slot.kind else {
            unreachable!("a validated record offer retains its descriptor kind")
        };
        let buffer = slot
            .buffer
            .as_ref()
            .expect("a validated record offer retains its adopted buffer");
        OrderedRecordPoll::Ready(OrderedRecordReady {
            ordinal: slot.ordinal,
            descriptor,
            buffer,
        })
    }

    fn release_terminal(
        &mut self,
        index: usize,
    ) -> Result<OrderedRecordTerminal<Terminal>, OrderedRecordCoordinatorError> {
        let slot = self.ready.as_slice()[index]
            .as_ref()
            .ok_or(OrderedRecordCoordinatorError::InternalContract(
                "terminal ordinal ring slot is empty",
            ))?;
        if !matches!(slot.kind, ReadyKind::Terminal(_)) {
            return Err(OrderedRecordCoordinatorError::InternalContract(
                "record ring slot entered terminal release",
            ));
        }
        let (next_records, next_results, next_ready, next_ordinal) =
            self.checked_release_state(slot.result_grant_bytes)?;
        let slot = self.ready.as_mut_slice()[index]
            .take()
            .ok_or(OrderedRecordCoordinatorError::InternalContract(
                "terminal ordinal ring slot is empty",
            ))?;
        let ReadySlot {
            ordinal,
            result_grant_bytes,
            kind,
            buffer,
        } = slot;
        let ReadyKind::Terminal(descriptor) = kind else {
            return Err(OrderedRecordCoordinatorError::InternalContract(
                "record ring slot entered terminal release",
            ));
        };
        debug_assert_eq!(result_grant_bytes, self.reorder_result_bytes - next_results);
        self.in_flight_records = next_records;
        self.reorder_result_bytes = next_results;
        self.ready_results = next_ready;
        self.next_release_ordinal = next_ordinal;

        self.control.cancel();
        for active in self.tasks.as_mut_slice().iter_mut().filter_map(Option::take) {
            let ActiveTask { reservation, task, .. } = active;
            if task.join().is_err() {
                self.cancelled_worker_panics += 1;
            }
            drop(reservation);
        }
        for ready in self.ready.as_mut_slice() {
            drop(ready.take());
        }
        self.in_flight_records = 0;
        self.in_flight_input_bytes = 0;
        self.reorder_result_bytes = 0;
        self.active_tasks = 0;
        self.ready_results = 0;
        self.offered_slot = None;
        self.dispatch_finished = true;
        self.dispatch_closed = true;
        self.complete = true;

        Ok(OrderedRecordTerminal {
            ordinal,
            descriptor,
            buffer,
        })
    }

    fn checked_release_state(
        &self,
        result_grant_bytes: u64,
    ) -> Result<(u32, u64, u32, u64), OrderedRecordCoordinatorError> {
        let records = self
            .in_flight_records
            .checked_sub(1)
            .ok_or(OrderedRecordCoordinatorError::InternalContract(
                "in-flight record count underflow",
            ))?;
        let results = self.reorder_result_bytes.checked_sub(result_grant_bytes).ok_or(
            OrderedRecordCoordinatorError::InternalContract("reorder result byte count underflow"),
        )?;
        let ready = self
            .ready_results
            .checked_sub(1)
            .ok_or(OrderedRecordCoordinatorError::InternalContract(
                "ready result count underflow",
            ))?;
        let ordinal = self
            .next_release_ordinal
            .checked_add(1)
            .ok_or(ResourceError::ArithmeticOverflow)?;
        Ok((records, results, ready, ordinal))
    }

    fn ring_index(&self, ordinal: u64) -> Result<usize, OrderedRecordCoordinatorError> {
        let capacity = u64::try_from(self.ready.len()).map_err(|_| ResourceError::ArithmeticOverflow)?;
        let index = ordinal % capacity;
        usize::try_from(index).map_err(|_| OrderedRecordCoordinatorError::Resource(ResourceError::ArithmeticOverflow))
    }

    fn fail_internal<T>(&mut self, contract: &'static str) -> Result<T, OrderedRecordCoordinatorError> {
        self.fail_error(OrderedRecordCoordinatorError::InternalContract(contract))
    }

    fn fail_error<T>(&mut self, error: OrderedRecordCoordinatorError) -> Result<T, OrderedRecordCoordinatorError> {
        self.cancel_and_drain();
        Err(error)
    }
}

impl<Record: Copy + Send + Sync, Terminal: Copy + Send + Sync> Drop
    for OrderedRecordCoordinator<'_, '_, Record, Terminal>
{
    fn drop(&mut self) {
        if self.active_tasks != 0
            || self.in_flight_records != 0
            || self.ready_results != 0
            || self.tasks.as_slice().iter().any(Option::is_some)
            || self.ready.as_slice().iter().any(Option::is_some)
        {
            self.cancel_and_drain();
        }
    }
}
