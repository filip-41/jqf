use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};

use jqf_codec_core::{DecodeRequest, DiagnosticPolicy, PreservationRequest, ValidationMode};
use jqf_data::{DialectId, FormatId};
use jqf_engine::{CodecRequirementPolicy, try_compile_program};
use jqf_resource::task::{DetachedTaskBuffer, TaskChildAccount, TaskGrantLimits, TaskOutputBuffer};
use jqf_resource::{
    ContinueControl, Control, ControlOutcome, RequestAccount, ResourceContext, ResourceError, ResourceLimits, WorkMeter,
};
use jqf_runtime::workers::{NativeWorkerControl, NativeWorkerHost};
use jqf_sdk::{CodecCatalog, EncodedItemReport, FacadeFraming, ItemSink, PipelinePolicy};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

static CONTINUE: ContinueControl = ContinueControl;

/// One representative record batch: the shape the record relay hands a worker.
const RECORDS: &[u8] = b"{\"v\":1}\n{\"v\":[2,3]}\n{\"v\":null}\n{\"v\":\"four\"}\n{\"v\":{\"n\":5}}\n";

fn work() -> WorkMeter {
    WorkMeter::try_new_v1(4_096).expect("valid work meter")
}

fn parent_resources() -> ResourceContext<'static> {
    ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, 32 << 20, u64::MAX, 64))
            .expect("parent account"),
        &CONTINUE,
        work(),
    )
    .expect("parent resources")
}

fn grant_limits(output_bytes: u64) -> TaskGrantLimits {
    TaskGrantLimits {
        retained_bytes: RequestAccount::minimum_memory_bytes() + (1 << 20),
        working_bytes: 1 << 20,
        pending_io_bytes: 1 << 20,
        output_bytes,
    }
}

/// Publication staging for one worker: every byte the record drive publishes lands in capacity the worker's own grant
/// already committed.
struct GrantStagingSink<'output> {
    output: &'output mut TaskOutputBuffer,
}

impl ItemSink for GrantStagingSink<'_> {
    type Error = ResourceError;

    fn begin_item(&mut self, _index: u64) -> Result<(), Self::Error> {
        Ok(())
    }

    fn write(&mut self, bytes: &[u8]) -> Result<usize, Self::Error> {
        self.output.try_append_within_capacity(bytes)?;
        Ok(bytes.len())
    }

    fn finish_item(&mut self, _index: u64, _report: EncodedItemReport) -> Result<(), Self::Error> {
        Ok(())
    }
}

struct VecSink {
    bytes: Vec<u8>,
}

impl ItemSink for VecSink {
    type Error = ResourceError;

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

fn record_policy() -> PipelinePolicy<'static> {
    let dialect: &'static DialectId = Box::leak(Box::new(DialectId::try_new("rfc8259").expect("dialect")));
    PipelinePolicy {
        decode: DecodeRequest {
            validation: ValidationMode::Strict,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            dialect,
            options: None,
            allow_adjacent_values: false,
            value_separator: &[],
        },
        encode_diagnostics: DiagnosticPolicy::ErrorsOnly,
        preservation: PreservationRequest::None,
        encode_options: None,
        cooperative_credits: 7,
        split: None,

        max_iterations: None,
    }
}

fn record_source(input: &[u8]) -> ResolvedSource<'_> {
    ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "worker.ndjson",
        input,
        0,
    )
}

/// Drives the record route through `jqf_sdk::execute` against `resources`, publishing through `sink`.
fn drive_records<Sink: ItemSink<Error = ResourceError>>(
    program_source: &str,
    input: &[u8],
    resources: &mut ResourceContext<'_>,
    sink: &mut Sink,
) -> Result<u64, String> {
    let json = jqf_codec_json::registration().map_err(|error| format!("{error:?}"))?;
    let streams = jqf_codec_json::ndjson::registration().map_err(|error| format!("{error:?}"))?;
    let registrations = [&json, &streams];
    let catalog = CodecCatalog::new(&registrations);
    let format = || FormatId::try_new(jqf_codec_json::FORMAT_ID).expect("format id");
    let dialect = || DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect id");

    let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    let program = try_compile_program(program_source, policy, resources).map_err(|e| format!("{e}"))?;
    let requirement = program
        .try_requirement(resources)
        .map_err(|error| format!("requirement: {:?}", error.kind()))?;
    let source = record_source(input);
    let options = jqf_codec_json::ndjson::NdjsonDecodeOptions::try_new(None, 1 << 20)
        .map_err(|error| format!("ceiling: {:?}", error.kind()))?;
    let provider = jqf_codec_json::ndjson::create_record_provider(
        source,
        jqf_codec_json::ndjson::NdjsonProfile::Strict,
        options,
        DiagnosticPolicy::ErrorsOnly,
        ValidationMode::Strict,
        resources,
    )
    .map_err(|error| format!("provider: {:?}", error.kind()))?;
    let request = jqf_sdk::Request::new(
        &program,
        jqf_sdk::Input::Records {
            source: source.bytes(),
            records: provider,
            slot: jqf_codec_json::ndjson::RECORD_ROUTE_SLOT,
        },
    )
    .with_catalog(catalog)
    .with_source(source)
    .with_format(format(), dialect())
    .with_output_format(format(), dialect())
    .with_policy(record_policy())
    .with_framing(FacadeFraming::item_suffix(b"\n"))
    .with_resources(resources)
    .with_requirement(&requirement);
    let outcome =
        jqf_sdk::execute(request, sink).map_err(|error| format!("record sequence: {:?}", error.pipeline_failure()))?;
    let report = match outcome {
        jqf_sdk::Outcome::Served(jqf_sdk::Report::Record(report)) => report,
        other => return Err(format!("record outcome unexpected: {other:?}")),
    };
    Ok(report.records())
}

/// The complete worker body: a child ledger in, a detached buffer out.
///
/// Nothing request-local crosses the thread boundary. The worker receives the child ledger the spawn seam opened from
/// the linear grant and bound it against worker-local control and work; it compiles its own program, builds its own
/// provider and encoder, and stages every published byte in output capacity the grant already committed.
fn record_worker(
    program_source: &'static str,
    input: &'static [u8],
    output_capacity: usize,
    child: TaskChildAccount,
    control: &NativeWorkerControl,
) -> Result<DetachedTaskBuffer, String> {
    let mut child = child.bind(control, work()).map_err(|error| format!("{error:?}"))?;
    let mut output = TaskOutputBuffer::try_with_capacity(output_capacity, &mut child)
        .map_err(|error| format!("output staging: {error:?}"))?;
    {
        let mut sink = GrantStagingSink { output: &mut output };
        drive_records(program_source, input, &mut child, &mut sink)?;
    }
    child
        .detach_result(output)
        .map_err(|error| format!("detach: {error:?}"))
}

#[test]
fn explicit_worker_count_is_the_exact_permit_ceiling() {
    let workers = NativeWorkerHost::new(2);

    workers.scope(|scope| {
        let first = scope.try_acquire().expect("first permit");
        let second = scope.try_acquire().expect("second permit");

        assert_eq!(scope.available_permits(), 0);
        assert!(scope.try_acquire().is_none());

        drop(first);
        assert_eq!(scope.available_permits(), 1);
        drop(second);
        assert_eq!(scope.available_permits(), 2);
    });
}

#[test]
fn dropping_a_live_task_cancels_and_joins_before_releasing_its_permit() {
    let resources = parent_resources();
    let baseline = resources.snapshot();
    let mut reservations = Vec::new();
    let mut budgets = Vec::new();
    for _ in 0..2 {
        let (reservation, budget) = resources.reserve_task_grant(grant_limits(0)).expect("task grant");
        reservations.push(reservation);
        budgets.push(budget);
    }

    let started = mpsc::channel();
    let completed = Arc::new(AtomicUsize::new(0));
    let workers = NativeWorkerHost::new(2);
    workers.scope(|scope| {
        let mut tasks = Vec::new();
        for budget in budgets {
            let permit = scope.try_acquire().expect("bounded permit");
            let started = started.0.clone();
            let completed = Arc::clone(&completed);
            tasks.push(
                permit
                    .spawn(budget, move |_child, control| {
                        started.send(()).expect("start observed");
                        while control.check() == ControlOutcome::Continue {
                            std::thread::yield_now();
                        }
                        completed.fetch_add(1, Ordering::SeqCst);
                    })
                    .expect("native worker spawns"),
            );
        }
        drop(started.0);
        started.1.recv().expect("first worker started");
        started.1.recv().expect("second worker started");

        drop(tasks);

        assert_eq!(completed.load(Ordering::SeqCst), 2);
        assert!(scope.is_cancelled());
        assert_eq!(scope.available_permits(), 2);
        assert!(
            scope.try_acquire().is_none(),
            "cancelled sessions never dispatch replacement work"
        );
    });

    drop(reservations);
    assert_eq!(
        resources.snapshot().memory_current_bytes(),
        baseline.memory_current_bytes()
    );
}

#[test]
fn a_worker_runs_the_record_route_from_a_numeric_grant_and_the_parent_adopts_its_bytes() {
    let mut resources = parent_resources();
    let baseline = resources.snapshot();

    let mut serial = VecSink { bytes: Vec::new() };
    let serial_records = drive_records(".v", RECORDS, &mut resources, &mut serial).expect("serial record drive");
    assert_eq!(serial_records, 5);

    let (reservation, budget) = resources.reserve_task_grant(grant_limits(4 << 10)).expect("task grant");
    let workers = NativeWorkerHost::new(1);
    let detached = workers
        .scope(|scope| {
            let permit = scope.try_acquire().expect("worker permit");
            let task = permit
                .spawn(budget, move |child, control| {
                    record_worker(".v", RECORDS, 64, child, control)
                })
                .expect("native worker spawns");
            let detached = task
                .join()
                .expect("native worker joins")
                .expect("worker child ledger opens");
            assert_eq!(scope.available_permits(), 1);
            assert!(!scope.is_cancelled());
            detached
        })
        .expect("worker record drive");

    let adopted = reservation.adopt(detached).expect("parent adopts worker result");
    assert_eq!(
        adopted.as_slice(),
        serial.bytes.as_slice(),
        "a worker's record bytes are the serial drive's bytes"
    );
    let after_adoption = resources.snapshot();
    let keep = u64::try_from(adopted.capacity()).expect("capacity");
    assert_eq!(
        after_adoption.memory_current_bytes(),
        baseline.memory_current_bytes() + keep,
        "adoption keeps the result capacity on the parent until the buffer drops"
    );

    drop(adopted);
    assert_eq!(
        resources.snapshot().memory_current_bytes(),
        baseline.memory_current_bytes()
    );
}

#[test]
fn a_worker_whose_output_staging_overflows_its_grant_fails_without_publishing() {
    let resources = parent_resources();
    let baseline = resources.snapshot();
    // Eight bytes cannot hold the five records the drive publishes, and the staging buffer never grows past the grant's
    // exact output component.
    let (reservation, budget) = resources.reserve_task_grant(grant_limits(8)).expect("task grant");
    let workers = NativeWorkerHost::new(1);
    let outcome = workers.scope(|scope| {
        let permit = scope.try_acquire().expect("worker permit");
        permit
            .spawn(budget, move |child, control| {
                record_worker(".v", RECORDS, 8, child, control)
            })
            .expect("native worker spawns")
            .join()
            .expect("native worker joins")
            .unwrap_or_else(|error| Err(format!("worker child ledger: {error:?}")))
    });

    assert!(
        outcome.is_err(),
        "a grant's output component binds a worker's published bytes"
    );
    drop(reservation);
    assert_eq!(
        resources.snapshot().memory_current_bytes(),
        baseline.memory_current_bytes()
    );
}

#[test]
fn a_panicking_worker_returns_its_grant_and_its_permit() {
    let resources = parent_resources();
    let baseline = resources.snapshot();
    let (reservation, budget) = resources.reserve_task_grant(grant_limits(1 << 10)).expect("task grant");

    let workers = NativeWorkerHost::new(1);
    workers.scope(|scope| {
        let permit = scope.try_acquire().expect("worker permit");
        let task = permit
            .spawn(budget, move |child, _control| {
                // The child ledger is dropped by the unwind, exactly as a worker returning early would drop it.
                drop(child);
                panic!("worker fault");
            })
            .expect("native worker spawns");
        let outcome = task.join();
        assert!(outcome.is_err(), "a worker panic surfaces at the join");
        assert_eq!(scope.available_permits(), 1, "a faulted worker releases its permit");
    });

    // Dropping the reservation releases the admission hold; a panicked worker never adopted, so the parent returns to
    // its baseline.
    drop(reservation);
    assert_eq!(
        resources.snapshot().memory_current_bytes(),
        baseline.memory_current_bytes()
    );
}

#[test]
fn a_cancelled_worker_publishes_nothing_and_returns_its_grant() {
    let resources = parent_resources();
    let baseline = resources.snapshot();
    let (reservation, budget) = resources.reserve_task_grant(grant_limits(4 << 10)).expect("task grant");

    let workers = NativeWorkerHost::new(1);
    let outcome = workers.scope(|scope| {
        scope.cancel();
        let permit = scope.try_acquire();
        assert!(permit.is_none(), "a cancelled scope never dispatches new work");
        // Cancellation is also visible to a worker already bound to the scope's control: the child ledger refuses to
        // bind at all.
        let control = NativeWorkerControl::default();
        control.cancel();
        let child = budget.open_child().expect("open child");
        record_worker(".v", RECORDS, 64, child, &control)
    });

    assert!(outcome.is_err(), "a cancelled worker publishes nothing");
    drop(reservation);
    assert_eq!(
        resources.snapshot().memory_current_bytes(),
        baseline.memory_current_bytes()
    );
}
