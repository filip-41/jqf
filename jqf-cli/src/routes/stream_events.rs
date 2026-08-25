//! The bounded `--stream` event route: the engine's [`EventParser`] walks the retained JSON text into the adopted
//! `[path, leaf]` events one at a time, the program runs per event (or once over the collected array under `-s`), and
//! the SDK's event drive publishes through the ordinary encoder.
//!
//! This is the BOUNDED realization of the `tostream | P` rewrite: the parsed document never exists, so a document
//! larger than the peak-RSS lane's ceiling streams with only the path stack resident. `--stream-errors` (which implies
//! `--stream`, the adopted coupling) turns parse refusals into `[message, path]` events; under plain `--stream` a
//! refusal is terminal with every earlier event's bytes already published, exactly as adopted exits 5 with its prefix
//! standing.

use std::io::Write as IoWrite;
use std::io::{self, Read as _};

use crate::errors::CliFailure;
use crate::plan::{RouteContext, RouteOutcome};
use crate::{eprint_line_buffered, print_diagnostic_records, record_and_render_failure, record_route};
use jqf_sdk::{Input, ReadFailure};

pub(crate) fn run(ctx: &mut RouteContext<'_, '_>) -> Result<RouteOutcome, CliFailure> {
    // The incremental arm: plain `--stream` over a JSON input reads the events from the SOURCE itself — a file
    // argument, a seekable stdin redirect, or a pipe — in the adopted fgets-shaped chunks, with only the parser's path
    // stack plus the current chunk resident. the adopted `--stream` parses incrementally regardless of seekability; the
    // whole-input event route below is the `-n`/`-s`/multi-file model, whose collection shapes the corpus pins.
    if ctx.stream_events_incremental {
        return run_incremental(ctx);
    }
    // `-n --stream`: the adopted canonical streaming idiom runs the program ONCE over null, with the input family
    // pulling `[path, leaf]` events from the shared parser — `-n --stream 'fromstream(inputs)'` reconstructs the
    // document. Without `-n`, the program runs once per event (and the input family pulls further events from the same
    // parser).
    let mut request = crate::routes::base_request(ctx, Input::Whole(ctx.input))
        .with_resources(ctx.resources)
        .as_stream_events(ctx.stream_errors);
    if ctx.null_input {
        request = request.with_null_input();
    }
    if ctx.slurp {
        request = request.slurped();
    }
    jqf_sdk::execute(request, ctx.sink)
        .map_err(|error| record_and_render_failure(ctx.diagnostics, ctx.diagnostics_buffer, &error))?;
    finish(ctx)
}

/// The incremental arm: the events are fed from the source itself in the reference's fgets-shaped chunks (≤4091 bytes
/// ending at a newline), so a document larger than memory streams with only the parser's path stack resident — the
/// promise the whole-input route could not keep (it read the source whole first). The drive is the same
/// `execute_stream_events_streaming` the non-seekable-stdin arm uses; this route reaches it for a file argument and a
/// seekable stdin redirect too.
fn run_incremental(ctx: &mut RouteContext<'_, '_>) -> Result<RouteOutcome, CliFailure> {
    let label = ctx.source.label().to_string();
    // The source is the named file when one was given, stdin otherwise. The drive reads it in fgets-shaped chunks and
    // the parser's window plus the current chunk is the whole resident state.
    let mut source: Box<dyn io::Read> = if label == "<stdin>" {
        Box::new(io::stdin().lock())
    } else {
        Box::new(std::fs::File::open(&label).map_err(|error| CliFailure::Message {
            class: crate::errors::ExitClass::Usage,
            message: format!("cannot open {label}: {error}"),
        })?)
    };
    let read_label = label.clone();
    let read = move |scratch: &mut [u8]| -> Result<usize, ReadFailure> {
        loop {
            match source.read(scratch) {
                Ok(count) => return Ok(count),
                // A signal-interrupted read retries, like the streaming-stdin arms'.
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    return Err(ReadFailure::new(format!("cannot read {read_label}: {error}")));
                }
            }
        }
    };
    let request = crate::routes::base_request(ctx, Input::Streaming(Box::new(read)))
        .with_resources(ctx.resources)
        .as_stream_events(ctx.stream_errors);
    jqf_sdk::execute(request, ctx.sink)
        .map_err(|error| record_and_render_failure(ctx.diagnostics, ctx.diagnostics_buffer, &error))?;
    finish(ctx)
}

/// The epilogue both `--stream` event arms share, spelled once: the route label, the sink flush, and the gated
/// diagnostics lines. Any new diagnostics line or flush-law change lands here for both arms.
fn finish(ctx: &mut RouteContext<'_, '_>) -> Result<RouteOutcome, CliFailure> {
    record_route(ctx.diagnostics_buffer, "stream-events");
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
    if ctx.diagnostics
        && let Some(buffer) = ctx.diagnostics_buffer
    {
        print_diagnostic_records(buffer);
    }
    Ok(RouteOutcome::Served)
}
