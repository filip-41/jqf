//! The explicit record-format route (NDJSON/CSV): the framer owns the physical stream, and every payload is decoded as
//! a COMPLETE document over the record's own byte range. None of the demand-projection rungs apply — each of them
//! validates the WHOLE input as one document, which a record stream deliberately is not.

use std::io::{self, Write as IoWrite};

use jqf_codec_json::ndjson::NdjsonProfile;
use jqf_codec_json::seq::JsonSeqProfile;
use jqf_data::{DialectId, FormatId};
use jqf_runtime::records::{
    OutputTarget, ParallelPlan, PlanDecision, RecordDriveError, RecordDriveSpec, RecordInputKind, RecordOutputSpec,
    RecordRunModel, execute_record_request, plan_request,
};

use crate::args::CliFormat;
use crate::errors::{CliFailure, ExitClass, render_failure, requirement_failure};
use crate::output::StdoutSink;
use crate::plan::{COOPERATIVE_CREDITS, RouteContext, RouteOutcome};
use crate::record_route;
use crate::{eprint_line_buffered, eprint_value_error, flush_stderr};
use jqf_sdk::{EncodedItemReport, ItemSink, RecordIssueReport, SequenceValueError};

/// One explicit record-format invocation, as the facade sees it.
#[derive(Clone, Copy)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "the record request carries one bool per CLI flag a drive must read; bundling \
              them would hide the flag surface from the planner that branches on it"
)]
pub(crate) struct RecordRequest<'request> {
    /// Which record format this request frames.
    pub(crate) kind: RecordInputKind,
    /// The NDJSON profile, meaningful only when `kind` is `Ndjson`.
    pub(crate) profile: NdjsonProfile,
    /// The json-seq profile, meaningful only when `kind` is `JsonSeq`.
    pub(crate) json_seq_profile: jqf_codec_json::seq::JsonSeqProfile,
    /// Whether record issues force the failure class. The seq route leaves the exit to the program (the `--seq` route's
    /// parse errors are never fatal).
    pub(crate) force_issue_exit: bool,
    /// `--csv-delimiter BYTE`: `Some` selects the TSV dialect and friends for CSV input; `None` is RFC 4180's comma.
    pub(crate) csv_delimiter: Option<u8>,
    /// The CSV alphabet freeze : the explicit rfc4180 dialects enforce TEXTDATA; the utf8 family admits UTF-8.
    pub(crate) csv_textdata: bool,
    /// Which input model (`-n`, `-s`, or one run per record) the program takes over this record stream.
    pub(crate) model: RecordRunModel,
    pub(crate) input: &'request [u8],
    pub(crate) output: crate::args::CliOutputSelection,
    pub(crate) catalog: jqf_sdk::CodecCatalog<'request, 'request>,
    pub(crate) parallel: crate::args::CliParallelSelection,
    pub(crate) diagnostics: bool,
    /// `-e` reads the LAST OUTPUT VALUE, which a morsel relay never delivers, so an exit-status request plans serial.
    pub(crate) exit_status: bool,
    /// Whether this is the source-preserving EDIT lane.
    pub(crate) edit: bool,
    /// Colour engagement: the colour pass renders per item, and the relay publishes a byte stream without item
    /// boundaries, so a colour-engaged request plans serial.
    pub(crate) colour: bool,
    /// `--unbuffered` flushes per ITEM, which the morsel relay cannot do (its workers publish per MORSEL), so an
    /// unbuffered request plans serial.
    pub(crate) unbuffered: bool,
    /// The split destination (`--split-exp`): one file per published item, which a morsel relay never delivers, so a
    /// split request plans serial.
    pub(crate) split: bool,
    /// `--arg`/`--argjson`/`--args`/`-L`: plans serial.
    pub(crate) host_bindings: bool,
    /// The frame-transition ceiling (`--max-iterations`), carried from the request policy into the drive.
    pub(crate) max_iterations: Option<u64>,
    /// Diagnostics label: the first named file, or `<stdin>`.
    pub(crate) source_name: &'request str,
    /// Per-file ranges of a multi-file concatenation. `None` for stdin.
    pub(crate) files: Option<&'request [jqf_source::SourceFileRange<'request>]>,
}

/// Resolves the selected output format and dialect into the drive's target.
///
/// Only the four record-capable outputs map here; every other target is rejected by the caller's capability check
/// before this is reached. The fallthrough is still a usage error rather than a panic: a future format joining
/// `CliFormat` without the record capability check following it must fail the request, never abort the process.
fn output_target(output: crate::args::CliOutputSelection) -> Result<OutputTarget, CliFailure> {
    match output.format {
        CliFormat::Ndjson => Ok(OutputTarget::Ndjson),
        CliFormat::JsonSeq => Ok(OutputTarget::JsonSeq),
        CliFormat::Csv => Ok(OutputTarget::Csv {
            header: output.dialect == crate::args::CliOutputDialect::CsvJqfRfc4180Header
                || output.dialect == crate::args::CliOutputDialect::CsvJqfUtf8Header,
            utf8: matches!(
                output.dialect,
                crate::args::CliOutputDialect::CsvJqfUtf8 | crate::args::CliOutputDialect::CsvJqfUtf8Header
            ),
        }),
        CliFormat::Tsv => Ok(OutputTarget::Tsv {
            header: output.dialect == crate::args::CliOutputDialect::TsvJqfLfHeader,
        }),
        CliFormat::Json => Ok(OutputTarget::Json),
        CliFormat::Render => Ok(OutputTarget::Render {
            dialect: output.dialect.id(),
            options: output.render.encode_options(),
        }),
        CliFormat::Yaml => Ok(OutputTarget::Yaml {
            dialect: output.dialect.id(),
        }),
        other => Err(CliFailure::Message {
            class: ExitClass::Usage,
            message: format!(
                "--output-format {} is not supported on a record input; \
                 record routes encode json, ndjson, json-seq, csv, tsv, render, or yaml",
                other.id()
            ),
        }),
    }
}

/// Builds one record drive over `input`. Shared by the whole-input record route and the `--follow` drive, so the two
/// can never disagree on a spec field (the follow route is the record route fed incrementally).
#[allow(
    clippy::too_many_arguments,
    reason = "record_drive_spec mirrors the RecordDriveSpec field set one-to-one, and the two               record routes that build it must pass every field"
)]
pub(crate) fn record_drive_spec<'request>(
    kind: RecordInputKind,
    profile: NdjsonProfile,
    json_seq_profile: jqf_codec_json::seq::JsonSeqProfile,
    model: RecordRunModel,
    input: &'request [u8],
    output: crate::args::CliOutputSelection,
    csv_delimiter: Option<u8>,
    csv_textdata: bool,
    edit: bool,
    source_name: &'request str,
    files: Option<&'request [jqf_source::SourceFileRange<'request>]>,
    max_iterations: Option<u64>,
) -> Result<RecordDriveSpec<'request>, CliFailure> {
    Ok(RecordDriveSpec {
        input,
        source_name,
        files,
        kind,
        profile,
        json_seq_profile,
        csv_delimiter,
        // The RFC 4180 alphabet freeze rides the explicit rfc4180 dialects ; the utf8 family and TSV decode
        // permissively.
        csv_textdata,
        // No per-record ceiling: the record stream's own ceiling defaults to the request's input ceiling, and that
        // ceiling is now unlimited — the retained input physically bounds a record. The worker-envelope law is
        // untouched: envelopes are sized from the concurrency window, never from this option.
        max_record_bytes: u64::MAX,
        catalog: crate::routes::record_catalog(),
        output: RecordOutputSpec {
            target: output_target(output)?,
            terminator: output.terminator,
            json: jqf_codec_json::JsonEncodeOptions {
                indent: output.indent,
                raw_strings: output.raw_strings,
                sort_keys: output.sort_keys,
                ascii_output: output.ascii_output,
                raw_output_nul: output.raw_output_nul,
            },
            no_newline: output.no_newline,
        },
        model,
        edit,
        cooperative_credits: COOPERATIVE_CREDITS,
        max_iterations,
    })
}

/// Plans one record request under the switch/dial pair and the program class.
///
/// The switch decides WHETHER any parallelism engages; the planner then decides whether this particular request
/// benefits, and how wide. Every ineligible arm ends on the same serial drive, so falling through is invisible except
/// under `--diagnostics`.
#[expect(
    clippy::too_many_arguments,
    clippy::fn_params_excessive_bools,
    reason = "the record planner's inventory: parallel, exit, colour, and the two dials stay explicit call-by-call"
)]
pub(crate) fn plan_record_request(
    parallel: crate::args::CliParallelSelection,
    exit_status: bool,
    colour: bool,
    edit: bool,
    unbuffered: bool,
    input_bytes: u64,
    spec: RecordDriveSpec<'_>,
    compiled: &jqf_engine::CompiledProgram,
    non_lenient: bool,
    non_default_strictness: bool,
    split: bool,
    host_bindings: bool,
) -> ParallelPlan {
    let kind = spec.kind;
    let profile = spec.profile;
    if parallel.enabled {
        if split {
            // The split destination opens one file per published item; a morsel worker publishes bytes and nothing
            // else, so the names and the per-item sink are serial-only.
            ParallelPlan::serial(parallel.workers, PlanDecision::SplitExp, input_bytes)
        } else if edit {
            // The source-preserving edit lane splices each record's OWN retained bytes and verifies by re-decode: a
            // morsel worker cannot splice, so the request plans serial.
            ParallelPlan::serial(parallel.workers, PlanDecision::Edit, input_bytes)
        } else if spec.model.is_single_run() {
            // `-n`/`-s` run the program ONCE over the whole stream: there is no per-record morsel to relay, and a
            // worker's array would be a fragment of the caller's.
            ParallelPlan::serial(parallel.workers, PlanDecision::SingleRunModel, input_bytes)
        } else if spec.output.target.is_stateful() {
            // The headered CSV output derives its one header row from the FIRST item's keys; per morsel that becomes
            // one header per morsel, which is not serial's bytes.
            ParallelPlan::serial(parallel.workers, PlanDecision::StatefulOutput, input_bytes)
        } else if exit_status {
            // The relay publishes bytes, not values; an exit-status request needs the last output VALUE and plans
            // serial, publishing identical bytes.
            ParallelPlan::serial(parallel.workers, PlanDecision::ExitStatus, input_bytes)
        } else if colour {
            // Colour renders per ITEM ; the relay publishes a byte stream without item boundaries, so a worker's bytes
            // could never reach the colour pass as items. Serial publishes per item with its reports, exactly as the
            // exit-status decline does.
            ParallelPlan::serial(parallel.workers, PlanDecision::Colour, input_bytes)
        } else if unbuffered {
            // `--unbuffered` flushes per ITEM; the relay publishes per MORSEL and its workers' finish_item is a no-op,
            // so a per-item-flush request would stream at morsel cadence under a flag that promises item cadence — the
            // value lane's planner declines the same shape (PlanDecision:Unbuffered there).
            ParallelPlan::serial(parallel.workers, PlanDecision::Unbuffered, input_bytes)
        } else if non_lenient {
            // The mismatch dial beyond lenient: a worker's report aggregates in its own context, so warn's report would
            // be lost and strict's raises would need the rerun. Serial's report and raises are the request's own.
            ParallelPlan::serial(parallel.workers, PlanDecision::MismatchPolicy, input_bytes)
        } else if non_default_strictness {
            //: a worker can neither surface a warning nor carry
            // promotion, so `warn`/`strict` plan serial (the mismatch decline's shape).
            ParallelPlan::serial(parallel.workers, PlanDecision::Strictness, input_bytes)
        } else if host_bindings {
            // `--arg`/`--argjson`/`--args`/`-L`: the CLI's binding table and module search path are request-local. A
            // morsel worker does not carry them, so the request plans serial. Named distinctly from the program class
            // so a diagnostics reader can tell the two declines apart.
            ParallelPlan::serial(parallel.workers, PlanDecision::HostBindings, input_bytes)
        } else {
            // Eligibility is the WIDENED per-record class : any program whose every overload is Effects:Pure is
            // per-record independent, so a worker's bytes for its morsel are serial's bytes. The input family, stderr
            // writers, halt, and the nondeterministic builtins are Impure and decline; the adjacent-value lane keeps
            // its own narrower static-path gate (its law says deliberately so).
            let ineligible = (!compiled.is_morsel_eligible())
                .then_some(PlanDecision::ProgramIneligible)
                .or_else(|| {
                    // Only a STRICT framing profile makes an unclean morsel unambiguous: its framing faults are
                    // terminal, so a morsel either publishes exactly what serial would or reports that it did not. A
                    // RECOVERING framer reports ordered issues carrying absolute offsets — diagnostics a worker holding
                    // one byte range cannot render — so recovering NDJSON and the `--seq` flag's recovering json-seq
                    // decline here with the same printable reason. CSV/TSV have a single strict framing; the headered
                    // CSV dialect is additionally stateful across records — row zero names every later row's members —
                    // so a worker handed a middle byte range would read its own first row as a header. It declines on
                    // the same arm.
                    let framed = match kind {
                        RecordInputKind::Ndjson => profile == NdjsonProfile::Strict,
                        RecordInputKind::JsonSeq => spec.json_seq_profile == JsonSeqProfile::Strict,
                        RecordInputKind::Csv { .. } => true,
                    };
                    (!framed || kind.is_stream_stateful()).then_some(PlanDecision::InputIneligible)
                });
            plan_request(parallel.workers, input_bytes, ineligible)
        }
    } else {
        ParallelPlan::serial(parallel.workers, PlanDecision::SwitchedOff, input_bytes)
    }
}

/// Runs the explicit-NDJSON record route, serially or on morsel workers.
///
/// Explicit NDJSON takes the RECORD route and nothing else: the framer owns the physical stream, and every payload is
/// decoded as a COMPLETE strict JSON document over the record's own byte range. None of the demand-projection rungs
/// apply — each of them validates the WHOLE input as one document, which an NDJSON stream deliberately is not.
///
/// The route reports [`RouteOutcome:Served`] on completion: its drive runs its own epilogue (flush, diagnostics,
/// precision line), so the driver's shared epilogue must not run again.
pub(crate) fn run(ctx: &mut RouteContext<'_, '_>) -> Result<RouteOutcome, CliFailure> {
    let request = RecordRequest {
        kind: ctx
            .input_selection
            .dialect
            .record_kind()
            .expect("the record route is only resolved for a record dialect"),
        profile: ctx
            .input_selection
            .dialect
            .ndjson_profile()
            .unwrap_or(NdjsonProfile::Strict),
        json_seq_profile: ctx.json_seq_profile,
        force_issue_exit: ctx.force_issue_exit,
        csv_delimiter: ctx.input_selection.csv_delimiter,
        csv_textdata: ctx.input_selection.dialect.csv_textdata(),
        model: ctx.record_model,
        input: ctx.input,
        output: ctx.output_selection,
        catalog: ctx.catalog,
        parallel: ctx.parallel_selection,
        diagnostics: ctx.diagnostics,
        exit_status: ctx.exit_status,
        edit: ctx.edit,
        colour: ctx.colour,
        unbuffered: ctx.sink.unbuffered,
        split: ctx.policy.split.is_some(),
        host_bindings: ctx.host_bindings,
        source_name: ctx.source.label(),
        files: ctx.files,
        max_iterations: ctx.policy.max_iterations,
    };
    let outcome = run_record_request(
        request,
        ctx.compiled,
        ctx.policy.split,
        ctx.resources,
        ctx.sink,
        ctx.diagnostics_buffer,
    );
    // The RECORD route is the explicit-format lane and nothing else: the framer owns the physical stream, so a
    // completed record request was served by it, whatever the parallel plan decided.
    if outcome.is_ok() {
        record_route(ctx.diagnostics_buffer, "record");
    }
    outcome.map(|()| RouteOutcome::Served)
}

fn run_record_request<W: IoWrite>(
    request: RecordRequest<'_>,
    compiled: &jqf_engine::CompiledProgram,
    split: Option<&jqf_engine::CompiledProgram>,
    resources: &mut jqf_resource::ResourceContext<'_>,
    sink: &mut crate::output::StdoutSink<W>,
    diagnostics_buffer: Option<&jqf_sdk::Diagnostics>,
) -> Result<(), CliFailure> {
    // The record route owns a per-record byte stream, so a document-shaped output target must be either served or
    // rejected — never silently encoded as JSON. The ONE check is `require_record_capable_output`, shared by the
    // record, streaming-stdin, and `--follow` routes.
    require_record_capable_output(request.output, request.catalog)?;
    let drive = record_drive_spec(
        request.kind,
        request.profile,
        request.json_seq_profile,
        request.model,
        request.input,
        request.output,
        request.csv_delimiter,
        request.csv_textdata,
        request.edit,
        request.source_name,
        request.files,
        request.max_iterations,
    )?;
    let input_bytes = request.input.len() as u64;
    let plan = plan_record_request(
        request.parallel,
        request.exit_status,
        request.colour,
        request.edit,
        request.unbuffered,
        input_bytes,
        drive,
        compiled,
        resources.mismatch_policy() != jqf_resource::policy::MismatchPolicy::Lenient,
        resources.strictness() != jqf_resource::policy::StrictnessPolicy::Error,
        request.split,
        request.host_bindings,
    );
    if request.diagnostics {
        // The plan is printed BEFORE the drive so a failing request still says what it planned to do. Which BINARY
        // planned it — allocator and build path, the width policy's pinned constants having been observed against one
        // of each — is `provenance:BuildProvenance`'s line, which `run` has already printed for every route rather than
        // for this one.
        crate::eprint_line_buffered(&format!("jqf: {plan}"));
        crate::flush_stderr();
    }
    let report = execute_record_request(drive, plan, compiled, resources, sink, split)
        .map_err(|error| record_and_render_record_failure(request.diagnostics, diagnostics_buffer, error))?;
    resources.note_strictness_warnings(report.issues().saturating_add(report.codec_value_errors()));
    if request.diagnostics {
        crate::eprint_line_buffered(&format!("jqf: {report}"));
        // The cost snapshot is recorded here too — this route and the sharded value lane run their own epilogues; the
        // shared epilogue owns the cost line only for the ladder's Completed arms. The RSS gate's physical-report lanes
        // read the accounted peak out of it (the ledger-vs-RSS ratio receipt).
        if let Some(buffer) = diagnostics_buffer {
            buffer.record_cost(&crate::reported_cost_snapshot(resources));
            crate::print_diagnostic_records(buffer);
        }
        crate::eprint_line_buffered(&format!(
            "jqf: precision_boundary_events={}",
            resources.precision_boundary_events()
        ));
    }
    sink.output
        .flush()
        .map_err(|error| format!("cannot flush stdout: {error}"))?;
    // The recovering-dialect exit law: ONE error-severity issue anywhere in the stream forces the runtime exit class
    // even when every later record succeeded. Advisories alone leave the class to the program's own last-record result,
    // which the `Ok` above already carries. The `--seq` route SKIPS this check: the `--seq` route's parse errors are
    // never fatal (every parse-error case exits 0), so its exit is the program's own last-record result alone.
    if request.force_issue_exit && report.error_issues() > 0 {
        return Err(CliFailure::Reported);
    }
    Ok(())
}

/// The ONE record-capable OUTPUT check: a document-shaped target must be served or rejected, never silently encoded as
/// JSON. The set is read from the codec's own `route_capabilities` declaration (039 item 1a/1b): a target whose
/// registration does not declare `RouteCapability:Record` is a lookup miss, raised before a byte is written, so a
/// `--input-format ndjson --output-format toml` request can never publish a format other than the one requested. Shared
/// by the record, streaming-stdin, and `--follow` routes, so a format joining the record route (json-seq, csv, ndjson)
/// can never drift between hand-written copies — the follow route is the record route fed incrementally and serves
/// exactly the record route's output set. The JSON registration declares `Record` (the record route's default output
/// target), as do ndjson, json-seq, and csv.
pub(crate) fn require_record_capable_output(
    output: crate::args::CliOutputSelection,
    catalog: jqf_sdk::CodecCatalog<'_, '_>,
) -> Result<(), CliFailure> {
    let format =
        FormatId::try_new(output.format.id()).map_err(|error| format!("invalid built-in format identity: {error}"))?;
    let dialect = DialectId::try_new(output.dialect.id())
        .map_err(|error| format!("invalid built-in dialect identity: {error}"))?;
    let record_capable = catalog
        .route_capabilities(&format, &dialect)
        .map_err(|error| CliFailure::Message {
            class: ExitClass::Usage,
            message: format!(
                "--output-format {} is not supported on a record input: \
                     no registration for the target ({error:?})",
                output.format.id()
            ),
        })?
        .contains(&jqf_codec_core::RouteCapability::Record);
    if !record_capable {
        return Err(CliFailure::Message {
            class: ExitClass::Usage,
            message: format!(
                "--output-format {} is not supported on a record input; \
                 record routes encode json, ndjson, json-seq, csv, tsv, render, or yaml",
                output.format.id()
            ),
        });
    }
    Ok(())
}

/// Refuses the stream-STATEFUL YAML output profiles on an incremental route.
///
/// Shared by the `--follow` and streaming-stdin routes, like [`refuse_headered_csv`]. The block profile writes `---`
/// BETWEEN documents from state in the encoder factory; a cycle drive re-opens that factory on every refill, so the
/// first item after a cycle boundary would lose its separator and splice two documents into one. The canonical profiles
/// are self-contained per item and serve fine.
pub(crate) fn refuse_stream_stateful_yaml(
    route: &str,
    output: &crate::args::CliOutputSelection,
) -> Result<(), CliFailure> {
    if output.format == crate::args::CliFormat::Yaml
        && matches!(
            output.dialect,
            crate::args::CliOutputDialect::YamlBlock | crate::args::CliOutputDialect::YamlJqf
        )
    {
        return Err(CliFailure::Message {
            class: ExitClass::Usage,
            message: format!(
                "--output-format {} is not supported on {route}: the block profile's \
                 between-documents --- separator is stream state a cycle restart cannot \
                 carry; use --output-format yaml:stream-canonical (or read whole)",
                output.dialect.id()
            ),
        });
    }
    Ok(())
}

/// Refuses the headered delimited dialect on an incremental route.
///
/// Shared by the `--follow` and streaming-stdin routes: the header row is a whole-stream fact, and the cycle drive
/// re-opens the framer and the payload provider on every refill, so each cycle would consume its own first record as a
/// header and key the rest by that record's values. A seekable stdin redirect reads whole and serves it.
pub(crate) fn refuse_headered_csv(route: &str, kind: &str, dialect: &str) -> CliFailure {
    CliFailure::Message {
        class: ExitClass::Usage,
        message: format!(
            "the headered {kind} dialect ({dialect}) is not served by {route}: \
             the header row is a whole-stream fact the incremental drive cannot carry \
             across refills; redirect stdin from a regular file (which reads whole) or \
             use the array dialect"
        ),
    }
}

/// Records a record drive's TERMINAL failure into the diagnostic stream (when one exists), then renders it — the record
/// lanes' mirror of [`crate:record_and_render_failure`]. The setup arm synthesizes a fresh codec failure record and the
/// pipeline arm records its own; the sink arm stays rendered-text-only (its io error is not Clone, and a stdout failure
/// has no codec context the stream would carry anyway).
pub(crate) fn record_and_render_record_failure(
    print_records: bool,
    diagnostics: Option<&jqf_sdk::Diagnostics>,
    error: RecordDriveError<std::io::Error>,
) -> CliFailure {
    if let Some(diagnostics) = diagnostics {
        match &error {
            RecordDriveError::Setup { error, .. } => {
                diagnostics.record_failure(&jqf_sdk::PipelineFailure::<std::io::Error>::Codec(error.clone()));
            }
            RecordDriveError::Pipeline(failure) => {
                if let Some(pipeline) = failure.pipeline_failure() {
                    diagnostics.record_failure(pipeline);
                }
            }
            // The host sink's io error is not Clone, so it cannot ride a synthesized record; the rendered text names
            // it, and a stdout failure has no codec context the stream would carry anyway.
            RecordDriveError::Sink(_) | RecordDriveError::Resource(_) | RecordDriveError::Control(_) => {}
        }
        if print_records {
            crate::print_diagnostic_records(diagnostics);
        }
    }
    render_record_drive_failure(error)
}

/// Renders a record drive's failure with the message its boundary owns.
pub(crate) fn render_record_drive_failure(error: RecordDriveError<std::io::Error>) -> CliFailure {
    match error {
        // The physical governor's refusal can land at the drive's SETUP (the plan / record-stream-open checkpoints) as
        // well as mid-drive; the follow route's per-cycle refusal law (D3) recognizes the CODEC shape, so a setup-phase
        // refusal renders as one — a Message would escape the per-cycle arm and kill the tail (docker battery catch,
        // 2026-08-15: the linux-amd64 run raced the refusal into setup). On the whole-input record route the same shape
        // is terminal either way.
        RecordDriveError::Setup { error, .. }
            if matches!(
                error.kind(),
                jqf_codec_core::CodecFailureKind::Control(jqf_resource::ControlError::MemoryExceeded)
            ) =>
        {
            crate::rss::refusal_failure()
        }
        RecordDriveError::Setup {
            step: "requirement",
            error,
        } => requirement_failure(&error),
        RecordDriveError::Setup {
            step: "record-ceiling",
            error,
        } => CliFailure::from(format!("invalid NDJSON record ceiling: {}", error.kind())),
        RecordDriveError::Setup { error, .. } => {
            CliFailure::from(format!("cannot open NDJSON record stream: {}", error.kind()))
        }
        RecordDriveError::Pipeline(failure) => render_failure(&failure),
        RecordDriveError::Sink(failure) => CliFailure::from(format!("stdout failed: {failure}")),
        RecordDriveError::Resource(error) => CliFailure::Codec {
            kind: jqf_codec_core::CodecFailureKind::Resource(error),
            diagnostic: None,
        },
        RecordDriveError::Control(jqf_resource::ControlError::MemoryExceeded) => crate::rss::refusal_failure(),
        RecordDriveError::Control(error) => CliFailure::Codec {
            kind: jqf_codec_core::CodecFailureKind::Control(error),
            diagnostic: None,
        },
    }
}

/// One incremental drive's outcome over a completed byte range.
#[derive(Clone, Copy)]
pub(crate) enum CycleOutcome {
    /// The drive completed; its last record succeeded.
    Clean,
    /// The drive completed but its LAST record failed with a per-value error (already streamed to stderr). The stream
    /// continues; this sets the whole run's exit class unless a later cycle clears it — the same last-record law the
    /// whole-input record route keeps within one drive.
    LastValueFailed,
}

/// The facade wrapper that re-bases an incremental drive's diagnostics to the absolute stream position.
///
/// An incremental drive is a self-contained record request over the cycle's completed range, so its offsets, line
/// numbers, and record ordinals are relative to that range. The offset is the load-bearing position for a live stream
/// (it is what a user greps the log for), the line number is what the `at <stdin>:N` frame means, and the ordinal is
/// `base + relative` so a pipe cut anywhere prints the same `record N` as a seekable whole-read. Shared by the
/// `--follow` route and the streaming-stdin route: both live routes re-base, they never re-render.
pub(crate) struct CycleTranslateSink<'a, W: IoWrite> {
    inner: &'a mut StdoutSink<W>,
    /// Absolute stream offset of the cycle's first byte.
    base_offset: u64,
    /// Number of newlines before the cycle's first byte (its absolute line base).
    base_line: u64,
    /// Physical records (values and issues) before the cycle's first byte.
    base_record: u64,
}

impl<W: IoWrite> ItemSink for CycleTranslateSink<'_, W> {
    type Error = io::Error;

    fn begin_item(&mut self, index: u64) -> Result<(), Self::Error> {
        self.inner.begin_item(index)
    }

    fn begin_item_named(&mut self, index: u64, name: &str) -> Result<(), Self::Error> {
        // The split destination travels the record routes too: the per-item name must reach the underlying sink's
        // destination.
        self.inner.begin_item_named(index, name)
    }

    fn write(&mut self, bytes: &[u8]) -> Result<usize, Self::Error> {
        self.inner.write(bytes)
    }

    fn finish_item(&mut self, index: u64, report: EncodedItemReport) -> Result<(), Self::Error> {
        self.inner.finish_item(index, report)
    }

    fn report_record_issue(&mut self, issue: RecordIssueReport<'_>) -> Result<(), Self::Error> {
        // The shared rendering law with the offset re-based to the absolute stream position and the framing codec's own
        // text; the incremental routes never re-render the issue shape.
        crate::output::render_record_issue(&issue, self.base_offset, self.base_record, self.inner.issue_text);
        Ok(())
    }

    fn report_value_error(&mut self, error: SequenceValueError) -> Result<(), Self::Error> {
        // Same law as the offset: the cycle's line count starts at one, so the facade adds the newlines the stream had
        // already consumed before the cycle began.
        eprint_value_error(
            error.input_line().saturating_add(self.base_line),
            error.frame_note(),
            error.message(),
        );
        Ok(())
    }
}

/// Counts the `\n` bytes in `bytes`: the per-cycle newline budget that turns a cycle's relative line numbers into
/// absolute ones.
pub(crate) fn count_newlines(bytes: &[u8]) -> u64 {
    memchr::memchr_iter(b'\n', bytes).count() as u64
}

/// Framer ordinals (values AND issues) in a completed cycle's bytes.
///
/// The framer assigns one dense ordinal per physical unit, so this is the increment that makes `base_record + relative`
/// cycle-invariant: record N must name the same unit a seekable whole-read of the same prefix names. Each arm therefore
/// walks the framing boundaries EXACTLY as that kind's framer does, never a raw byte count that merely correlates with
/// it:
///
/// - NDJSON ends every unit at a line feed; a recovering blank line or bare
///   CR is an issue whose unit still terminates at the next line feed — one ordinal per `\n`.
/// - json-seq coalesces a maximal RS run into the `1*RS` prefix of ONE
///   following text: one run is one ordinal, so counting raw RS bytes would drift after every coalesced pair. A run with
///   no following byte opens a unit THIS SPAN has not framed (a cycle boundary inside a run), so it counts nothing here —
///   exactly what the drive consumed and what the next cycle will charge once.
/// - TSV ends a unit at LF or CRLF; a lone CR is a terminal framing fault
///   for the framer, never a terminator (`tsv_complete_span` carries the same law), so counting lone CRs would frame
///   records the drive refuses.
/// - CSV walks the quote-state record ends, as the framer's own scanner does.
pub(crate) fn physical_record_count(kind: RecordInputKind, bytes: &[u8]) -> u64 {
    match kind {
        RecordInputKind::Ndjson => count_newlines(bytes),
        RecordInputKind::JsonSeq => json_seq_physical_records(bytes),
        RecordInputKind::Csv { tsv: true, .. } => tsv_physical_records(bytes),
        RecordInputKind::Csv { tsv: false, .. } => csv_physical_records(bytes),
    }
}

/// One ordinal per maximal RS run followed by at least one byte: a coalesced run is ONE unit's `1*RS` prefix, and a
/// trailing run belongs to a unit the NEXT cycle frames.
fn json_seq_physical_records(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .enumerate()
        .filter(|(index, byte)| **byte == 0x1E && bytes.get(index + 1).is_some_and(|&next| next != 0x1E))
        .count() as u64
}

/// Physical TSV records: every LF- or CRLF-terminated unit. The TSV grammar has no quote state, but a lone CR is NOT a
/// terminator (the framer faults it, exactly as NDJSON's framer does), so counting newlines alone would be right only
/// by coincidence with the cut walk; this names the law instead.
fn tsv_physical_records(bytes: &[u8]) -> u64 {
    let mut count = 0u64;
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'\n' => {
                count = count.saturating_add(1);
                index += 1;
            }
            b'\r' if bytes.get(index + 1) == Some(&b'\n') => {
                count = count.saturating_add(1);
                index += 2;
            }
            _ => index += 1,
        }
    }
    count
}

fn csv_physical_records(bytes: &[u8]) -> u64 {
    let mut cursor = 0usize;
    let mut count = 0u64;
    while cursor < bytes.len() {
        let mut index = cursor;
        let mut in_quotes = false;
        // The framer's own quote law (jqf-codec/delimited/src/boundary.rs): a quote OPENS quoted state only at a field
        // start (record start, or the byte after a delimiter outside quotes); inside quotes a quote closes unless
        // doubled. A lone quote mid-unquoted-field toggling here would swallow every later terminator and run the
        // ordinal base LOW against the whole-input drive.
        let mut at_field_start = true;
        let mut record_end: Option<usize> = None;
        while index < bytes.len() {
            match bytes[index] {
                b'"' if in_quotes => {
                    if bytes.get(index + 1) == Some(&b'"') {
                        index += 2;
                    } else {
                        in_quotes = false;
                        at_field_start = false;
                        index += 1;
                    }
                }
                b'"' => {
                    if at_field_start {
                        in_quotes = true;
                        at_field_start = false;
                    }
                    index += 1;
                }
                b'\n' if !in_quotes => {
                    record_end = Some(index + 1);
                    break;
                }
                b'\r' if !in_quotes => match bytes.get(index + 1) {
                    Some(&b'\n') => {
                        record_end = Some(index + 2);
                        break;
                    }
                    // A bare CR is a framing fault for the framer, never a terminator; it cannot appear in a counted
                    // prefix (the fault hands the whole buffer to the drive), so stopping the walk here only keeps the
                    // counter honest.
                    _ => break,
                },
                _ => {
                    if !in_quotes {
                        at_field_start = bytes[index] == b',';
                    }
                    index += 1;
                }
            }
        }
        match record_end {
            Some(end) => {
                count = count.saturating_add(1);
                cursor = end;
            }
            None => break,
        }
    }
    count
}

/// Drives one incremental cycle's completed byte range through the record route.
///
/// This is the whole-input record route over the cycle's bytes: same spec builder, same planner, same drive, same
/// failure rendering. The parallel machinery engages exactly when the plan says the batch is big enough; a recovering
/// batch is input-ineligible (the plan's own law) and stays serial, which is what keeps its diagnostics ordered. The
/// drive flushes the sink at every cycle boundary: a live run must publish a record before the next one is read, on a
/// run that may never end.
///
/// Shared by the `--follow` route and the streaming-stdin route, so the two can never disagree on a cycle law. A
/// per-value error on the cycle's LAST record returns [`CycleOutcome:LastValueFailed`] instead of failing the run: the
/// whole-input route keeps streaming past a per-record error within one drive, and both live routes keep the same law
/// across their cycle boundaries.
#[expect(
    clippy::too_many_arguments,
    reason = "the cycle drive takes offset, line, and record bases plus the issue tally; bundling them would hide the rebase surface"
)]
pub(crate) fn drive_range(
    ctx: &mut RouteContext<'_, '_>,
    batch: &[u8],
    kind: RecordInputKind,
    profile: NdjsonProfile,
    base_offset: u64,
    base_line: u64,
    base_record: u64,
    error_issues: &mut u64,
) -> Result<CycleOutcome, CliFailure> {
    let spec = record_drive_spec(
        kind,
        profile,
        // The json-seq framing profile of the REQUEST (`--seq`'s flag-scoped recovering route, or strict for an
        // explicit json-seq selection); meaningless for kinds that do not frame on RS.
        ctx.json_seq_profile,
        // Both incremental routes refuse `-n`/`-s` up front (they read the input to completion, which a live stream
        // never reaches), so a cycle is always the per-record model.
        RecordRunModel::PerRecord,
        batch,
        ctx.output_selection,
        ctx.input_selection.csv_delimiter,
        ctx.input_selection.dialect.csv_textdata(),
        // The follow cycle is never the edit lane (the follow route refuses `--edit` before a byte is read).
        false,
        ctx.source.label(),
        ctx.files,
        ctx.policy.max_iterations,
    )?;
    let input_bytes = batch.len() as u64;
    let plan = plan_record_request(
        ctx.parallel_selection,
        ctx.exit_status,
        ctx.colour,
        false,
        ctx.sink.unbuffered,
        input_bytes,
        spec,
        ctx.compiled,
        ctx.resources.mismatch_policy() != jqf_resource::policy::MismatchPolicy::Lenient,
        ctx.resources.strictness() != jqf_resource::policy::StrictnessPolicy::Error,
        ctx.policy.split.is_some(),
        ctx.host_bindings,
    );
    if ctx.diagnostics {
        // The plan is printed BEFORE the drive so a failing request still says what it planned to do.
        eprint_line_buffered(&format!("jqf: {plan}"));
        flush_stderr();
    }
    let mut sink = CycleTranslateSink {
        inner: ctx.sink,
        base_offset,
        base_line,
        base_record,
    };
    let outcome = match execute_record_request(spec, plan, ctx.compiled, ctx.resources, &mut sink, ctx.policy.split) {
        Ok(report) => {
            *error_issues = error_issues.saturating_add(report.error_issues());
            //: record advisories and per-record codec failures are
            // promotable warnings under `--strictness strict`.
            ctx.resources
                .note_strictness_warnings(report.issues().saturating_add(report.codec_value_errors()));
            if ctx.diagnostics {
                eprint_line_buffered(&format!("jqf: {report}"));
                eprint_line_buffered(&format!(
                    "jqf: precision_boundary_events={}",
                    ctx.resources.precision_boundary_events()
                ));
            }
            CycleOutcome::Clean
        }
        Err(RecordDriveError::Pipeline(jqf_sdk::Failure::Pipeline(ref pipeline)))
            if jqf_runtime::feed::is_per_value_failure(pipeline.failure()) =>
        {
            // The failure was already reported to stderr as the sequence continued past it; the run must go on, and its
            // exit class waits for the true last record.
            CycleOutcome::LastValueFailed
        }
        Err(error) => {
            return Err(record_and_render_record_failure(
                ctx.diagnostics,
                ctx.diagnostics_buffer,
                error,
            ));
        }
    };
    // Records stream OUT as they arrive: a live run must publish a record before the next one is read, so the sink is
    // flushed at every cycle boundary rather than at the end of a run that may never come.
    ctx.sink
        .output
        .flush()
        .map_err(|error| format!("cannot flush stdout: {error}"))?;
    // And stderr is flushed so a fault in the record just read is visible before the next read — the whole point of a
    // live stream.
    flush_stderr();
    Ok(outcome)
}

#[cfg(test)]
mod record_count_tests {
    use super::{
        count_newlines, csv_physical_records, json_seq_physical_records, physical_record_count, tsv_physical_records,
    };
    use jqf_runtime::records::RecordInputKind;

    #[test]
    fn csv_counts_use_the_framer_quote_law() {
        // A lone quote mid-unquoted-field is NON-STRUCTURAL (the framer never toggles on it): counting it as an open
        // quote would swallow every later terminator and run the cycle ordinal base low against the whole-input drive.
        let lone = &b"a,b\"x\nc,d\n"[..];
        assert_eq!(csv_physical_records(lone), 2);
        assert_eq!(
            physical_record_count(
                RecordInputKind::Csv {
                    header: false,
                    tsv: false
                },
                lone
            ),
            2
        );
        // A line feed inside a quoted field ends nothing; CRLF ends one.
        assert_eq!(csv_physical_records(&b"a,\"b\nc\"\nd\n"[..]), 2);
        assert_eq!(csv_physical_records(&b"a,b\r\nc,d\r\n"[..]), 2);
    }

    #[test]
    fn csv_counts_quote_state_record_ends() {
        // The embedded newline inside the quoted field is payload, not a record end: two records, three line feeds.
        assert_eq!(csv_physical_records(b"a,b\nc,\"d\ne\"\n"), 2);
    }

    #[test]
    fn tsv_counts_lf_and_crlf_terminators() {
        assert_eq!(tsv_physical_records(&b"a\tb\r\nc\td\r\n"[..]), 2);
        assert_eq!(tsv_physical_records(&b"a\tb\nc\td\n"[..]), 2);
        assert_eq!(tsv_physical_records(&b"a\tb"[..]), 0);
        // The kind dispatch reaches the same walk.
        assert_eq!(
            physical_record_count(
                RecordInputKind::Csv {
                    header: false,
                    tsv: true
                },
                &b"a\tb\r\nc"[..]
            ),
            1
        );
        // A blank line is an issue that consumes its own ordinal.
        assert_eq!(
            physical_record_count(
                RecordInputKind::Csv {
                    header: false,
                    tsv: true
                },
                b"a\n\nb\n"
            ),
            3
        );
        // A lone CR is NOT a terminator: the framer faults it terminally (`tsv_complete_span` hands the fault over
        // whole), so counting it would frame records the drive refuses.
        assert_eq!(tsv_physical_records(b"a\tb\rc\td\n"), 1);
        // NDJSON keeps the same one-ordinal-per-line-feed law (blank lines and bare-CR issues terminate at the next
        // line feed too).
        assert_eq!(
            physical_record_count(RecordInputKind::Ndjson, b"a\n\nb\n"),
            count_newlines(b"a\n\nb\n")
        );
    }

    #[test]
    fn json_seq_counts_one_ordinal_per_coalesced_rs_run() {
        // A maximal RS run is the `1*RS` prefix of ONE following text: two adjacent RS bytes are one unit's prefix, not
        // an empty unit plus a unit. Counting raw RS bytes (the old counter) would drift +1 here.
        assert_eq!(
            physical_record_count(RecordInputKind::JsonSeq, b"\x1e{\"a\":1}\x1e\x1e{\"b\":2}"),
            2
        );
        // Plain units: one ordinal per RS-bounded text.
        assert_eq!(json_seq_physical_records(b"\x1e1\n\x1e2\n"), 2);
        // An RS-only span frames no unit yet: it is one unterminated prefix.
        assert_eq!(json_seq_physical_records(b"\x1e\x1e\x1e"), 0);
    }

    #[test]
    fn json_seq_ordinals_survive_a_cycle_split_inside_an_rs_run() {
        // The complete-span cut ends a cycle AT the last RS, so a run can be split across cycles (`...\x1e\x1e`
        // buffered as `...\x1e` + `\x1e...`). The trailing run must count nothing in the first cycle (the drive's
        // framer discards the zero-byte unit it would open) and the remainder counts once in the next — so the sum
        // always equals the seekable whole-read's ordinal count for the same bytes.
        let stream = b"\x1e1\x1e\x1e2";
        assert_eq!(
            physical_record_count(RecordInputKind::JsonSeq, stream),
            2,
            "whole-read ordinals: unit `1`, then the coalesced run + unit `2`"
        );
        let cut = stream.iter().rposition(|&byte| byte == 0x1E).unwrap();
        let cycle_one = physical_record_count(RecordInputKind::JsonSeq, &stream[..cut]);
        let cycle_two = physical_record_count(RecordInputKind::JsonSeq, &stream[cut..]);
        assert_eq!(cycle_one + cycle_two, 2, "cycles sum to the whole-read");
        assert_eq!(cycle_one, 1, "the trailing run opens no unit this cycle");
        // The old raw-RS-byte counter saw three RS bytes across the two cycles and drifted base_record +1 past this
        // boundary forever.
        #[allow(
            clippy::naive_bytecount,
            reason = "one-shot test assertion over a handful of bytes; bytecount would be a new dev-dep for nothing"
        )]
        let rs_bytes = stream.iter().filter(|&&byte| byte == 0x1E).count();
        assert_eq!(rs_bytes, 3);
    }
}
