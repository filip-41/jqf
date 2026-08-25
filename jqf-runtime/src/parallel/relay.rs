//! The ordered morsel relay: one dispatch-and-publish loop, shared by every morsel route.
//!
//! # The one law this module rests on
//!
//! A morsel worker publishes bytes and nothing else, or the whole request yields to serial. "Nothing else" is exact: no
//! record issue, no per-value runtime error, no decode failure, no worker fault. A CLEAN morsel's bytes are what serial
//! would have written for the same range, so relaying them in ordinal order reproduces serial's stream byte for byte.
//!
//! Everything else — a diagnostic whose text carries an absolute input line number, a partial prefix before a terminal
//! error, a spawn failure, a panic — reaches the coordinator as its ordered TERMINAL. Because the terminal is released
//! only at the frontier, every EARLIER morsel is clean and already published, so the request holds exactly serial's
//! published prefix. The caller then re-runs the request SERIALLY over the whole input and discards precisely those
//! prefix bytes. What follows is not an imitation of serial: it IS serial, including its diagnostics, its line numbers,
//! and its exit class.
//!
//! A memory-ceiling or host-control refusal is not a yield: the request stops typed, the remaining stream is not
//! re-run, and no poisoned slice is published. Admission that cannot afford a worker still yields, so a request that
//! never started a worker can take the governed serial path.
//!
//! That is why byte identity here does not depend on the program class. Eligibility
//! ([`jqf_engine::CompiledProgram::is_morsel_eligible`] — every overload in the graph is `Effects::Pure`) is a promise
//! about what a campaign observed, not about what is provable; each route widened to it with its own differential
//! population.

use jqf_engine::CompiledProgram;
use jqf_resource::task::{DetachedTaskBuffer, TaskChildAccount, TaskGrantLimits, TaskOutputBuffer};
use jqf_resource::{ControlError, CooperativeError, ResourceContext, ResourceError, WorkMeter};
use jqf_sdk::{EncodedItemReport, ItemSink, RecordIssueReport, SequenceValueError};

use crate::parallel::plan::{Morsel, ParallelPlan};
use crate::workers::{
    MorselByteRange, MorselFallbackCause, MorselOutcome, NativeWorkerControl, NativeWorkerHost,
    OrderedRecordCoordinator, OrderedRecordCoordinatorError, OrderedRecordDescriptorContract, OrderedRecordDispatch,
    OrderedRecordPoll, OrderedRecordTaskOutput, RecordConcurrencyWindow, RecordWorkerEnvelope,
};

/// What one morsel drive counted, beside the bytes it published.
///
/// `units` is the route's own unit of progress — records for the record route, adjacent values for the value route —
/// and travels only so a completed request can report it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct MorselUnits {
    /// Input units this morsel decoded and ran.
    pub(crate) units: u64,
    /// Ordered items this morsel published.
    pub(crate) items: u64,
}

/// One route's worker-side drive, in a form that crosses a thread.
///
/// Implementors are `Copy` and carry only borrowed immutable bytes and plain values: no account, no document, no
/// provider, no encoder, and no sink, which is what lets a worker rebuild the whole drive from its own numeric grant.
pub(crate) trait MorselDrive<'request>: Copy + Send + Sync {
    /// The whole retained input the morsels index into.
    fn input(self) -> &'request [u8];

    /// Credits installed on every cooperative resume.
    fn cooperative_credits(self) -> u32;

    /// Runs this route's serial drive over `bytes`, publishing through `sink`.
    ///
    /// # Errors
    ///
    /// Returns the fallback cause when the drive did not complete CLEANLY: every failure and every diagnostic collapses
    /// here, because a worker that saw one byte range cannot render an ordered diagnostic itself.
    fn drive_morsel(
        self,
        bytes: &[u8],
        program: &CompiledProgram,
        resources: &mut ResourceContext<'_>,
        sink: &mut MorselSink<'_>,
    ) -> Result<MorselUnits, MorselFallbackCause>;
}

/// Pulls the next morsel for the relay. A slice source is finite and infallible; the adjacent-value scanner may report
/// a malformed remainder as a terminal (the same outcome a worker on that remainder would produce).
pub(crate) trait MorselSource {
    fn next_morsel(&mut self) -> Result<Option<Morsel>, MorselFallbackCause>;
}

/// `&[Morsel]` as a pull source. Used by the record lane, which still partitions before dispatch (newline cuts are
/// cheap).
pub(crate) struct SliceMorsels<'a> {
    rest: &'a [Morsel],
}

impl<'a> SliceMorsels<'a> {
    pub(crate) const fn new(morsels: &'a [Morsel]) -> Self {
        Self { rest: morsels }
    }
}

impl MorselSource for SliceMorsels<'_> {
    fn next_morsel(&mut self) -> Result<Option<Morsel>, MorselFallbackCause> {
        match self.rest.split_first() {
            Some((first, rest)) => {
                self.rest = rest;
                Ok(Some(*first))
            }
            None => Ok(None),
        }
    }
}

/// What the relay produced.
pub(crate) enum Relay<SinkError> {
    /// Every morsel was clean and its bytes are on the sink.
    Complete {
        /// Input units the clean morsels covered.
        units: u64,
        /// Ordered items the clean morsels published.
        items: u64,
        /// Morsels relayed.
        morsels_published: u64,
        /// Whether admission granted fewer workers than the plan asked for.
        degraded: bool,
        /// Workers that panicked on a cancel path. Zero on a clean complete.
        cancelled_worker_panics: u32,
    },
    /// The lane stopped at its frontier; the caller must re-run SERIALLY and discard `published_bytes` of prefix.
    YieldToSerial {
        /// Bytes the clean prefix already wrote to the sink.
        published_bytes: u64,
        /// Morsels relayed before the terminal.
        morsels_published: u64,
        /// Why the lane yielded.
        cause: MorselFallbackCause,
        /// Workers that panicked while the coordinator cancelled the rest.
        cancelled_worker_panics: u32,
    },
    /// The caller's sink refused a relayed morsel's bytes.
    SinkFailed(SinkError),
    /// A worker or the parent ledger refused the request. The remaining stream is not re-run: the blast radius is the
    /// whole request.
    Refused(ResourceError),
    /// The host cancelled the request, the deadline expired, or the physical memory ceiling was exceeded while workers
    /// were in flight.
    Control(ControlError),
}

const fn yielded<SinkError>(
    published_bytes: u64,
    morsels_published: u64,
    cause: MorselFallbackCause,
    cancelled_worker_panics: u32,
) -> Relay<SinkError> {
    Relay::YieldToSerial {
        published_bytes,
        morsels_published,
        cause,
        cancelled_worker_panics,
    }
}

/// A sink that discards a byte prefix the caller already published.
///
/// The prefix is exactly the clean morsels' relayed bytes, which the byte-identity law makes equal to serial's own
/// prefix over the same range. Item boundaries and diagnostics pass through unchanged: a clean prefix produced no
/// diagnostic, so the serial redrive produces none over it either.
pub(crate) struct SkippingSink<'sink, Sink> {
    inner: &'sink mut Sink,
    remaining: u64,
}

impl<'sink, Sink: ItemSink> SkippingSink<'sink, Sink> {
    /// Wraps `inner`, discarding its first `remaining` bytes.
    pub(crate) const fn new(inner: &'sink mut Sink, remaining: u64) -> Self {
        Self { inner, remaining }
    }
}

impl<Sink: ItemSink> ItemSink for SkippingSink<'_, Sink> {
    type Error = Sink::Error;

    fn begin_item(&mut self, index: u64) -> Result<(), Self::Error> {
        self.inner.begin_item(index)
    }

    fn begin_item_named(&mut self, index: u64, name: &str) -> Result<(), Self::Error> {
        self.inner.begin_item_named(index, name)
    }

    fn write(&mut self, bytes: &[u8]) -> Result<usize, Self::Error> {
        if self.remaining == 0 {
            return self.inner.write(bytes);
        }
        let skipped = usize::try_from(self.remaining).unwrap_or(usize::MAX).min(bytes.len());
        self.remaining -= skipped as u64;
        if skipped == bytes.len() {
            return Ok(bytes.len());
        }
        let written = self.inner.write(&bytes[skipped..])?;
        Ok(skipped.saturating_add(written))
    }

    fn finish_item(&mut self, index: u64, report: EncodedItemReport) -> Result<(), Self::Error> {
        self.inner.finish_item(index, report)
    }

    fn report_value_error(&mut self, error: SequenceValueError) -> Result<(), Self::Error> {
        self.inner.report_value_error(error)
    }

    fn report_record_issue(&mut self, issue: RecordIssueReport<'_>) -> Result<(), Self::Error> {
        self.inner.report_record_issue(issue)
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.inner.flush()
    }
}

/// Publication staging for one worker: every byte lands in capacity the worker's own grant already committed, and every
/// diagnostic is counted so the morsel can declare itself unclean.
pub(crate) struct MorselSink<'output> {
    output: &'output mut TaskOutputBuffer,
    diagnostics: u64,
}

impl MorselSink<'_> {
    /// Returns whether the drive reported any ordered diagnostic.
    #[must_use]
    pub(crate) const fn is_clean(&self) -> bool {
        self.diagnostics == 0
    }

    /// Returns the bytes staged so far.
    #[must_use]
    pub(crate) fn published_len(&self) -> u64 {
        self.output.len() as u64
    }
}

impl ItemSink for MorselSink<'_> {
    type Error = ResourceError;

    fn begin_item(&mut self, _index: u64) -> Result<(), Self::Error> {
        Ok(())
    }

    fn write(&mut self, bytes: &[u8]) -> Result<usize, Self::Error> {
        self.output.try_append_within_capacity(bytes)?;
        Ok(bytes.len())
    }

    fn observes_host_progress(&self) -> bool {
        false
    }

    fn finish_item(&mut self, _index: u64, _report: EncodedItemReport) -> Result<(), Self::Error> {
        Ok(())
    }

    fn report_value_error(&mut self, _error: SequenceValueError) -> Result<(), Self::Error> {
        self.diagnostics = self.diagnostics.saturating_add(1);
        Ok(())
    }

    fn report_record_issue(&mut self, _issue: RecordIssueReport<'_>) -> Result<(), Self::Error> {
        self.diagnostics = self.diagnostics.saturating_add(1);
        Ok(())
    }
}

/// The complete morsel worker: a child ledger in, a clean morsel or a fallback terminal out.
///
/// Nothing request-local crosses the thread boundary. The worker binds the child ledger the spawn seam opened from the
/// linear grant (the spawn closure installed that ledger as this thread's ambient account), borrows the parent
/// [`CompiledProgram`] (retained bytes stay on the parent ledger), builds its own provider and encoder, and stages
/// every published byte inside output capacity the grant already committed.
fn run_morsel<'request, Drive: MorselDrive<'request>>(
    drive: Drive,
    morsel: Morsel,
    program: &'request CompiledProgram,
    output_capacity: usize,
    child: TaskChildAccount,
    control: &NativeWorkerControl,
) -> OrderedRecordTaskOutput<MorselOutcome, MorselFallbackCause> {
    match try_run_morsel(drive, morsel, program, output_capacity, child, control) {
        Ok((descriptor, detached)) => OrderedRecordTaskOutput::record(descriptor, detached),
        Err(MorselRunError::Fallback(cause)) => OrderedRecordTaskOutput::terminal(cause, None),
        Err(MorselRunError::Refused(error)) => OrderedRecordTaskOutput::refused(error),
    }
}

enum MorselRunError {
    Fallback(MorselFallbackCause),
    Refused(CooperativeError),
}

impl From<MorselFallbackCause> for MorselRunError {
    fn from(cause: MorselFallbackCause) -> Self {
        Self::Fallback(cause)
    }
}

fn try_run_morsel<'request, Drive: MorselDrive<'request>>(
    drive: Drive,
    morsel: Morsel,
    program: &'request CompiledProgram,
    output_capacity: usize,
    child: TaskChildAccount,
    control: &NativeWorkerControl,
) -> Result<(MorselOutcome, DetachedTaskBuffer), MorselRunError> {
    let work = WorkMeter::try_new_v1(drive.cooperative_credits()).ok_or(MorselFallbackCause::WorkerUnavailable)?;
    let mut child = match child.bind(control, work) {
        Ok(child) => child,
        Err(error) => {
            if let Some(refused) = control.refused() {
                return Err(MorselRunError::Refused(refused));
            }
            return Err(match error {
                ControlError::MemoryExceeded | ControlError::DeadlineExceeded => {
                    MorselRunError::Refused(CooperativeError::Control(error))
                }
                ControlError::Cancelled => MorselFallbackCause::WorkerUnavailable.into(),
            });
        }
    };
    let mut output = TaskOutputBuffer::try_with_capacity(output_capacity, &mut child)
        .map_err(|_| MorselFallbackCause::WorkerUnavailable)?;
    let bytes = drive
        .input()
        .get(morsel.start()..morsel.end())
        .ok_or(MorselFallbackCause::WorkerUnavailable)?;
    let outcome = {
        let mut sink = MorselSink {
            output: &mut output,
            diagnostics: 0,
        };
        let counts = drive.drive_morsel(bytes, program, &mut child, &mut sink)?;
        if !sink.is_clean() {
            return Err(MorselFallbackCause::Diagnostics.into());
        }
        let published = sink.published_len();
        let range = MorselByteRange::try_new(0, published).ok_or(MorselFallbackCause::DriveFailed)?;
        MorselOutcome::new(range, counts.units, counts.items)
    };
    // Detachment demands a QUIESCENT child: its ledger baseline plus the one result allocation, nothing else. The
    // program is a parent borrow — its retained bytes never landed on this ledger — so there is nothing to drop here
    // before detach.
    let detached = match child.detach_result(output) {
        Ok(detached) => detached,
        Err(error) => {
            return Err(MorselRunError::Refused(CooperativeError::Memory(error)));
        }
    };
    Ok((outcome, detached))
}

/// Dispatches `morsels` across `plan.workers()` and relays their bytes in ordinal order.
///
/// Spawn, panic, and admission-empty problems become [`Relay::YieldToSerial`]. A memory-ceiling or host-control refusal
/// becomes [`Relay::Refused`] or [`Relay::Control`] and is not re-run.
#[allow(
    clippy::too_many_lines,
    reason = "one linear dispatch-and-relay loop; splitting it would thread the coordinator, the \
              scope, the plan, and the sink through helpers that each read one of them"
)]
pub(crate) fn relay_morsels<'request, Drive: MorselDrive<'request>, Sink: ItemSink, Source: MorselSource>(
    drive: Drive,
    plan: ParallelPlan,
    mut morsels: Source,
    program: &'request CompiledProgram,
    resources: &mut ResourceContext<'_>,
    sink: &mut Sink,
) -> Relay<Sink::Error> {
    // The SDK's process-lifetime default dialect is allocated exactly once; warm it HERE, on the coordinator thread, so
    // that one-time charge never lands on a worker child ledger whose detach demands quiescence (the 55-byte Working
    // leak that turned every morsel drive into a fallback).
    jqf_sdk::warm_default_dialect();
    // First `std::sync::Mutex::lock` in the process allocates a 64-byte pthread mutex box. Worker `install_ambient`
    // takes the resource crate's quiesce lock as an outermost install; without a parent ambient ledger that lock is the
    // first, the box is charged to the child, and detach fails (`Working=64`). Warm it here, on the coordinator.
    jqf_resource::warm_ambient_mutex();
    let Ok(envelope) = RecordWorkerEnvelope::try_new(plan.morsel_bytes(), RecordWorkerEnvelope::MEASURED_FIXED_BYTES)
    else {
        return yielded(0, 0, MorselFallbackCause::WorkerUnavailable, 0);
    };
    let Ok(limits) = envelope.task_grant_limits() else {
        return yielded(0, 0, MorselFallbackCause::WorkerUnavailable, 0);
    };
    // Staging is charged on its own grant line (`pending_io_bytes`), so `output_bytes` is the full publishable
    // expansion. The worker claims that whole component for the adopted buffer.
    if limits.output_bytes == 0 {
        return yielded(0, 0, MorselFallbackCause::WorkerUnavailable, 0);
    }
    let output_capacity = usize::try_from(limits.output_bytes).unwrap_or(usize::MAX);
    let Some(window) = concurrency_window(plan, limits) else {
        return yielded(0, 0, MorselFallbackCause::WorkerUnavailable, 0);
    };
    let contract = OrderedRecordDescriptorContract::new(
        |descriptor: &MorselOutcome, buffer_len| descriptor.fits_buffer(buffer_len),
        // A fallback terminal carries no bytes: its morsel's output is discarded, and serial republishes the range from
        // the start.
        |_terminal: &MorselFallbackCause, buffer_len| buffer_len.is_none(),
    );
    let workers = affordable_workers(resources, limits, plan.workers());
    if workers == 0 {
        return yielded(0, 0, MorselFallbackCause::WorkerUnavailable, 0);
    }
    let admission_degraded = workers < plan.workers();
    let host = NativeWorkerHost::new(workers);
    host.scope(|scope| {
        let Ok(mut coordinator) = OrderedRecordCoordinator::try_new(scope, window, contract) else {
            return yielded(0, 0, MorselFallbackCause::WorkerUnavailable, 0);
        };
        let mut held: Option<Morsel> = None;
        let mut source_done = false;
        let mut published_bytes = 0u64;
        let mut morsels_published = 0u64;
        let mut units = 0u64;
        let mut items = 0u64;
        loop {
            if let Err(error) = resources.check_control() {
                scope.refuse(CooperativeError::Control(error));
                coordinator.cancel_and_drain();
                return Relay::Control(error);
            }
            while !source_done {
                let morsel = match held.take() {
                    Some(morsel) => morsel,
                    None => match morsels.next_morsel() {
                        Ok(Some(morsel)) => morsel,
                        Ok(None) => {
                            source_done = true;
                            break;
                        }
                        Err(cause) => {
                            coordinator.cancel_and_drain();
                            return yielded(
                                published_bytes,
                                morsels_published,
                                cause,
                                coordinator.snapshot().cancelled_worker_panics(),
                            );
                        }
                    },
                };
                // A morsel larger than the window's whole input-bytes budget can NEVER be dispatched: even alone it
                // exceeds the ceiling, so `try_dispatch` answers WindowFull forever and the relay would retry into a
                // livelock. The boundary scan grows a morsel past its target when the record boundary is inside a
                // quoted field (CSV) — an unterminated quote runs the morsel to end-of-input, which is the
                // oversized-record shape the serial fallback serves. The VALUES lane fires the same guard on its common
                // shape: one huge value after small ones — the boundary scan cuts only at a PROVEN top-level boundary,
                // so the huge value runs one shard to end-of-input, oversized by construction. Yield to serial: the
                // redrive reproduces the exact bytes.
                if morsel.len() as u64 > window.max_in_flight_input_bytes() {
                    coordinator.cancel_and_drain();
                    return yielded(
                        published_bytes,
                        morsels_published,
                        MorselFallbackCause::WorkerUnavailable,
                        coordinator.snapshot().cancelled_worker_panics(),
                    );
                }
                let dispatch = coordinator.try_dispatch(
                    scope,
                    morsel.len() as u64,
                    limits,
                    resources,
                    move |_ordinal, child, control| run_morsel(drive, morsel, program, output_capacity, child, control),
                );
                match dispatch {
                    Ok(OrderedRecordDispatch::Started { .. }) => {}
                    Ok(OrderedRecordDispatch::WindowFull | OrderedRecordDispatch::NoWorkerPermit) => {
                        held = Some(morsel);
                        // After a clean prefix is acknowledged, in-flight is 0 and a later MemoryBytes refusal is
                        // WindowFull — yield, do not park with nobody to unpark.
                        if coordinator.snapshot().in_flight_records() == 0 {
                            coordinator.cancel_and_drain();
                            return yielded(
                                published_bytes,
                                morsels_published,
                                MorselFallbackCause::WorkerUnavailable,
                                coordinator.snapshot().cancelled_worker_panics(),
                            );
                        }
                        break;
                    }
                    Ok(_) => {
                        held = Some(morsel);
                        break;
                    }
                    Err(_) => {
                        coordinator.cancel_and_drain();
                        return yielded(
                            published_bytes,
                            morsels_published,
                            MorselFallbackCause::WorkerUnavailable,
                            coordinator.snapshot().cancelled_worker_panics(),
                        );
                    }
                }
            }
            if source_done {
                coordinator.finish_dispatch();
            }
            match coordinator.poll_next() {
                Ok(OrderedRecordPoll::Pending) => coordinator.park_frontier(),
                Ok(OrderedRecordPoll::Ready(ready)) => {
                    if let Err(error) = resources.check_control() {
                        scope.refuse(CooperativeError::Control(error));
                        coordinator.cancel_and_drain();
                        return Relay::Control(error);
                    }
                    let descriptor = *ready.descriptor();
                    let range = descriptor.bytes();
                    let start = usize::try_from(range.start()).unwrap_or(usize::MAX);
                    let end = usize::try_from(range.end()).unwrap_or(usize::MAX);
                    let Some(bytes) = ready.buffer().as_slice().get(start..end) else {
                        coordinator.cancel_and_drain();
                        return yielded(
                            published_bytes,
                            morsels_published,
                            MorselFallbackCause::WorkerUnavailable,
                            coordinator.snapshot().cancelled_worker_panics(),
                        );
                    };
                    match write_all(sink, bytes) {
                        Ok(written) if written == bytes.len() => {
                            published_bytes = published_bytes.saturating_add(range.len());
                            morsels_published = morsels_published.saturating_add(1);
                            // A PUBLISHED morsel must be OBSERVABLE before the next one lands: the host sink (the CLI's
                            // 64 KiB stdout BufWriter) would otherwise hold every morsel until the run's final flush,
                            // which makes the parallel lane's first byte the run's last byte. The terminal-discard law
                            // is byte ACCOUNTING, not timing — the serial redrive's SkippingSink skips
                            // `published_bytes` whether they reached the host or not — so a per-morsel flush changes
                            // only WHEN bytes appear, never WHICH bytes, and byte identity is untouched. A short-count
                            // violation returns before this, so a lying sink is never flushed past its own count.
                            if let Err(error) = sink.flush() {
                                return Relay::SinkFailed(error);
                            }
                        }
                        Ok(written) => {
                            // The sink accepted fewer bytes than the morsel holds (a contract violation: `Ok(0)`, or a
                            // count past the slice). Count ONLY the bytes the sink confirmed and stop: this lane must
                            // never report `Complete` with a short count, or the caller's yield-to-serial SkippingSink
                            // would skip bytes the serial redrive never wrote. `WorkerUnavailable` is the closest
                            // existing cause; morsel.rs owns the taxonomy.
                            coordinator.cancel_and_drain();
                            return yielded(
                                published_bytes.saturating_add(written as u64),
                                morsels_published,
                                MorselFallbackCause::WorkerUnavailable,
                                coordinator.snapshot().cancelled_worker_panics(),
                            );
                        }
                        Err(error) => return Relay::SinkFailed(error),
                    }
                    units = units.saturating_add(descriptor.records());
                    items = items.saturating_add(descriptor.items());
                    if coordinator.acknowledge_ready(resources).is_err() {
                        coordinator.cancel_and_drain();
                        return yielded(
                            published_bytes,
                            morsels_published,
                            MorselFallbackCause::WorkerUnavailable,
                            coordinator.snapshot().cancelled_worker_panics(),
                        );
                    }
                }
                Ok(OrderedRecordPoll::Terminal(terminal)) => {
                    let cause = *terminal.descriptor();
                    let panics = coordinator.snapshot().cancelled_worker_panics();
                    return yielded(published_bytes, morsels_published, cause, panics);
                }
                Ok(OrderedRecordPoll::Complete) => {
                    return Relay::Complete {
                        units,
                        items,
                        morsels_published,
                        degraded: admission_degraded,
                        cancelled_worker_panics: coordinator.snapshot().cancelled_worker_panics(),
                    };
                }
                Err(OrderedRecordCoordinatorError::Resource(error)) => {
                    return Relay::Refused(error);
                }
                Err(OrderedRecordCoordinatorError::Control(error)) => {
                    return Relay::Control(error);
                }
                Err(_) => {
                    coordinator.cancel_and_drain();
                    return yielded(
                        published_bytes,
                        morsels_published,
                        MorselFallbackCause::WorkerUnavailable,
                        coordinator.snapshot().cancelled_worker_panics(),
                    );
                }
            }
        }
    })
}

fn affordable_workers(resources: &ResourceContext<'_>, limits: TaskGrantLimits, requested: usize) -> usize {
    let Ok(per_worker) = limits.total_memory_bytes() else {
        return 0;
    };
    if per_worker == 0 {
        return requested;
    }
    let headroom = resources
        .limits()
        .max_memory_bytes()
        .saturating_sub(resources.snapshot().memory_current_bytes());
    usize::try_from(headroom / per_worker).unwrap_or(0).min(requested)
}

fn concurrency_window(plan: ParallelPlan, limits: TaskGrantLimits) -> Option<RecordConcurrencyWindow> {
    // Ready-but-unacknowledged morsels share the ordinal ring with dispatched ones, so the ring holds one reorder slot
    // per worker beyond the workers themselves.
    let in_flight = u32::try_from(plan.workers().saturating_mul(2)).ok()?.max(2);
    RecordConcurrencyWindow::try_new(
        core::num::NonZeroU32::new(in_flight)?,
        u64::from(in_flight).checked_mul(plan.morsel_bytes())?,
        u64::from(in_flight).checked_mul(limits.output_bytes)?,
    )
}

fn write_all<Sink: ItemSink>(sink: &mut Sink, bytes: &[u8]) -> Result<usize, Sink::Error> {
    let mut written = 0usize;
    while written < bytes.len() {
        let n = sink.write(&bytes[written..])?;
        if n == 0 || n > bytes.len() - written {
            // The sink contract forbids both: `Ok(0)` accepts nothing and a count past the slice is a claim no slice
            // can honor. The relay cannot fabricate a `Sink::Error` for a violating sink, so it stops and reports ONLY
            // the bytes the sink confirmed — never the unaccepted tail, which would let a yield-to-serial SkippingSink
            // discard bytes that never reached the sink.
            break;
        }
        written += n;
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    use jqf_codec_core::{DiagnosticPolicy, ValidationMode};
    use jqf_engine::{CodecRequirementPolicy, CompiledProgram, try_compile_program};
    use jqf_resource::{
        ContinueControl, Control, ControlError, ControlOutcome, RequestAccount, ResourceContext, ResourceLimits,
        WorkMeter,
    };
    use jqf_sdk::{EncodedItemReport, ItemSink};

    use crate::parallel::plan::{Morsel, WidthPolicy, WorkerRequest, plan_request};

    static CONTINUE: ContinueControl = ContinueControl;

    fn resources() -> ResourceContext<'static> {
        ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(256 << 20, 256 << 20, 512 << 20, 0, 10_000))
                .expect("request account"),
            &CONTINUE,
            WorkMeter::try_new_v1(1).expect("work meter"),
        )
        .expect("resources")
    }

    fn identity_program(resources: &ResourceContext<'_>) -> CompiledProgram {
        try_compile_program(
            ".",
            CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly),
            resources,
        )
        .expect("identity compiles")
    }

    /// A contract-violating sink: every write returns `Ok(0)` — it accepts nothing, so any count the relay claims for
    /// it is a lie.
    struct ZeroSink {
        /// Writes the relay actually attempted; proves the lane engaged.
        calls: usize,
    }

    impl ItemSink for ZeroSink {
        type Error = std::convert::Infallible;

        fn begin_item(&mut self, _index: u64) -> Result<(), Self::Error> {
            Ok(())
        }

        fn write(&mut self, _bytes: &[u8]) -> Result<usize, Self::Error> {
            self.calls += 1;
            Ok(0)
        }

        fn finish_item(&mut self, _index: u64, _report: EncodedItemReport) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    /// A drive publishing one fixed byte per morsel, so every relayed morsel hands `write_all` a nonempty slice.
    #[derive(Clone, Copy)]
    struct OneByteDrive;

    impl<'request> MorselDrive<'request> for OneByteDrive {
        fn input(self) -> &'request [u8] {
            b"xxxx"
        }

        fn cooperative_credits(self) -> u32 {
            1
        }

        fn drive_morsel(
            self,
            _bytes: &[u8],
            _program: &CompiledProgram,
            _resources: &mut ResourceContext<'_>,
            sink: &mut MorselSink<'_>,
        ) -> Result<MorselUnits, MorselFallbackCause> {
            sink.write(b"x").map_err(|_| MorselFallbackCause::DriveFailed)?;
            Ok(MorselUnits::default())
        }
    }

    #[test]
    fn zero_write_counts_nothing_accepted() {
        let mut sink = ZeroSink { calls: 0 };
        // A sink that accepts nothing must never be counted as having published the slice: the relay would otherwise
        // report Complete with a short count and the yield-to-serial SkippingSink would discard bytes that never
        // reached the sink.
        assert_eq!(write_all(&mut sink, b"hello").expect("no sink error"), 0);
        assert_eq!(sink.calls, 1);
    }

    #[test]
    fn zero_write_sink_never_completes_short() {
        let mut resources = resources();
        // A window large enough that the lane actually ENGAGES, or the test would pass for the wrong reason (an early
        // WorkerUnavailable fast path never reaching the sink).
        let plan = plan_request(
            WorkerRequest::Explicit(2),
            4 << 20,
            None,
            WidthPolicy {
                break_even_bytes: 0,
                min_workers: 1,
                min_morsel_bytes: 1 << 20,
                max_morsel_bytes: 1 << 20,
                morsels_per_worker: 2,
            },
        );
        let morsels = [Morsel::new(0, 1), Morsel::new(1, 2)];
        let mut sink = ZeroSink { calls: 0 };
        let program = identity_program(&resources);
        let result = relay_morsels(
            OneByteDrive,
            plan,
            SliceMorsels::new(&morsels),
            &program,
            &mut resources,
            &mut sink,
        );
        // The lane must have ENGAGED (the relay called the sink's write), so the outcome below is the relay's, not a
        // grant/unavailable fast path.
        assert!(
            sink.calls >= 1,
            "lane never reached the sink's write (calls={})",
            sink.calls
        );
        match result {
            Relay::Complete { .. } => {
                panic!("Complete with a sink that accepted nothing")
            }
            Relay::SinkFailed(error) => match error {},
            Relay::YieldToSerial { .. } => {}
            Relay::Refused(error) => panic!("typed refusal on a short-count sink: {error}"),
            Relay::Control(error) => panic!("host control stop on a short-count sink: {error}"),
        }
    }

    struct AfterFirst {
        seen: core::cell::Cell<bool>,
    }

    impl Control for AfterFirst {
        fn check(&self) -> ControlOutcome {
            if self.seen.replace(true) {
                ControlOutcome::MemoryExceeded
            } else {
                ControlOutcome::Continue
            }
        }
    }

    struct CollectSink {
        bytes: Vec<u8>,
    }

    impl ItemSink for CollectSink {
        type Error = std::convert::Infallible;

        fn begin_item(&mut self, _index: u64) -> Result<(), Self::Error> {
            Ok(())
        }

        fn write(&mut self, bytes: &[u8]) -> Result<usize, Self::Error> {
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn finish_item(&mut self, _index: u64, _report: EncodedItemReport) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[test]
    fn a_host_memory_stop_refuses_typed_and_publishes_nothing() {
        let stop = AfterFirst {
            seen: core::cell::Cell::new(false),
        };
        let mut resources = ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(256 << 20, 256 << 20, 512 << 20, 0, 10_000))
                .expect("request account"),
            &stop,
            WorkMeter::try_new_v1(1).expect("work meter"),
        )
        .expect("resources");
        let plan = plan_request(
            WorkerRequest::Explicit(2),
            4 << 20,
            None,
            WidthPolicy {
                break_even_bytes: 0,
                min_workers: 1,
                min_morsel_bytes: 1 << 20,
                max_morsel_bytes: 1 << 20,
                morsels_per_worker: 2,
            },
        );
        let morsels = [Morsel::new(0, 1), Morsel::new(1, 2)];
        let mut sink = CollectSink { bytes: Vec::new() };
        let program = identity_program(&resources);
        let result = relay_morsels(
            OneByteDrive,
            plan,
            SliceMorsels::new(&morsels),
            &program,
            &mut resources,
            &mut sink,
        );
        assert!(
            sink.bytes.is_empty(),
            "a host memory stop must not publish: {:?}",
            sink.bytes
        );
        match result {
            Relay::Control(ControlError::MemoryExceeded) => {}
            Relay::Complete { .. } => panic!("complete after a host memory stop"),
            Relay::YieldToSerial { .. } => panic!("yielded to serial after a host memory stop"),
            Relay::SinkFailed(error) => match error {},
            Relay::Refused(error) => panic!("resource refusal after a host memory stop: {error}"),
            Relay::Control(error) => panic!("unexpected control stop: {error}"),
        }
    }

    #[test]
    fn parent_program_borrow_completes_and_detaches() {
        let mut resources = resources();
        let program = identity_program(&resources);
        let plan = plan_request(
            WorkerRequest::Explicit(2),
            4 << 20,
            None,
            WidthPolicy {
                break_even_bytes: 0,
                min_workers: 1,
                min_morsel_bytes: 1 << 20,
                max_morsel_bytes: 1 << 20,
                morsels_per_worker: 2,
            },
        );
        let morsels = [Morsel::new(0, 1), Morsel::new(1, 2)];
        let mut sink = CollectSink { bytes: Vec::new() };
        let result = relay_morsels(
            OneByteDrive,
            plan,
            SliceMorsels::new(&morsels),
            &program,
            &mut resources,
            &mut sink,
        );
        match result {
            Relay::Complete { morsels_published, .. } => {
                assert_eq!(morsels_published, 2);
                assert_eq!(sink.bytes, b"xx");
            }
            Relay::YieldToSerial { cause, .. } => {
                panic!("parent-borrow relay yielded: {cause:?}")
            }
            Relay::SinkFailed(error) => match error {},
            Relay::Refused(error) => panic!("parent-borrow relay refused: {error}"),
            Relay::Control(error) => panic!("parent-borrow relay control: {error}"),
        }
    }
}
