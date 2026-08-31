//! The value-route ladder: the sharded adjacent-value lane, then the serial tail — one [`jqf_sdk:Request`] →
//! [`jqf_sdk:execute`].
//!
//! The sharded lane is a runtime-declining attempt: it publishes nothing on a decline and reports
//! [`RouteOutcome:Declined`] so the driver tries the serial tail. The tail never declines. The roundtrip / range-locate
//! / input-family / floor order lives inside `execute`.

use std::io::Write as IoWrite;

use jqf_runtime::values::{
    ParallelPlan, PlanDecision, ValueDriveError, ValueDriveSpec, ValueOutputSpec, execute_value_request,
    partition_request, plan_request,
};
use jqf_sdk::{Input, Outcome, Report};

use crate::args::CliFormat;
use crate::errors::{CliFailure, render_failure, requirement_failure};
use crate::output::StdoutSink;
use crate::plan::{COOPERATIVE_CREDITS, RouteContext, RouteOutcome};
use crate::{eprint_line_buffered, flush_stderr, print_diagnostic_records, record_and_render_failure, record_route};

/// One flagless adjacent-value invocation, as the facade sees it.
#[derive(Clone, Copy)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "ValueRequest carries one bool per CLI dial a route must read (diagnostics, exit-status, colour, split); bundling them would hide the flag surface"
)]
struct ValueRequest<'request> {
    input: &'request [u8],
    output: crate::args::CliOutputSelection,
    diagnostics: bool,
    /// `-e` reads the LAST OUTPUT VALUE, which a morsel relay never delivers, so an exit-status request plans serial.
    exit_status: bool,
    /// Colour engagement: the colour pass renders per item, and the relay publishes a byte stream without item
    /// boundaries, so a colour-engaged request plans serial.
    colour: bool,
    /// The split destination (`--split-exp`): one file per published item, which a morsel relay never delivers, so a
    /// split request plans serial.
    split: bool,
    /// `--arg`/`--argjson`/`--args`/`-L`: plans serial.
    host_bindings: bool,
    /// `--unbuffered` flushes per ITEM, which the morsel relay cannot do.
    unbuffered: bool,
}

/// The sharded adjacent-value lane, or declines to the routes below.
///
/// The whole lane — planning, boundary discovery, diagnostics, and the drive — sits behind ONE never-inlined call for a
/// observed reason. The record §9.7 finding is that a CLI function large enough to host a whole route is a measurement
/// hazard for every OTHER route in the same binary: an inline branch here cost `collect` +10.2 %, `fan-out` +9.5 %,
/// `sum fold` +7.0 % and the scoped lane +6.4 % with their own code untouched, and extracting it returned every one of
/// them to within ±1 %. Bisect the commit, do not tune the lane.
pub(crate) fn value_lane(ctx: &mut RouteContext<'_, '_>) -> Result<RouteOutcome, CliFailure> {
    let request = ValueRequest {
        input: ctx.input,
        output: ctx.output_selection,
        diagnostics: ctx.diagnostics,
        exit_status: ctx.exit_status,
        colour: ctx.colour,
        split: ctx.policy.split.is_some(),
        host_bindings: ctx.host_bindings,
        unbuffered: ctx.sink.unbuffered,
    };
    let Some(outcome) = run_value_lane(
        request,
        ctx.parallel_selection,
        ctx.compiled,
        ctx.resources,
        ctx.sink,
        ctx.diagnostics_buffer,
    ) else {
        return Ok(RouteOutcome::Declined);
    };
    // The sharded adjacent-value lane ran: every shard is the ordinary `execute_sequence` drive, so the lane's route
    // record names the LANE (`value-lane`) rather than the serial rung. The route record itself is written INSIDE
    // run_value_request's epilogue (before its in-lane print), so a parallel run carries exactly one ROUTE_SELECTED —
    // no duplicate lands in the buffer after the print.
    outcome.map(|()| RouteOutcome::Served)
}

/// Plans the flagless adjacent-value lane, or records why it declines.
///
/// The lane promises exactly the class the record campaign observed — a morsel static path — and only against the route
/// it observed it against, `execute_sequence`. A program any single-document rung serves is left to that rung: each of
/// those validates the WHOLE input as one value, which is a shape this lane has nothing to shard and no business
/// overriding.
#[expect(
    clippy::fn_params_excessive_bools,
    clippy::too_many_arguments,
    reason = "the value lane's planner inventory: parallel, exit, colour, output format, and the two dials stay explicit"
)]
fn plan_value_lane(
    compiled: &jqf_engine::CompiledProgram,
    parallel: crate::args::CliParallelSelection,
    input_bytes: u64,
    exit_status: bool,
    colour: bool,
    non_lenient: bool,
    non_default_strictness: bool,
    output_format: CliFormat,
    split: bool,
    host_bindings: bool,
    unbuffered: bool,
) -> ParallelPlan {
    if !parallel.enabled {
        return ParallelPlan::serial(parallel.workers, PlanDecision::SwitchedOff, input_bytes);
    }
    if split {
        // The split destination opens one file per published ITEM, and a morsel worker publishes a byte stream without
        // item boundaries — the worker's bytes could never reach the per-item sink (, D19's destination model). The
        // serial drive publishes per item with the names, exactly as the exit-status decline does.
        return ParallelPlan::serial(parallel.workers, PlanDecision::SplitExp, input_bytes);
    }
    if exit_status {
        // The relay publishes bytes, not values; an exit-status request needs the last output VALUE and plans serial,
        // publishing identical bytes.
        return ParallelPlan::serial(parallel.workers, PlanDecision::ExitStatus, input_bytes);
    }
    if colour {
        // Colour renders per ITEM ; the relay publishes a byte stream without item boundaries, so a worker's bytes
        // could never reach the colour pass as items. Serial publishes per item with its reports, exactly as the
        // exit-status decline does.
        return ParallelPlan::serial(parallel.workers, PlanDecision::Colour, input_bytes);
    }
    if unbuffered {
        // `--unbuffered` flushes per ITEM; the relay publishes per MORSEL and never reaches the CLI sink's finish_item
        // flush, so a per-item-flush request would stream at morsel cadence under a flag that promises item cadence —
        // the same relay-incompatible shape colour and split decline for.
        return ParallelPlan::serial(parallel.workers, PlanDecision::Unbuffered, input_bytes);
    }
    if non_lenient {
        // The mismatch dial beyond lenient: a worker's report aggregates in its own context (never the coordinator's),
        // so warn's report would be lost and strict's raises would need the rerun. The request plans serial, whose
        // report and raises are the request's own.
        return ParallelPlan::serial(parallel.workers, PlanDecision::MismatchPolicy, input_bytes);
    }
    if non_default_strictness {
        //: a worker can neither surface a warning nor carry
        // promotion, so `warn`/`strict` plan serial (the mismatch decline's shape); the serial drive's warnings and
        // exit class are the request's own.
        return ParallelPlan::serial(parallel.workers, PlanDecision::Strictness, input_bytes);
    }
    if !matches!(output_format, CliFormat::Json | CliFormat::Ndjson | CliFormat::JsonSeq) {
        // A non-JSON-family output format cannot travel this lane's relay: the relay concatenates JSON-family byte
        // streams, and a worker cannot encode a one-document-per-run format per value. The serial drive's encoder owns
        // the format.
        return ParallelPlan::serial(parallel.workers, PlanDecision::OutputFormat, input_bytes);
    }
    if host_bindings {
        // The bindings are HOST state, not a program property: named distinctly from the program-ineligible decline so
        // a diagnostics reader can tell the two apart.
        return ParallelPlan::serial(parallel.workers, PlanDecision::HostBindings, input_bytes);
    }
    // Eligibility is the WIDENED per-value class (round 2): any program whose every overload is Effects:Pure is
    // per-VALUE independent, so a shard's bytes for its range are serial's bytes — the same argument the record route's
    // widening observed. The single-document rung guards are NOT repeated here: `partition_request`'s boundary scan
    // declines every single-document input itself (a root close at end of input is never a cut, and the 128 KiB probe
    // declines larger documents), so a rung-eligible program over ONE document still takes its rung, and only
    // multi-value stdin — where the rungs decline by construction — reaches the lane. The guards would have kept
    // collect/fan-out shapes off this lane on multi-value stdin, which is exactly the gap they closed.
    let ineligible = (!compiled.is_morsel_eligible()).then_some(PlanDecision::ProgramIneligible);
    plan_request(parallel.workers, input_bytes, ineligible)
}

/// Decides the flagless adjacent-value lane and runs it, or declines.
///
/// `None` means "this request takes the ordinary route ladder", which is then reached with its code untouched.
#[inline(never)]
fn run_value_lane<W: IoWrite>(
    request: ValueRequest<'_>,
    parallel: crate::args::CliParallelSelection,
    compiled: &jqf_engine::CompiledProgram,
    resources: &mut jqf_resource::ResourceContext<'_>,
    sink: &mut StdoutSink<W>,
    diagnostics_buffer: Option<&jqf_sdk::Diagnostics>,
) -> Option<Result<(), CliFailure>> {
    let plan = plan_value_lane(
        compiled,
        parallel,
        request.input.len() as u64,
        request.exit_status,
        request.colour,
        resources.mismatch_policy() != jqf_resource::policy::MismatchPolicy::Lenient,
        resources.strictness() != jqf_resource::policy::StrictnessPolicy::Error,
        request.output.format,
        request.split,
        request.host_bindings,
        request.unbuffered,
    );
    let (plan, morsels) = partition_request(request.input, plan);
    if request.diagnostics {
        // Printed AFTER partitioning, so the decision on the line is the one the request will actually take.
        eprint_line_buffered(&format!("jqf: {plan}"));
        flush_stderr();
    }
    if !plan.is_parallel() {
        return None;
    }
    Some(run_value_request(
        request,
        plan,
        morsels,
        compiled,
        resources,
        sink,
        diagnostics_buffer,
    ))
}

/// Runs the flagless adjacent-value route on ordered shard workers.
///
/// Reached only when the plan says `parallel`; every other flagless request takes the ordinary route ladder, untouched.
/// That split is what keeps the serial path's code — and so its measurement — exactly what it was.
fn run_value_request<W: IoWrite>(
    request: ValueRequest<'_>,
    plan: ParallelPlan,
    morsels: jqf_runtime::values::ValueMorsels<'_>,
    compiled: &jqf_engine::CompiledProgram,
    resources: &mut jqf_resource::ResourceContext<'_>,
    sink: &mut StdoutSink<W>,
    diagnostics_buffer: Option<&jqf_sdk::Diagnostics>,
) -> Result<(), CliFailure> {
    let drive = ValueDriveSpec {
        input: request.input,
        source_name: "<stdin>",
        output: ValueOutputSpec {
            target: match request.output.format {
                CliFormat::Json => jqf_runtime::records::OutputTarget::Json,
                CliFormat::Ndjson => jqf_runtime::records::OutputTarget::Ndjson,
                CliFormat::JsonSeq => jqf_runtime::records::OutputTarget::JsonSeq,
                // Reached only when the plan is parallel, and parallel requires a JSON-family output format (the value
                // lane's `PlanDecision:OutputFormat` decline) — a non-JSON format here is a planner/lane invariant
                // break, never a silent wrong-format encode.
                _ => unreachable!("non-JSON output formats plan serial via OutputFormat"),
            },
            terminator: request.output.terminator,
            json: jqf_codec_json::JsonEncodeOptions {
                indent: request.output.indent,
                raw_strings: request.output.raw_strings,
                sort_keys: request.output.sort_keys,
                ascii_output: request.output.ascii_output,
                raw_output_nul: request.output.raw_output_nul,
            },
            no_newline: request.output.no_newline,
        },
        catalog: crate::routes::record_catalog(),
        cooperative_credits: COOPERATIVE_CREDITS,
    };
    let report =
        execute_value_request(drive, plan, morsels, compiled, resources, sink).map_err(render_value_drive_failure)?;
    // The route record and the cost snapshot are recorded whenever the buffer exists (Some for --diagnostics AND
    // --explain): this lane runs its own epilogue, the shared epilogue owns the ladder's Completed arms, and the RSS
    // gate's physical-report lanes read the accounted peak out of it (the ledger-vs-RSS ratio receipt). The route
    // record must land BEFORE the records are printed so the parallel arm's diag stream matches the serial twin's.
    if let Some(buffer) = diagnostics_buffer {
        // The sharded lane is its OWN route, distinct from the serial sequence rung: the explain block names the rung
        // that actually served the request, and a lane whose line is byte-identical to serial's cannot be told apart
        // from it.
        record_route(Some(buffer), "value-lane");
        buffer.record_cost(&crate::reported_cost_snapshot(resources));
    }
    if request.diagnostics {
        eprint_line_buffered(&format!("jqf: {report}"));
        if let Some(buffer) = diagnostics_buffer {
            print_diagnostic_records(buffer);
        }
        eprint_line_buffered(&format!(
            "jqf: precision_boundary_events={} declined_deferrals={}",
            resources.precision_boundary_events(),
            jqf_codec_core::declined_deferrals(),
        ));
    }
    sink.output
        .flush()
        .map_err(|error| format!("cannot flush stdout: {error}"))?;
    Ok(())
}

/// The serial tail: one [`jqf_sdk:Request`] → [`jqf_sdk:execute`].
///
/// Roundtrip, range-locate, input-family, and the floor live inside `execute`. The whole tail sits behind ONE
/// never-inlined call: an inline CLI chain cost sibling routes 7–10% with their own code untouched.
#[inline(never)]
pub(crate) fn serial_execute(ctx: &mut RouteContext<'_, '_>) -> Result<RouteOutcome, CliFailure> {
    if ctx.compiled.uses_input_family() && crate::routes::json_input_family_streamable(ctx) {
        let owned = ctx.input.to_vec();
        let request = crate::routes::base_request(
            ctx,
            Input::Streaming(Box::new(crate::routes::serve_whole_buffer(owned))),
        )
        .with_resources(ctx.resources)
        .with_input_family();
        jqf_sdk::execute(request, ctx.sink)
            .map_err(|error| record_and_render_failure(ctx.diagnostics, ctx.diagnostics_buffer, &error))?;
        ctx.sink
            .output
            .flush()
            .map_err(|error| format!("cannot flush stdout: {error}"))?;
        if ctx.diagnostics {
            eprint_line_buffered(&format!(
                "jqf: precision_boundary_events={}",
                ctx.resources.precision_boundary_events()
            ));
        }
        return Ok(RouteOutcome::Served);
    }
    let requirement = whole_requirement(ctx)?;
    let mut request = crate::routes::base_request(ctx, Input::Whole(ctx.input))
        .with_resources(ctx.resources)
        .with_requirement(&requirement);
    if ctx.compiled.uses_input_family() && !ctx.single_document_input {
        request = request.with_input_family();
    }
    let outcome = jqf_sdk::execute(request, ctx.sink)
        .map_err(|error| record_and_render_failure(ctx.diagnostics, ctx.diagnostics_buffer, &error))?;
    match outcome {
        Outcome::Served(Report::Sequence(report)) => {
            ctx.resources.note_strictness_warnings(report.codec_value_errors());
        }
        Outcome::Served(_) => {}
        Outcome::Declined => {
            return Err(CliFailure::from(
                "the serial execute tail declined; the floor must not decline",
            ));
        }
    }
    if ctx.compiled.uses_input_family() && !ctx.single_document_input {
        ctx.sink
            .output
            .flush()
            .map_err(|error| format!("cannot flush stdout: {error}"))?;
        if ctx.diagnostics {
            eprint_line_buffered(&format!(
                "jqf: precision_boundary_events={}",
                ctx.resources.precision_boundary_events()
            ));
        }
        return Ok(RouteOutcome::Served);
    }
    Ok(RouteOutcome::Completed)
}

/// Lowers the whole-read requirement under this program's policy.
///
/// The record drive's rung law applied to the whole-read routes: the requirement the codec binds is the same one the
/// program's own path class names, so a record-independent program (`empty`, literals) binds the lazy whole document
/// exactly like any other whole-document program and never materializes a value it does not read.
pub(crate) fn whole_requirement(
    ctx: &mut RouteContext<'_, '_>,
) -> Result<jqf_codec_core::AccessRequirement, CliFailure> {
    ctx.compiled
        .try_requirement(ctx.resources)
        .map_err(|error| requirement_failure(&error))
        .map(|requirement| crate::routes::with_decode_fact_intent(requirement, ctx, false))
}

/// Renders an adjacent-value drive's failure with the message its boundary owns.
fn render_value_drive_failure(error: ValueDriveError<std::io::Error>) -> CliFailure {
    match error {
        ValueDriveError::Setup { error, .. } => requirement_failure(&error),
        ValueDriveError::Pipeline(failure) => render_failure(&failure),
        ValueDriveError::Sink(failure) => CliFailure::from(format!("stdout failed: {failure}")),
        ValueDriveError::Resource(error) => CliFailure::Codec {
            kind: jqf_codec_core::CodecFailureKind::Resource(error),
            diagnostic: None,
        },
        ValueDriveError::Control(jqf_resource::ControlError::MemoryExceeded) => crate::rss::refusal_failure(),
        ValueDriveError::Control(error) => CliFailure::Codec {
            kind: jqf_codec_core::CodecFailureKind::Control(error),
            diagnostic: None,
        },
    }
}
