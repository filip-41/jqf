//! The route implementations, one module per lane family.
//!
//! Each function is one candidate in the resolved chain: it serves the request, declines (publishing nothing) so the
//! driver tries the next candidate, or fails. The record and edit lanes own the explicit input models; the value module
//! owns the sharded lane and the serial tail (`execute`).

mod edit;
mod follow;
mod record;
pub(crate) mod serve;
mod stream;
mod stream_events;
mod value;

use std::io::{self, Write as IoWrite};

use crate::errors::CliFailure;
use crate::plan::{Route, RouteContext, RouteOutcome};
use jqf_codec_core::FactIntent;

/// The process-lifetime catalog of the seven record formats the runtime's record and value drives serve (127 A3; render
/// joined when the record route began serving the presentation renderers, yaml when the record route began publishing
/// YAML items). The registrations are installed once through [`jqf_runtime:records:install_record_catalog`]; every spec
/// the routes build carries a copy, so a worker can rebuild its drive from its own numeric grant without naming a codec
/// crate.
#[must_use]
pub(crate) fn record_catalog() -> jqf_sdk::CodecCatalog<'static, 'static> {
    jqf_runtime::records::install_record_catalog(
        jqf_codec_json::registration().expect("the built-in JSON registration is valid"),
        jqf_codec_json::ndjson::registration().expect("the built-in NDJSON registration is valid"),
        jqf_codec_json::seq::registration().expect("the built-in json-seq registration is valid"),
        jqf_codec_delimited::registration().expect("the built-in CSV registration is valid"),
        jqf_codec_delimited::registration_tsv().expect("the built-in TSV registration is valid"),
        jqf_codec_render::registration().expect("the built-in render registration is valid"),
        jqf_codec_yaml::registration().expect("the built-in YAML registration is valid"),
        jqf_codec_xml::registration().expect("the built-in XML registration is valid"),
        jqf_codec_html::registration().expect("the built-in HTML registration is valid"),
    )
}

/// Serves one owned whole buffer to a streaming read callback in chunks.
///
/// The streaming input-family drives need a `'static` byte source (the host extension seam boxes the cursor), while the
/// retained input is borrowed for the request. A whole-file JSON input-family program pays one copy of the retained
/// bytes and then parses values ON DEMAND, which keeps the allocator high-water bounded by the current value instead of
/// one owned Value per record (the eager cursor's O(records) commit).
pub(crate) fn serve_whole_buffer(owned: Vec<u8>) -> impl FnMut(&mut [u8]) -> Result<usize, jqf_sdk::ReadFailure> {
    let offset = core::cell::Cell::new(0usize);
    move |buffer| {
        let start = offset.get();
        if start >= owned.len() {
            return Ok(0);
        }
        let count = (owned.len() - start).min(buffer.len());
        buffer[..count].copy_from_slice(&owned[start..start + count]);
        offset.set(start + count);
        Ok(count)
    }
}

/// Whether a whole-file source can be served on demand through the JSON event cursor: the JSON adjacent-value grammar
/// the streaming cursor speaks (the same grammar the pipe path already uses), a single source (the streaming cursor
/// serves one label), and an adjacent-value decode policy.
pub(crate) fn json_streamable_source(ctx: &crate::plan::RouteContext<'_, '_>) -> bool {
    ctx.format.as_str() == jqf_codec_json::FORMAT_ID
        && ctx.files.is_none_or(|files| files.len() <= 1)
        && ctx.policy.decode.allow_adjacent_values
}

/// Whether a whole-file input-family program can be served on demand.
pub(crate) fn json_input_family_streamable(ctx: &crate::plan::RouteContext<'_, '_>) -> bool {
    ctx.compiled.uses_input_family() && json_streamable_source(ctx)
}

/// The streamed `-n` input-family lane's byte source : ONE source — the named positional file, or stdin when none was
/// named — pulled in chunks as the cursor demands them. Nothing is read ahead of the pull, so the request never holds
/// more than the current chunk and the cursor's parse window.
///
/// An unreadable positional file follows the multi-file read law: reported at the read site and skipped, with
/// [`crate:input:record_missing_file`] carrying the exit-class consequence — which leaves the cursor empty exactly as
/// the combined whole-read would have.
pub(crate) fn serve_null_input_source(
    file: Option<&std::path::Path>,
) -> impl FnMut(&mut [u8]) -> Result<usize, jqf_sdk::ReadFailure> + 'static {
    use std::io::Read as _;
    enum Source {
        Stdin(std::io::StdinLock<'static>),
        File(std::fs::File),
        Missing,
    }
    let mut source = match file {
        Some(path) => match std::fs::File::open(path) {
            Ok(file) => Source::File(file),
            Err(error) => {
                crate::input::record_missing_file();
                crate::eprint_line_buffered(&format!(
                    "jqf: error: Could not open file {}: {}",
                    path.display(),
                    crate::input::io_error_text(&error)
                ));
                Source::Missing
            }
        },
        None => Source::Stdin(io::stdin().lock()),
    };
    move |buffer: &mut [u8]| loop {
        let read = match &mut source {
            Source::Stdin(stdin) => stdin.read(buffer),
            Source::File(file) => file.read(buffer),
            // The missing file was already reported; its stream is empty.
            Source::Missing => return Ok(0),
        };
        match read {
            Ok(count) => return Ok(count),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                return Err(jqf_sdk::ReadFailure::new(format!("cannot read input: {error}")));
            }
        }
    }
}

/// Runs one candidate of the resolved chain.
///
/// The match is exhaustive: a route added to the [`Route`] enum is a compile error here, which is the point of the
/// closed inventory.
pub(crate) fn run(route: Route, ctx: &mut RouteContext<'_, '_>) -> Result<RouteOutcome, CliFailure> {
    match route {
        Route::Follow => follow::run(ctx),
        Route::Stream => stream::run(ctx),
        Route::Record => record::run(ctx),
        Route::StreamEvents => stream_events::run(ctx),
        Route::Edit => edit::edit(ctx),
        Route::Diff => edit::diff(ctx),
        Route::NullFirst => edit::null_first(ctx),
        Route::Slurped => edit::slurped(ctx),
        Route::ValueLane => value::value_lane(ctx),
        Route::Execute => value::serial_execute(ctx),
    }
}

/// Builds the base request every route arm runs: the compiled program, the input, the format/dialect pair on both
/// sides, the pipeline policy and framing, the resource context, and the diagnostic stream, all from the route context.
/// The input-model flags (`.with_null_input`, `.slurped`, …) and the input shape are applied by the arm that owns the
/// model.
pub(crate) fn base_request<'request, 'context>(
    ctx: &RouteContext<'request, 'context>,
    input: jqf_sdk::Input<'request>,
) -> jqf_sdk::Request<'request, 'context, 'request> {
    let mut request = jqf_sdk::Request::new(ctx.compiled, input)
        .with_catalog(ctx.catalog)
        .with_source(ctx.source)
        .with_format(
            jqf_data::FormatId::try_new(ctx.format.as_str()).expect("built-in format identity"),
            jqf_data::DialectId::try_new(ctx.dialect.as_str()).expect("built-in dialect identity"),
        )
        .with_output_format(
            jqf_data::FormatId::try_new(ctx.output_format.as_str()).expect("built-in format identity"),
            jqf_data::DialectId::try_new(ctx.output_dialect.as_str()).expect("built-in dialect identity"),
        )
        .with_policy(ctx.policy)
        .with_framing(ctx.framing)
        .with_diagnostics(ctx.diagnostics_buffer)
        .with_label(ctx.source.label());
    if let Some(files) = ctx.files {
        request = request.with_files(files);
    }
    request
}

/// Sets [`FactIntent:Preserve`] when the decode must retain comments for a later encode, a source-preserving edit, or a
/// program that reads facts.
pub(crate) fn with_decode_fact_intent(
    requirement: jqf_codec_core::AccessRequirement,
    ctx: &RouteContext<'_, '_>,
    editing: bool,
) -> jqf_codec_core::AccessRequirement {
    let preserve = editing || matches!(ctx.output_format.as_str(), "jsonc" | "json5") || ctx.compiled.accesses_facts();
    requirement.with_fact_intent(if preserve {
        FactIntent::Preserve
    } else {
        FactIntent::None
    })
}

/// Serves `execute`'s outcome to the caller, mapping the report shape the arm's drive family produces and turning a
/// decline into the arm's own verdict. `expect_report` names the report family the arm requires.
pub(crate) fn outcome_report(
    outcome: jqf_sdk::Outcome,
    expect_report: &'static str,
) -> Result<jqf_sdk::Report, CliFailure> {
    match outcome {
        jqf_sdk::Outcome::Served(report) => Ok(report),
        jqf_sdk::Outcome::Declined => Err(CliFailure::from(format!("the {expect_report} drive declined"))),
    }
}

/// The shared epilogue the driver runs after a [`RouteOutcome:Completed`] candidate: flush the sink, record the run's
/// cost, print the retained diagnostic records and the precision-boundary count. The terminal arms (record, edit, the
/// single-run models, the sharded lane, the input-family drive) run their OWN epilogues and report
/// [`RouteOutcome:Served`] instead, exactly as the ladder's early returns did.
pub(crate) fn shared_epilogue(ctx: &mut RouteContext<'_, '_>) -> Result<(), CliFailure> {
    ctx.sink
        .output
        .flush()
        .map_err(|error| format!("cannot flush stdout: {error}"))?;
    if let Some(diagnostics) = ctx.diagnostics_buffer {
        diagnostics.record_cost(&crate::reported_cost_snapshot(ctx.resources));
    }
    if ctx.diagnostics
        && let Some(buffer) = ctx.diagnostics_buffer
    {
        crate::print_diagnostic_records(buffer);
    }
    if ctx.diagnostics {
        crate::eprint_line_buffered(&format!(
            "jqf: precision_boundary_events={} declined_deferrals={}",
            ctx.resources.precision_boundary_events(),
            jqf_codec_core::declined_deferrals(),
        ));
    }
    Ok(())
}
