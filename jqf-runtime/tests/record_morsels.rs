//! The record-parallel lane's byte-identity laws.
//!
//! Every case here asserts the same thing from a different angle: what a request publishes, what it reports, and how it
//! ends do not depend on how many workers ran it.

use jqf_codec_json::ndjson::{NdjsonProfile, NdjsonTerminator};
use jqf_codec_json::seq::JsonSeqProfile;
use jqf_codec_json::{JsonEncodeOptions, JsonIndent};
use jqf_engine::{CodecRequirementPolicy, CompiledProgram, try_compile_program};
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_runtime::records::{
    OutputTarget, PlanDecision, RecordDriveError, RecordDriveSpec, RecordInputKind, RecordOutputSpec, RecordRunModel,
    WorkerRequest, execute_record_request, plan_request,
};
use jqf_sdk::{EncodedItemReport, ItemSink, RecordIssueReport, SequenceValueError};

static CONTINUE: ContinueControl = ContinueControl;

const CREDITS: u32 = 4_096;

/// Captures everything a host would observe: published bytes AND the ordered diagnostics a CLI would mirror to stderr.
#[derive(Default)]
struct CapturingSink {
    bytes: Vec<u8>,
    diagnostics: Vec<String>,
}

impl ItemSink for CapturingSink {
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

    fn report_value_error(&mut self, error: SequenceValueError) -> Result<(), Self::Error> {
        self.diagnostics.push(format!(
            "error(line {}){}: {}",
            error.input_line(),
            error.frame_note(),
            error.message()
        ));
        Ok(())
    }

    fn report_record_issue(&mut self, issue: RecordIssueReport<'_>) -> Result<(), Self::Error> {
        self.diagnostics.push(format!(
            "issue(record {}, byte {}, {:?}, {:?})",
            issue.ordinal(),
            issue.offset(),
            issue.severity(),
            issue.code()
        ));
        Ok(())
    }
}

fn resources() -> ResourceContext<'static> {
    ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(256 << 20, 256 << 20, 512 << 20, 0, 10_000))
            .expect("request account"),
        &CONTINUE,
        WorkMeter::try_new_v1(CREDITS).expect("work meter"),
    )
    .expect("resources")
}

fn compile(source: &str, resources: &ResourceContext<'_>) -> CompiledProgram {
    try_compile_program(
        source,
        CodecRequirementPolicy::new(
            jqf_codec_core::ValidationMode::Strict,
            jqf_codec_core::DiagnosticPolicy::ErrorsOnly,
        ),
        resources,
    )
    .expect("program compiles")
}

fn spec_with_kind(input: &[u8], kind: RecordInputKind) -> RecordDriveSpec<'_> {
    RecordDriveSpec {
        input,
        source_name: "<stdin>",
        files: None,
        kind,
        profile: NdjsonProfile::Strict,
        json_seq_profile: JsonSeqProfile::Strict,
        csv_delimiter: None,
        csv_textdata: false,
        max_record_bytes: 256 << 20,
        catalog: jqf_runtime::records::install_record_catalog(
            jqf_codec_json::registration().expect("json"),
            jqf_codec_json::ndjson::registration().expect("ndjson"),
            jqf_codec_json::seq::registration().expect("json-seq"),
            jqf_codec_delimited::registration().expect("csv"),
            jqf_codec_delimited::registration_tsv().expect("tsv"),
            jqf_codec_render::registration().expect("render"),
            jqf_codec_yaml::registration().expect("yaml"),
            jqf_codec_xml::registration().expect("xml"),
            jqf_codec_html::registration().expect("html"),
        ),
        output: RecordOutputSpec {
            target: OutputTarget::Json,
            terminator: NdjsonTerminator::Lf,
            // Compact: these tests compare a morsel drive's bytes against the serial drive's, and the render style is
            // not what they are about.
            json: JsonEncodeOptions {
                indent: JsonIndent::Compact,
                raw_strings: false,
                sort_keys: false,
                ascii_output: false,
                raw_output_nul: false,
            },
            no_newline: false,
        },
        model: RecordRunModel::PerRecord,
        edit: false,
        cooperative_credits: CREDITS,
        max_iterations: None,
    }
}

fn spec(input: &[u8]) -> RecordDriveSpec<'_> {
    spec_with_kind(input, RecordInputKind::Ndjson)
}

/// A stream large enough to hold many morsels at the planner's real floor.
fn stream(records: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    for index in 0..records {
        bytes.extend_from_slice(format!("{{\"v\":{index},\"pad\":\"{}\"}}\n", "x".repeat(48)).as_bytes());
    }
    bytes
}

/// Replaces one record's payload, keeping the framing intact.
fn replace_record(input: &[u8], ordinal: usize, payload: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for (index, line) in input.split_inclusive(|&byte| byte == b'\n').enumerate() {
        if index == ordinal {
            out.extend_from_slice(payload.as_bytes());
            out.push(b'\n');
        } else {
            out.extend_from_slice(line);
        }
    }
    out
}

struct Run {
    sink: CapturingSink,
    failed: bool,
    records: u64,
    error_issues: u64,
    decision: PlanDecision,
    fell_back: bool,
}

fn run(program: &str, input: &[u8], request: Option<WorkerRequest>) -> Run {
    let mut resources = resources();
    let compiled = compile(program, &resources);
    let drive = spec(input);
    let plan = match request {
        None => jqf_runtime::records::ParallelPlan::serial(
            WorkerRequest::Explicit(1),
            PlanDecision::SwitchedOff,
            input.len() as u64,
        ),
        Some(request) => {
            // Any program whose every overload is `Effects::Pure` is per-record independent, so the collect/fan-out
            // shapes below take the lane.
            let ineligible = (!compiled.is_morsel_eligible()).then_some(PlanDecision::ProgramIneligible);
            plan_request(request, input.len() as u64, ineligible)
        }
    };
    let mut sink = CapturingSink::default();
    let outcome = execute_record_request(drive, plan, &compiled, &mut resources, &mut sink, None);
    match outcome {
        Ok(report) => {
            // The relay facts this file asserts are the Display line the CLI prints; the report's field accessors stay
            // private to the crate.
            let relay_line = format!("{report}");
            Run {
                sink,
                failed: false,
                records: report.records(),
                error_issues: report.error_issues(),
                decision: plan.decision(),
                fell_back: !relay_line.contains("fallback=none"),
            }
        }
        Err(
            RecordDriveError::Pipeline(_)
            | RecordDriveError::Setup { .. }
            | RecordDriveError::Resource(_)
            | RecordDriveError::Control(_),
        ) => Run {
            sink,
            failed: true,
            records: 0,
            error_issues: 0,
            decision: plan.decision(),
            fell_back: false,
        },
        // `CapturingSink::Error` is `Infallible`, so this arm is unreachable by construction; the empty match disposes
        // of the uninhabited payload without discarding any real error.
        Err(RecordDriveError::Sink(error)) => match error {},
    }
}

fn assert_matches_serial(program: &str, input: &[u8], widths: &[usize]) {
    let serial = run(program, input, None);
    for &width in widths {
        let parallel = run(program, input, Some(WorkerRequest::Explicit(width)));
        assert_eq!(
            parallel.sink.bytes, serial.sink.bytes,
            "published bytes diverged at workers={width}"
        );
        assert_eq!(
            parallel.sink.diagnostics, serial.sink.diagnostics,
            "ordered diagnostics diverged at workers={width}"
        );
        assert_eq!(parallel.failed, serial.failed, "exit class diverged at workers={width}");
        assert_eq!(
            parallel.records, serial.records,
            "record count diverged at workers={width}"
        );
        assert_eq!(
            parallel.error_issues, serial.error_issues,
            "forced exit class diverged at workers={width}"
        );
    }
}

/// Collect and fan-out shapes over per-record state relay byte-identically at every width: a program whose every
/// overload is `Effects::Pure` is morsel-eligible even where it calls builtins or lowers through `map`.
#[test]
fn collect_and_fan_out_over_per_record_state_matches_serial_at_every_width() {
    let input = stream(2000);
    for program in [
        ".[] | .v",
        "[.[] | .v] | length",
        "[.[] | .v]",
        ".[] | length",
        "map(.v) | length",
    ] {
        assert_matches_serial(program, &input, &[1, 2, 4]);
    }
}

/// An IMPURE program must decline the lane even though it is a static path: `stderr` writes to a host channel the
/// worker has none of, so its bytes on a worker could never be serial's.
#[test]
fn an_impure_program_declines_the_lane() {
    let resources = resources();
    let compiled = compile("stderr", &resources);
    let plan = plan_request(
        WorkerRequest::Explicit(2),
        stream(2000).len() as u64,
        (!compiled.is_morsel_eligible()).then_some(PlanDecision::ProgramIneligible),
    );
    assert_eq!(plan.decision(), PlanDecision::ProgramIneligible);
    let compiled = compile("input", &resources);
    let plan = plan_request(
        WorkerRequest::Explicit(2),
        stream(2000).len() as u64,
        (!compiled.is_morsel_eligible()).then_some(PlanDecision::ProgramIneligible),
    );
    assert_eq!(plan.decision(), PlanDecision::ProgramIneligible);
}

/// An unterminated quoted CSV field blocks the boundary scan, so the morsel containing it grows to end-of-input — past
/// the relay window's input-bytes budget (2 x `morsel_bytes`). Before the fix the relay retried the never- fitting
/// dispatch forever, livelocking `--workers 1` instead of yielding; it must yield to serial, which reproduces serial's
/// bytes and exit class.
#[test]
fn an_oversized_csv_morsel_yields_to_serial_instead_of_livelocking() {
    let mut input = Vec::new();
    for index in 0..60_000 {
        input.extend_from_slice(format!("i{index},v{index}\n").as_bytes());
    }
    // Position the unterminated quote in the SECOND morsel so a clean morsel is published before the oversized one is
    // reached.
    let insert = 200_000;
    input.splice(insert..insert, b"\"unterminated\n".iter().copied());

    let serial = run_csv(".v?", &input, None);
    let parallel = run_csv(".v?", &input, Some(WorkerRequest::Explicit(1)));
    assert_eq!(parallel.sink.bytes, serial.sink.bytes, "published bytes diverged");
    assert_eq!(
        parallel.sink.diagnostics, serial.sink.diagnostics,
        "ordered diagnostics diverged"
    );
    assert_eq!(parallel.failed, serial.failed, "exit class diverged");
}

fn run_csv(program: &str, input: &[u8], request: Option<WorkerRequest>) -> Run {
    let mut resources = resources();
    let compiled = compile(program, &resources);
    let drive = spec_with_kind(
        input,
        RecordInputKind::Csv {
            header: false,
            tsv: false,
        },
    );
    let plan = match request {
        None => jqf_runtime::records::ParallelPlan::serial(
            WorkerRequest::Explicit(1),
            PlanDecision::SwitchedOff,
            input.len() as u64,
        ),
        Some(request) => {
            let ineligible = (!compiled.is_morsel_eligible()).then_some(PlanDecision::ProgramIneligible);
            plan_request(request, input.len() as u64, ineligible)
        }
    };
    let mut sink = CapturingSink::default();
    let outcome = execute_record_request(drive, plan, &compiled, &mut resources, &mut sink, None);
    match outcome {
        Ok(report) => {
            let relay_line = format!("{report}");
            Run {
                sink,
                failed: false,
                records: report.records(),
                error_issues: report.error_issues(),
                decision: plan.decision(),
                fell_back: !relay_line.contains("fallback=none"),
            }
        }
        Err(
            RecordDriveError::Pipeline(_)
            | RecordDriveError::Setup { .. }
            | RecordDriveError::Resource(_)
            | RecordDriveError::Control(_),
        ) => Run {
            sink,
            failed: true,
            records: 0,
            error_issues: 0,
            decision: plan.decision(),
            fell_back: false,
        },
        // `CapturingSink::Error` is `Infallible`, so this arm is unreachable by construction; the empty match disposes
        // of the uninhabited payload without discarding any real error.
        Err(RecordDriveError::Sink(error)) => match error {},
    }
}

#[test]
fn a_clean_stream_is_byte_identical_at_every_width() {
    let input = stream(20_000);
    assert!(input.len() > (1 << 20), "the fixture spans many morsels");
    assert_matches_serial(".v", &input, &[1, 2, 4, 8]);

    let wide = run(".v", &input, Some(WorkerRequest::Explicit(4)));
    assert_eq!(wide.decision, PlanDecision::Parallel);
    assert!(!wide.fell_back, "a clean stream never yields to serial");
}

#[test]
fn the_degenerate_single_worker_plan_is_byte_identical_to_serial() {
    let input = stream(20_000);
    let serial = run(".v", &input, None);
    let single = run(".v", &input, Some(WorkerRequest::Explicit(1)));
    assert_eq!(single.decision, PlanDecision::Parallel);
    assert_eq!(single.sink.bytes, serial.sink.bytes);
}

#[test]
fn oversubscription_beyond_the_core_count_still_matches_serial() {
    let input = stream(20_000);
    assert_matches_serial(".v", &input, &[50]);
}

#[test]
fn a_malformed_record_publishes_the_exact_serial_prefix_and_class() {
    // The fault sits deep in the stream, so several clean morsels have already been relayed when the terminal reaches
    // the frontier.
    let input = replace_record(&stream(20_000), 15_000, "{\"v\":");
    assert_matches_serial(".v", &input, &[1, 2, 4, 8]);

    let parallel = run(".v", &input, Some(WorkerRequest::Explicit(4)));
    assert!(parallel.failed, "a malformed payload is terminal in strict");
    assert!(!parallel.sink.bytes.is_empty(), "earlier records stay published");
}

#[test]
fn a_per_record_runtime_error_yields_to_serial_with_its_line_number() {
    // `.v` over a scalar record is a per-record type mismatch: reported to the sink with an absolute input line number,
    // then the stream continues.
    let input = replace_record(&stream(20_000), 9_000, "42");
    assert_matches_serial(".v", &input, &[2, 4]);

    let parallel = run(".v", &input, Some(WorkerRequest::Explicit(4)));
    assert!(parallel.fell_back, "a diagnostic yields the request to serial");
    assert_eq!(parallel.sink.diagnostics.len(), 1);
    assert!(
        parallel.sink.diagnostics[0].contains("line 9001"),
        "the yielded drive renders serial's absolute line number: {:?}",
        parallel.sink.diagnostics[0]
    );
}

#[test]
fn a_blank_record_yields_to_serial_and_keeps_the_strict_class() {
    let input = replace_record(&stream(20_000), 12_000, "");
    assert_matches_serial(".v", &input, &[2, 4]);
}

#[test]
fn impure_programs_decline_the_lane_and_pure_iteration_engages_it() {
    let input = stream(20_000);
    // `.v[]?` is pure iteration, so it is per-record independent and engages the lane. The lane's ineligible class is
    // the IMPURE one — a program that observes the world outside the record in front of it.
    let iterating = run(".v[]?", &input, Some(WorkerRequest::Auto));
    assert_eq!(iterating.decision, PlanDecision::Parallel);

    let serial = run(".v[]?", &input, None);
    assert_eq!(iterating.sink.bytes, serial.sink.bytes);

    // Decisions for the whole impure vocabulary; bytes only for the DETERMINISTIC members (`now` observes the wall
    // clock, so two runs of it can never be byte-compared).
    for impure in ["stderr", "input", "now", "env", "debug"] {
        let declined = run(impure, &input, Some(WorkerRequest::Auto));
        assert_eq!(
            declined.decision,
            PlanDecision::ProgramIneligible,
            "{impure} must decline the morsel lane"
        );
    }
    for impure in ["stderr", "debug", "env", "input"] {
        let declined = run(impure, &input, Some(WorkerRequest::Auto));
        let serial = run(impure, &input, None);
        assert_eq!(declined.sink.bytes, serial.sink.bytes);
        assert_eq!(declined.sink.diagnostics, serial.sink.diagnostics);
    }
}

#[test]
fn auto_keeps_a_small_input_on_the_serial_path() {
    let input = stream(64);
    let auto = run(".v", &input, Some(WorkerRequest::Auto));
    assert_eq!(auto.decision, PlanDecision::BelowBreakEven);

    let serial = run(".v", &input, None);
    assert_eq!(auto.sink.bytes, serial.sink.bytes);
}

#[test]
fn identity_is_inside_the_morsel_static_path_class() {
    let input = stream(20_000);
    assert_matches_serial(".", &input, &[2, 4]);

    let auto = run(".", &input, Some(WorkerRequest::Auto));
    assert_eq!(auto.decision, PlanDecision::Parallel);
}

/// A split destination cannot cross the relay: even when the planner said parallel, the runtime seam takes the serial
/// drive and keeps the split program. The request must not spawn workers that would drop the names.
#[test]
fn a_split_plus_parallel_request_refuses_structurally() {
    let input = stream(20_000);
    let mut resources = resources();
    let compiled = compile(".v", &resources);
    // Unique per published item (the number `.v` already extracted). A constant name trips the serial path's collision
    // law, which is a different contract than the parallel-seam refusal.
    let split = compile("tostring", &resources);
    let drive = spec(&input);
    let plan = plan_request(
        WorkerRequest::Explicit(4),
        input.len() as u64,
        (!compiled.is_morsel_eligible()).then_some(PlanDecision::ProgramIneligible),
    );
    assert!(
        plan.is_parallel(),
        "the planner would have engaged; the seam must refuse"
    );
    let mut sink = CapturingSink::default();
    let report = execute_record_request(drive, plan, &compiled, &mut resources, &mut sink, Some(&split))
        .expect("split+parallel takes serial, not an error");
    assert!(
        format!("{report}").contains("morsels_published=0"),
        "the relay must not engage under a split destination: {report}"
    );
    let serial = run(".v", &input, None);
    assert_eq!(sink.bytes, serial.sink.bytes);
}

/// The frame-transition ceiling (`--max-iterations`) is ENFORCED on the record drive: a program whose iteration count
/// crosses it fails the request, exactly as the serial tail refuses the same program — never a silently uncapped answer
/// on the record lane.
#[test]
fn max_iterations_ceiling_refuses_the_record_drive() {
    let input: &[u8] = b"[1]\n";
    let program = "[range(1000)] | reduce .[] as $x (0; . + 1)";
    // The uncapped twin answers.
    let uncapped = run(program, input, None);
    assert!(!uncapped.failed, "the same program is fine without a ceiling");
    // The capped twin refuses.
    let mut resources = resources();
    let compiled = compile(program, &resources);
    let mut drive = spec(input);
    drive.max_iterations = Some(50);
    let plan = jqf_runtime::records::ParallelPlan::serial(
        WorkerRequest::Explicit(1),
        PlanDecision::SwitchedOff,
        input.len() as u64,
    );
    let mut sink = CapturingSink::default();
    let outcome = execute_record_request(drive, plan, &compiled, &mut resources, &mut sink, None);
    assert!(
        outcome.is_err(),
        "a ceiling-crossing program must fail the record request"
    );
}
