//! Task-grant receipt: the minimum viable grant for one record-route worker.
//!
//! Bisects the smallest envelope a real native worker completes under over
//! one full concurrency window. The measured floor backs
//! [`jqf_runtime::workers::RecordWorkerEnvelope::MEASURED_FIXED_BYTES`].
//! Record-route law pins live in [`crate::records`].

use crate::harness::{CONTROL, PartialSink, json_dialect, program_for};
use jqf_codec_core::{DecodeRequest, DiagnosticPolicy, PreservationRequest, ValidationMode};
use jqf_data::{DialectId, FormatId};
use jqf_resource::task::{TaskGrantLimits, TaskOutputBuffer};
use jqf_resource::{MemoryCategory, RequestAccount, ResourceContext, ResourceLimits, UsageSnapshot, WorkMeter};
use jqf_runtime::workers::{NativeWorkerHost, RecordWorkerEnvelope};
use jqf_sdk::{
    CodecCatalog, EncodedItemReport, FacadeFraming, ItemSink, PipelinePolicy, RECORD_BATCH_ENTRIES,
    RECORD_BATCH_TARGET_BYTES,
};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};
use std::sync::OnceLock;

/// Stand-in aggregate memory ceiling for the worker-grant receipt (not the
/// CLI's default, which is a percent of RAM).
const DEFAULT_CEILING_BYTES: u64 = 512 << 20;

/// Publication staging for one worker: every byte the record drive publishes
/// lands in capacity the worker's own grant already committed.
struct GrantStagingSink<'output> {
    output: &'output mut TaskOutputBuffer,
}

impl ItemSink for GrantStagingSink<'_> {
    type Error = &'static str;

    fn begin_item(&mut self, _index: u64) -> Result<(), Self::Error> {
        Ok(())
    }

    fn write(&mut self, bytes: &[u8]) -> Result<usize, Self::Error> {
        self.output
            .try_append_within_capacity(bytes)
            .map_err(|_| "worker output staging exceeded its grant")?;
        Ok(bytes.len())
    }

    fn finish_item(&mut self, _index: u64, _report: EncodedItemReport) -> Result<(), Self::Error> {
        Ok(())
    }

    fn observes_host_progress(&self) -> bool {
        false
    }
}

/// One full record concurrency window: `RECORD_BATCH_ENTRIES` records whose
/// payloads total `RECORD_BATCH_TARGET_BYTES`. This is the batch one worker
/// is granted, so it is what the grant floor must be measured over.
fn representative_record_batch() -> Vec<u8> {
    let records = RECORD_BATCH_ENTRIES as usize;
    let record_bytes = usize::try_from(RECORD_BATCH_TARGET_BYTES).unwrap_or(usize::MAX) / records;
    let mut batch = Vec::with_capacity(records * record_bytes);
    for index in 0..records {
        let prefix = format!("{{\"k\":{index},\"v\":\"");
        let suffix = "\"}\n";
        let padding = record_bytes.saturating_sub(prefix.len() + suffix.len());
        batch.extend_from_slice(prefix.as_bytes());
        batch.extend(core::iter::repeat_n(b'x', padding));
        batch.extend_from_slice(suffix.as_bytes());
    }
    assert_eq!(
        u64::try_from(batch.len()).unwrap_or(u64::MAX),
        RECORD_BATCH_TARGET_BYTES,
        "record batch must fill the grant window"
    );
    batch
}

fn worker_parent_resources() -> ResourceContext<'static> {
    ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(
            u64::MAX,
            u64::MAX,
            DEFAULT_CEILING_BYTES,
            u64::MAX,
            128,
        ))
        .expect("worker parent account"),
        &CONTROL,
        WorkMeter::try_new_v1(7).expect("work meter"),
    )
    .expect("worker parent resources")
}

/// Runs main's real record route — `execute_record_sequence` over the
/// `jqf.record-stream@1` provider — against `resources`.
fn ndjson_registrations() -> &'static [jqf_codec_core::CodecRegistration<'static>; 2] {
    static REGS: OnceLock<[jqf_codec_core::CodecRegistration<'static>; 2]> = OnceLock::new();
    REGS.get_or_init(|| {
        [
            jqf_codec_json::registration().expect("json registration"),
            jqf_codec_json::ndjson::registration().expect("ndjson registration"),
        ]
    })
}

fn drive_records<Sink: ItemSink<Error = &'static str>>(
    input: &[u8],
    resources: &mut ResourceContext<'_>,
    sink: &mut Sink,
) -> Result<u64, String> {
    let regs = ndjson_registrations();
    let registrations = [&regs[0], &regs[1]];
    let catalog = CodecCatalog::new(&registrations);

    // Format identities are request-local (`String`), so a worker builds
    // its own rather than borrowing the coordinator's across the boundary.
    let format = &FormatId::try_new(jqf_codec_json::FORMAT_ID).map_err(|error| error.to_string())?;
    let dialect = &DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).map_err(|error| error.to_string())?;
    let program = program_for(".v", resources)?;
    let requirement = program
        .try_requirement(resources)
        .map_err(|error| format!("grant requirement: {:?}", error.kind()))?;
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(9), SourceKind::Input),
        "worker.ndjson",
        input,
        0,
    );
    let options = jqf_codec_json::ndjson::NdjsonDecodeOptions::try_new(None, 1 << 20)
        .map_err(|error| format!("grant ceiling: {:?}", error.kind()))?;
    let provider = jqf_codec_json::ndjson::create_record_provider(
        source,
        jqf_codec_json::ndjson::NdjsonProfile::Strict,
        options,
        DiagnosticPolicy::ErrorsOnly,
        ValidationMode::Strict,
        resources,
    )
    .map_err(|error| format!("grant provider: {:?}", error.kind()))?;
    let report = match jqf_sdk::execute(
        jqf_sdk::Request::new(
            &program,
            jqf_sdk::Input::Records {
                source: source.bytes(),
                records: provider,
                slot: jqf_codec_json::ndjson::RECORD_ROUTE_SLOT,
            },
        )
        .with_catalog(catalog)
        .with_source(source)
        .with_format(
            FormatId::try_new(format.as_str()).expect("format id"),
            DialectId::try_new(dialect.as_str()).expect("dialect id"),
        )
        .with_output_format(
            FormatId::try_new(format.as_str()).expect("format id"),
            DialectId::try_new(dialect.as_str()).expect("dialect id"),
        )
        .with_policy(PipelinePolicy {
            decode: DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: json_dialect(),
                options: None,
                allow_adjacent_values: false,
                value_separator: jqf_codec_json::VALUE_SEPARATORS,
            },
            encode_diagnostics: DiagnosticPolicy::ErrorsOnly,
            preservation: PreservationRequest::None,
            encode_options: None,
            cooperative_credits: 7,
            split: None,

            max_iterations: None,
        })
        .with_framing(FacadeFraming::item_suffix(b"\n"))
        .with_resources(resources)
        .with_requirement(&requirement),
        sink,
    )
    .map_err(|error| format!("run: {:?}", error.pipeline_failure()))?
    {
        jqf_sdk::Outcome::Served(jqf_sdk::Report::Record(report)) => report,
        other => return Err(format!("record outcome unexpected: {other:?}")),
    };
    Ok(report.records())
}

/// What one measured worker run reports back to the coordinator.
#[derive(Clone, Copy)]
struct WorkerRunMeasurement {
    published_bytes: usize,
    peaks: UsageSnapshot,
}

/// Why one measured worker run did not complete under its grant.
type GrantTooSmall = String;

/// Reserves one grant, runs a real native worker over the batch, and adopts
/// its result. `Ok(Err(_))` means the run ended at the GRANT BOUNDARY — the
/// signal the floor search bisects on; any other defect (a worker panic, an
/// accounting-invariant violation, a byte divergence from the serial drive)
/// is a harness error, never a data point.
fn measure_record_worker(
    limits: TaskGrantLimits,
    staging_capacity: usize,
    batch: &[u8],
    expected_output: &[u8],
) -> Result<Result<WorkerRunMeasurement, GrantTooSmall>, String> {
    let resources = worker_parent_resources();
    let baseline = resources.snapshot();
    let (reservation, budget) = match resources.reserve_task_grant(limits) {
        Ok(pair) => pair,
        Err(error) => return Ok(Err(format!("reservation refused: {error:?}"))),
    };
    let host = NativeWorkerHost::new(1);
    let outcome = host.scope(|scope| {
        let permit = scope.try_acquire().expect("worker permit");
        permit
            .spawn(budget, |child, control| {
                let mut child = child
                    .bind(control, WorkMeter::try_new_v1(4_096).expect("work meter"))
                    .map_err(|error| format!("bind: {error:?}"))?;
                let mut output = TaskOutputBuffer::try_with_capacity(staging_capacity, &mut child)
                    .map_err(|error| format!("staging: {error:?}"))?;
                {
                    let mut sink = GrantStagingSink { output: &mut output };
                    drive_records(batch, &mut child, &mut sink)?;
                }
                let peaks = child.snapshot();
                let detached = child
                    .detach_result(output)
                    .map_err(|error| format!("detach: {error:?}"))?;
                Ok((detached, peaks))
            })
            .map_err(|error| format!("native worker spawn: {error}"))
            .and_then(|task| {
                task.join()
                    .map_err(|_| "native worker joined with a panic".to_owned())
                    .and_then(|inner| match inner {
                        Ok(worker_out) => worker_out,
                        Err(error) => Err(format!("worker child ledger: {error:?}")),
                    })
            })
    });
    let (detached, peaks) = match outcome {
        Ok(pair) => pair,
        // Only a resource-boundary failure is a "too small" answer. A panic
        // or an invariant violation read as "too small" would bias the floor
        // bisection toward a grant that cannot actually run the batch.
        Err(reason) if is_grant_boundary_reason(&reason) => return Ok(Err(reason)),
        Err(reason) => return Err(reason),
    };
    let adopted = reservation
        .adopt(detached)
        .map_err(|error| format!("grant adoption: {error:?}"))?;
    if adopted.as_slice() != expected_output {
        return Err(format!(
            "the worker's staged output diverges from the serial reference drive: \
             worker={} bytes, serial={} bytes",
            adopted.len(),
            expected_output.len()
        ));
    }
    let published_bytes = adopted.len();
    drop(adopted);
    if resources.snapshot().memory_current_bytes() != baseline.memory_current_bytes() {
        return Err("a completed worker left residency on the parent ledger".into());
    }
    Ok(Ok(WorkerRunMeasurement { published_bytes, peaks }))
}

/// Whether one failed worker run's reason names the grant boundary itself — a
/// refused reservation or an allocation/ceiling refusal inside the worker —
/// which is the only failure class the floor bisection may read as "this
/// grant was too small". The reasons are Debug renderings of the resource
/// errors those boundaries raise, matched by their variant names.
fn is_grant_boundary_reason(reason: &str) -> bool {
    const GRANT_BOUNDARY_MARKERS: &[&str] = &[
        // `ResourceError::LimitExceeded` — a ceiling charge the grant's
        // component could not cover.
        "LimitExceeded",
        // `ResourceError::AllocationFailed` — an allocation the grant did not
        // authorize.
        "AllocationFailed",
        // `ResourceError::OutputPermitExceeded` — publication past the
        // envelope's output component.
        "OutputPermitExceeded",
        // The staging sink's own message for the same boundary.
        "exceeded its grant",
        // The coordinator refused to reserve the envelope at all.
        "reservation refused",
    ];
    GRANT_BOUNDARY_MARKERS.iter().any(|marker| reason.contains(marker))
}

fn scale_limits(limits: TaskGrantLimits, permille: u64) -> TaskGrantLimits {
    let scale = |value: u64| value.saturating_mul(permille) / 1_000;
    TaskGrantLimits {
        retained_bytes: scale(limits.retained_bytes),
        working_bytes: scale(limits.working_bytes),
        pending_io_bytes: scale(limits.pending_io_bytes),
        output_bytes: scale(limits.output_bytes),
    }
}

/// Task-grant receipt: the minimum viable grant for one record-route worker.
///
/// The MINIMUM VIABLE GRANT is a measurement, not a guess. A real native
/// worker runs main's real record route over one full concurrency window
/// (`RECORD_BATCH_ENTRIES` records totalling `RECORD_BATCH_TARGET_BYTES`),
/// first under a deliberately generous envelope so its child ledger's per
/// category PEAKS can be read, and then repeatedly under scaled multiples of
/// those peaks. A bisection over the scale finds the smallest envelope the
/// batch actually completes under — the practical floor of child ledger,
/// worker-local provider, schema prototype, batch arena, and output staging
/// together.
///
/// Two things are then pinned against it. The envelope
/// `RecordWorkerEnvelope` builds from the SDK's real batch bound must COVER
/// the measured floor componentwise, so `MEASURED_FIXED_BYTES` cannot drift
/// below reality unnoticed. And the answer the rule asked for — whether
/// `--workers 50` under the default 512 MiB ceiling runs 50 workers — is
/// answered by RESERVING fifty envelopes against a default-ceiling account and
/// reporting how many the ledger granted, not by arithmetic.
#[allow(
    clippy::too_many_lines,
    reason = "the measurement, its bisection, the pinned-envelope check, and the N=50 answer are \
              one receipt and read as one"
)]
pub(crate) fn assert_task_grants() -> Result<(), String> {
    let batch = representative_record_batch();
    let payload_bytes = batch.len();
    // The serial reference: the SAME record drive the worker runs, executed
    // on the parent side once, outside every measured region. Every worker
    // run must reproduce these bytes exactly.
    let expected_output = {
        let mut resources = worker_parent_resources();
        let mut sink = PartialSink {
            bytes: Vec::new(),
            boundaries: Vec::new(),
            reports: Vec::new(),
        };
        drive_records(&batch, &mut resources, &mut sink)?;
        sink.bytes
    };
    let generous_staging = 1 << 20;
    let generous = TaskGrantLimits {
        retained_bytes: 64 << 20,
        working_bytes: 64 << 20,
        pending_io_bytes: 8 << 20,
        output_bytes: generous_staging,
    };
    let measurement = measure_record_worker(
        generous,
        usize::try_from(generous_staging).unwrap_or(usize::MAX),
        &batch,
        &expected_output,
    )?
    .map_err(|reason| format!("a generous envelope failed to run one record batch: {reason}"))?;

    let peaks = measurement.peaks;
    let published = u64::try_from(measurement.published_bytes).map_err(|_| "published bytes exceed u64".to_string())?;
    if published != expected_output.len() as u64 {
        return Err(format!(
            "the generous run published {published} bytes, the serial drive {}",
            expected_output.len()
        ));
    }
    // Output staging is separated from the rest of the PendingIo group: the
    // exact staging need is the byte count the drive published.
    let floor = TaskGrantLimits {
        retained_bytes: peaks.memory(MemoryCategory::Retained).peak(),
        working_bytes: peaks.memory(MemoryCategory::Working).peak() + peaks.memory(MemoryCategory::Diagnostic).peak(),
        pending_io_bytes: peaks
            .memory(MemoryCategory::PendingIo)
            .peak()
            .saturating_sub(generous_staging),
        output_bytes: published,
    };

    let staging = usize::try_from(published).map_err(|_| "staging exceeds usize".to_string())?;
    let attempt = |permille: u64| -> Result<bool, String> {
        match measure_record_worker(scale_limits(floor, permille), staging, &batch, &expected_output)? {
            Ok(_) => Ok(true),
            Err(reason) => {
                if is_grant_boundary_reason(&reason) {
                    Ok(false)
                } else {
                    Err(format!("a bisection run failed for a non-grant reason: {reason}"))
                }
            }
        }
    };

    let mut high = 1_000;
    while !attempt(high)? {
        high *= 2;
        if high > 64_000 {
            return Err("no scaled multiple of the measured peaks ran one record batch".into());
        }
    }
    let mut low = 0;
    while high - low > 16 {
        let middle = low + (high - low) / 2;
        if attempt(middle)? {
            high = middle;
        } else {
            low = middle;
        }
    }
    let min_viable = scale_limits(floor, high);
    let min_viable_bytes = min_viable.total_memory_bytes().map_err(|error| format!("{error:?}"))?;

    // The pinned envelope must cover the measured floor componentwise.
    let envelope = RecordWorkerEnvelope::try_new(RECORD_BATCH_TARGET_BYTES, RecordWorkerEnvelope::MEASURED_FIXED_BYTES)
        .map_err(|error| format!("window envelope: {error:?}"))?;
    let pinned = envelope.task_grant_limits().map_err(|error| format!("{error:?}"))?;
    if pinned.retained_bytes < min_viable.retained_bytes
        || pinned.working_bytes < min_viable.working_bytes
        || pinned.pending_io_bytes < min_viable.pending_io_bytes
        || pinned.output_bytes < min_viable.output_bytes
    {
        return Err(format!(
            "the pinned worker envelope no longer covers the measured floor: pinned={pinned:?} \
             measured={min_viable:?}"
        ));
    }
    let pinned_bytes = pinned.total_memory_bytes().map_err(|error| format!("{error:?}"))?;

    // The N=50 answer, reserved rather than computed.
    let resources = worker_parent_resources();
    let fifty = jqf_runtime::workers::reserve_record_worker_grants(50, envelope, &resources)
        .map_err(|error| format!("{error:?}"))?;
    let granted_fifty = fifty.report().granted();
    let degraded = fifty.report().degraded();
    drop(fifty);
    if granted_fifty != 50 {
        return Err(format!(
            "the default ceiling granted {granted_fifty} of 50 window-sized envelopes"
        ));
    }
    if degraded {
        return Err("the default ceiling degraded the 50-worker reservation".into());
    }

    // The parent-ceiling degradation seam is gone: a grant no
    // longer reserves against the parent, so nothing refuses at grant time —
    // the ambient allocator refuses at the `GlobalAlloc` boundary instead.
    // The N=50 answer above is the surviving worker-count receipt.

    println!(
        "task-grant: min_viable_bytes={min_viable_bytes} retained={} working={} pending_io={} \
         output={} scale_permille={high} measured_over=records={},payload_bytes={payload_bytes},\
published_bytes={published} pinned_envelope_bytes={pinned_bytes} \
workers_at_default_ceiling=requested=50,granted={granted_fifty},degraded={degraded}",
        min_viable.retained_bytes,
        min_viable.working_bytes,
        min_viable.pending_io_bytes,
        min_viable.output_bytes,
        RECORD_BATCH_ENTRIES,
    );
    Ok(())
}
