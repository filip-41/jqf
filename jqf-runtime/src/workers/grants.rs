//! Record-worker grant envelopes and the observable worker count.
//!
//! This module is deliberately thread-free — it decides only how large one worker's numeric envelope is and how many
//! envelopes the parent ledger actually admits. It lives in the runtime adapter rather than in `jqf-codec-core` because
//! it is worker-dispatch policy that the parallel coordinator consumes, and the `no_std` substrate gains nothing by
//! carrying it.

use core::fmt;

use jqf_resource::task::{TaskGrantBudget, TaskGrantLimits, TaskGrantReservation};
use jqf_resource::{ResourceContext, ResourceError};

/// Working-state floor for one worker: parser, engine, and encoder scratch that exists independently of how much
/// payload the window carries.
const WORKING_FLOOR_BYTES: u64 = 64 << 10;

/// Output-staging floor for one worker. A batch whose payload encodes to nothing still needs room for framing and for
/// the smallest published item.
pub(crate) const OUTPUT_FLOOR_BYTES: u64 = 4 << 10;

/// Output capacity is sized at this multiple of the window's payload bytes.
///
/// It is a reservation CEILING, not the charge: the parent holds this much while the worker is in flight, and adopt
/// shrinks the charge to the published prefix. A program may publish more bytes than it reads. An output overflow
/// yields the morsel to serial: it surfaces as the worker's `try_append_within_capacity` failure, which fails the
/// morsel drive (`MorselFallbackCause::DriveFailed`) and reruns the request serially.
pub(crate) const OUTPUT_WINDOW_FACTOR: u64 = 2;

/// Encoder item-staging residency, charged on its own grant line (`pending_io_bytes`) so `output_bytes` stays the
/// publishable expansion.
///
/// The child ledger's `PendingIo` partition is `pending_io_bytes + output_bytes`: the output buffer claims
/// `output_bytes` exactly, and this reserve is what the encoder stages into. Subtracting it from `output_bytes` instead
/// collapsed the publishable ratio at the morsel floor.
pub(crate) const WORKER_ENCODER_STAGING_BYTES: u64 = 128 << 10;

/// Smallest morsel either planner will cut. The three-constant triangle (this floor, [`OUTPUT_WINDOW_FACTOR`],
/// [`WORKER_ENCODER_STAGING_BYTES`]) is asserted below so a floor edit cannot silently underflow the publishable
/// expansion.
pub(crate) const MORSEL_FLOOR_BYTES: u64 = 128 << 10;

const _: () = {
    const OUTPUT_AT_FLOOR: u64 = OUTPUT_FLOOR_BYTES + MORSEL_FLOOR_BYTES * OUTPUT_WINDOW_FACTOR;
    assert!(OUTPUT_AT_FLOOR > MORSEL_FLOOR_BYTES);
    assert!(WORKER_ENCODER_STAGING_BYTES > 0);
    assert!(OUTPUT_AT_FLOOR.checked_add(WORKER_ENCODER_STAGING_BYTES).is_some());
};

/// The per-worker memory envelope for a record-parallel request.
///
/// # Sized by the morsel window, never by the record ceiling
///
/// The envelope is derived from the MORSEL window — the payload bytes one worker may hold in flight. Production builds
/// it from `plan.morsel_bytes()` (`parallel/relay.rs`; the record route cuts morsels from 128 KiB to 4 MiB per
/// `RECORD_WIDTH_POLICY`), with the entry count from `jqf_sdk::RECORD_BATCH_ENTRIES` — never from the SDK's record
/// batch bound (`jqf_sdk::RECORD_BATCH_TARGET_BYTES`, the 256 KiB TEST-side construction only) and never from the
/// record stream's `max_record_bytes` option.
///
/// That option defaults to the request's whole input ceiling. Sizing a grant from it would make one worker's envelope
/// as large as the entire input, and since the parent request account binds the SUM of every in-flight grant, the
/// second reservation would always be refused: every `N >= 2` would silently degrade to one worker. The morsel size is
/// bounded by construction (128 KiB..4 MiB), so `N` envelopes fit whenever `N * morsel` fits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecordWorkerEnvelope {
    window_bytes: u64,
    fixed_bytes: u64,
}

impl RecordWorkerEnvelope {
    /// The observed per-worker fixed floor: the child ledger, the worker-local provider, its schema prototype, and the
    /// decode/encode session state that exist before a single record payload is read.
    ///
    /// This value is backed by a pinned receipt, not an estimate. `jqf-sdk-smoke`'s `task-grant` receipt runs a real
    /// native worker over one full concurrency window (256 records / 256 KiB) and bisects the smallest envelope the
    /// batch completes under. The measured floor is 322 699 bytes total — 19 997 retained, 27 648 working, 16 384
    /// pending IO, and 258 670 of output staging, which is the published byte count itself.
    ///
    /// The constant sits deliberately above the fixed part of that floor (~48 KiB of retained plus working) because the
    /// measurement is one program over one record shape, while the envelope must hold whatever a worker's program
    /// retains. The headroom is affordable: the resulting envelope still admits 326 workers under the default 512 MiB
    /// ceiling, far past the contract's documented hard cap. The receipt FAILS if the observed floor ever outgrows the
    /// envelope built from this constant, so the number cannot drift below reality unnoticed.
    pub const MEASURED_FIXED_BYTES: u64 = 512 << 10;

    /// Builds an envelope for one worker holding `window_bytes` of record payload.
    ///
    /// `fixed_bytes` is the observed per-worker floor; pass [`RecordWorkerEnvelope::MEASURED_FIXED_BYTES`] unless a
    /// caller has its own measurement.
    ///
    /// # Errors
    ///
    /// Returns an arithmetic error when the components do not fit in `u64`, and an accounting-invariant error for an
    /// empty window, which cannot carry a record.
    pub fn try_new(window_bytes: u64, fixed_bytes: u64) -> Result<Self, ResourceError> {
        if window_bytes == 0 {
            return Err(ResourceError::AccountingInvariantViolation);
        }
        let envelope = Self {
            window_bytes,
            fixed_bytes,
        };
        let _ = envelope.task_grant_limits()?;
        Ok(envelope)
    }

    /// Returns the four-component grant limits for one ordinary worker.
    ///
    /// `pending_io_bytes` is the encoder's own staging reserve: a record worker reads a BORROWED range of input the
    /// parent already charged, and the output buffer claims `output_bytes` exactly. Staging rides this line so the
    /// publishable expansion does not shrink as width rises and morsels hit the floor.
    ///
    /// # Errors
    ///
    /// Returns an arithmetic error when the components do not fit in `u64`.
    pub fn task_grant_limits(self) -> Result<TaskGrantLimits, ResourceError> {
        let add = |left: u64, right: u64| left.checked_add(right).ok_or(ResourceError::ArithmeticOverflow);
        let limits = TaskGrantLimits {
            retained_bytes: add(self.fixed_bytes, self.window_bytes)?,
            working_bytes: add(WORKING_FLOOR_BYTES, self.window_bytes)?,
            pending_io_bytes: WORKER_ENCODER_STAGING_BYTES,
            output_bytes: add(
                OUTPUT_FLOOR_BYTES,
                self.window_bytes
                    .checked_mul(OUTPUT_WINDOW_FACTOR)
                    .ok_or(ResourceError::ArithmeticOverflow)?,
            )?,
        };
        let _ = limits.total_memory_bytes()?;
        Ok(limits)
    }
}

/// How many workers a request ASKED for and how many actually ran.
///
/// The distinction is observable on purpose. The ledger law degrades a request to fewer workers when the parent ceiling
/// refuses a grant, and a measurement that cannot see the degradation would silently attribute serial timings to
/// `--workers N`. This batch-grant report is consumed by the `task-grant` receipt and the grant tests; the production
/// relay reserves per morsel inside the coordinator and surfaces degradation on its own relay line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerCountReport {
    requested: usize,
    granted: usize,
}

impl WorkerCountReport {
    /// Returns the worker count the request asked for.
    #[must_use]
    pub const fn requested(self) -> usize {
        self.requested
    }

    /// Returns the worker count the parent ledger actually granted.
    ///
    /// Zero means the request takes the serial path.
    #[must_use]
    pub const fn granted(self) -> usize {
        self.granted
    }

    /// Returns whether the ledger granted fewer workers than requested.
    #[must_use]
    pub const fn degraded(self) -> bool {
        self.granted < self.requested
    }
}

/// Every worker grant one request actually reserved, with its count report.
#[must_use = "dropping the grants releases every reserved envelope"]
pub struct RecordWorkerGrants {
    report: WorkerCountReport,
    /// Every reserved envelope. No code READS the vector — dropping it is what releases each reservation back to the
    /// parent ledger — so the dead-code allowance on this field is deliberate.
    #[allow(dead_code)]
    grants: Vec<(TaskGrantReservation, TaskGrantBudget)>,
}

impl fmt::Debug for RecordWorkerGrants {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RecordWorkerGrants")
            .field("report", &self.report)
            .finish_non_exhaustive()
    }
}

impl RecordWorkerGrants {
    /// Returns the requested-versus-granted report.
    #[must_use]
    pub const fn report(&self) -> WorkerCountReport {
        self.report
    }
}

/// Reserves up to `requested` worker envelopes against the parent request.
///
/// # The degradation law
///
/// Reservation stops at the FIRST envelope the parent ceiling refuses, and the report says how many were granted. A
/// refused grant degrades the request to fewer workers — possibly to zero, which is the serial path — and never to an
/// allocation outside the ledger. The parent ceiling therefore binds the sum of every in-flight envelope by
/// construction, because every envelope is an ordinary reservation against it.
///
/// # Errors
///
/// Returns an arithmetic error when the envelope's components do not fit in `u64`. A refused reservation is NOT an
/// error: it is the degradation the report records.
pub fn reserve_record_worker_grants(
    requested: usize,
    envelope: RecordWorkerEnvelope,
    resources: &ResourceContext<'_>,
) -> Result<RecordWorkerGrants, ResourceError> {
    let limits = envelope.task_grant_limits()?;
    let mut grants = Vec::new();
    for _ in 0..requested {
        match resources.reserve_task_grant(limits) {
            Ok(pair) => grants.push(pair),
            Err(_) => break,
        }
    }
    Ok(RecordWorkerGrants {
        report: WorkerCountReport {
            requested,
            granted: grants.len(),
        },
        grants,
    })
}

#[cfg(test)]
mod budget_ratio_tests {
    use super::{
        MORSEL_FLOOR_BYTES, OUTPUT_FLOOR_BYTES, OUTPUT_WINDOW_FACTOR, RecordWorkerEnvelope,
        WORKER_ENCODER_STAGING_BYTES,
    };

    #[test]
    fn publishable_expansion_at_the_morsel_floor_stays_above_one() {
        let envelope = RecordWorkerEnvelope::try_new(MORSEL_FLOOR_BYTES, RecordWorkerEnvelope::MEASURED_FIXED_BYTES)
            .expect("floor envelope");
        let limits = envelope.task_grant_limits().expect("limits");
        assert_eq!(
            limits.pending_io_bytes, WORKER_ENCODER_STAGING_BYTES,
            "staging rides its own grant line"
        );
        assert_eq!(
            limits.output_bytes,
            OUTPUT_FLOOR_BYTES + MORSEL_FLOOR_BYTES * OUTPUT_WINDOW_FACTOR,
            "output_bytes is the full expansion, not expansion minus staging"
        );
        assert!(
            limits.output_bytes > MORSEL_FLOOR_BYTES,
            "publishable expansion at the morsel floor must stay above 1× input"
        );
    }
}
