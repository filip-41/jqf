//! The route decision: a closed [`Route`] inventory, the ordered candidate chain each request resolves to, and the
//! context every route function reads.
//!
//! The ladder the CLI used to walk with mutable booleans is a STRUCTURE here: [`resolve`] builds an ordered chain from
//! static request facts (flags, formats, program eligibility), and the driver walks it — each candidate either serves
//! the request or [`RouteOutcome:Declined`]s to the next, and the last candidate ([`Route:Execute`]) is the serial tail
//! that never declines: one [`jqf_sdk:Request`] → [`jqf_sdk:execute`]. A route added to the enum is a compile error
//! everywhere it must be handled; a format that cannot serve a route is a lookup miss at resolve time, never a silent
//! fallthrough to different bytes.

use std::path::Path;

use jqf_data::{DialectId, FormatId};
use jqf_engine::{CodecRequirementPolicy, CompiledProgram};
use jqf_resource::ResourceContext;
use jqf_sdk::{CodecCatalog, Diagnostics, FacadeFraming, PipelinePolicy};
use jqf_source::ResolvedSource;

use crate::args::{CliFormat, CliInputSelection, CliOutputSelection, CliParallelSelection};
use crate::output::{CliWriter, StdoutSink};

/// The cooperative work quantum shared by every drive the CLI runs: the SDK's own default, named once.
pub(crate) const COOPERATIVE_CREDITS: u32 = jqf_sdk::DEFAULT_COOPERATIVE_CREDITS;

/// One closed route the CLI can serve.
///
/// The variants mirror the ladder's rungs in their RESOLVED order; the [`resolve`] chain fixes the order, so the enum
/// itself carries no ordering law. The runtime-declining rungs (everything after [`Route:ValueLane`]) are attempts:
/// each may report [`RouteOutcome:Declined`] and let the driver try the next candidate. [`Route:Execute`] is the serial
/// tail.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Route {
    /// `--follow`: the record route driven over an incrementally-fed live stream (growing file or pipe), served before
    /// the whole-input lanes.
    Follow,
    /// A NON-SEEKABLE stdin (pipe/FIFO/socket) with a record-route input
    ///  the record route driven over the live stream, ending at EOF.
    Stream,
    /// Explicit NDJSON/CSV: the framer owns the physical stream.
    Record,
    /// `--stream`/`--stream-errors` over a JSON input: the bounded event route — the engine's `EventParser` walks the
    /// retained text into `[path, leaf]` events and the program runs per event (or once over the collected array under
    /// `-s`). Never declines for the JSON input shape; everything else keeps the `tostream` rewrite.
    StreamEvents,
    /// `--edit`: the document is the output subject; declines when the lane cannot serve the program.
    Edit,
    /// `--diff OLD NEW`: the fixed diff program over two single documents.
    Diff,
    /// `-n`: the filter runs once over `null`.
    NullFirst,
    /// `-s`: the filter runs once over the array of every decoded input.
    Slurped,
    /// The sharded adjacent-value lane; declines when the plan is serial.
    ValueLane,
    /// Serial tail: one [`jqf_sdk:Request`] → [`jqf_sdk:execute`]. The roundtrip / range-locate / input-family / floor
    /// order lives inside `execute`. Never declines.
    Execute,
}

/// What one candidate did with the request.
#[derive(Clone, Copy, Debug)]
pub(crate) enum RouteOutcome {
    /// The route served the request; the driver runs the shared epilogue (flush, cost record, diagnostic records,
    /// precision line).
    Completed,
    /// The route served the request and already ran its own epilogue.
    Served,
    /// The route declined and published nothing; try the next candidate.
    Declined,
}

/// Everything a route function reads, borrowed once by the driver.
///
/// One struct instead of a dozen arguments: every route takes `&mut RouteContext` and reads exactly the fields its rung
/// needs, so adding a rung cannot grow the driver's argument list. The two lifetimes separate the borrow of the request
/// state (`'a`) from the resource context's own inner borrow (`'b`), so the driver can hold `&mut ResourceContext<'b>`
/// without forcing the request borrow to outlive its body.
#[allow(
    clippy::struct_excessive_bools,
    reason = "RouteContext carries one bool per CLI flag a route must read; bundling them would \
              hide the flag surface from the routes that branch on it"
)]
pub(crate) struct RouteContext<'a, 'b> {
    pub(crate) catalog: CodecCatalog<'a, 'a>,
    pub(crate) source: ResolvedSource<'a>,
    /// The per-file byte ranges of a multi-file concatenation, when files were named. The value routes pass this to the
    /// SDK drives so `input_filename`/`input_line_number` resolve per value; single-file and stdin requests carry
    /// `None`.
    pub(crate) files: Option<&'a [jqf_source::SourceFileRange<'a>]>,
    pub(crate) format: &'a FormatId,
    pub(crate) dialect: &'a DialectId,
    pub(crate) output_format: &'a FormatId,
    pub(crate) output_dialect: &'a DialectId,
    pub(crate) policy: PipelinePolicy<'a>,
    pub(crate) framing: FacadeFraming<'a>,
    pub(crate) compiled: &'a CompiledProgram,
    pub(crate) resources: &'a mut ResourceContext<'b>,
    pub(crate) sink: &'a mut StdoutSink<CliWriter>,
    pub(crate) diagnostics: bool,
    /// On the SECOND lifetime: `execute` pins the diagnostics borrow to the resource context's inner lifetime, so the
    /// diagnostics ride `'b` and the request borrows (`'a`) stay independent of it.
    pub(crate) diagnostics_buffer: Option<&'b Diagnostics>,
    pub(crate) input: &'a [u8],
    pub(crate) input_selection: CliInputSelection,
    pub(crate) output_selection: CliOutputSelection,
    pub(crate) parallel_selection: CliParallelSelection,
    /// `--stream-errors`: the event route turns parse refusals into `[message, path]` events instead of aborting.
    pub(crate) stream_errors: bool,
    /// `--stream` over a JSON input: the event route is LIVE — the original program runs over `[path, leaf]` events,
    /// not the `tostream` rewrite. The streaming-stdin arm reads it to dispatch the incremental event drive.
    pub(crate) stream_events_requested: bool,
    /// The incremental `--stream` event drive: the route feeds the events from the SOURCE itself — a file argument, a
    /// seekable stdin redirect, or a pipe — in the adopted fgets-shaped chunks, with only the parser's path stack
    /// resident. False under `-n`/`-s` and for multi-file input, which keep the whole-input event route.
    pub(crate) stream_events_incremental: bool,
    /// `-n`/`--null-input`: the event route runs the program once over `null` with the events served through the input
    /// family, jq's canonical `-n --stream 'fromstream(inputs)'` idiom.
    pub(crate) null_input: bool,
    /// `-s`/`--slurp`: the event route collects the events into one array before the program runs, the adopted slurped
    /// stream.
    pub(crate) slurp: bool,
    /// `-e`/`--exit-status`: the value and record lanes plan serial for it, because the parallel relay publishes bytes
    /// and never a value to judge.
    pub(crate) exit_status: bool,
    /// Colour engagement: the value and record lanes plan serial for it, because the parallel relay publishes a byte
    /// stream without item boundaries and the colour pass renders per item (the raw-text skip arrives in the item
    /// report). The serial drives publish per item with their reports, exactly as the `-e` and mismatch declines do.
    pub(crate) colour: bool,
    /// Which input model the RECORD route runs under: `-n` and `-s` read the record stream exactly as they read
    /// adjacent values.
    pub(crate) record_model: jqf_runtime::records::RecordRunModel,
    /// The json-seq framing profile the record route frames under: the flag-scoped RECOVERING profile under `--seq`,
    /// strict for an explicit json-seq selection (`json-seq.recover@1` is never a registered dialect).
    pub(crate) json_seq_profile: jqf_codec_json::seq::JsonSeqProfile,
    /// Whether record issues FORCE the request's failure class. The standing NDJSON recovering law says yes; the
    /// adopted `--seq` route says NO — its parse errors are never fatal (every parse-error case exits 0), so the seq
    /// route leaves the exit to the program's own last-record result.
    pub(crate) force_issue_exit: bool,
    /// The positional input file `--follow` tails, when one was named.
    pub(crate) follow_input: Option<&'a Path>,
    /// The streamed `-n` input-family lane : the request is served by pulling bytes on demand, so no whole buffer was
    /// read; the single positional file to open, or stdin when `None`.
    pub(crate) streamed_null_first: bool,
    pub(crate) streamed_null_first_file: Option<&'a Path>,
    /// `--edit --check`: the edit lane compares its would-be output with the input and returns a silent exit-1 on any
    /// difference instead of writing.
    pub(crate) edit_check: bool,
    /// `--edit`: the request is the source-preserving edit lane. The record route reads it to serve a record-format
    /// edit instead of a plain per-record run.
    pub(crate) edit: bool,
    /// `--arg`/`--argjson`/`--args`/`-L` (or any other host binding): a morsel worker does not carry the CLI's binding
    /// table, so the request plans serial.
    pub(crate) host_bindings: bool,
    pub(crate) in_place: Option<&'a Path>,
    pub(crate) output_path: Option<&'a Path>,
    pub(crate) no_atomic: bool,
    pub(crate) diff_pair: Option<(&'a Path, &'a Path)>,
    /// The per-side `--diff` input selections: each defaults to the request's input selection;
    /// `--old-format`/`--new-format` swap one side to another format's selection.
    pub(crate) diff_old_selection: CliInputSelection,
    pub(crate) diff_new_selection: CliInputSelection,
    pub(crate) single_document_input: bool,
    pub(crate) compile_policy: CodecRequirementPolicy,
}

#[allow(
    clippy::too_many_arguments,
    clippy::fn_params_excessive_bools,
    reason = "the resolver reads every input-model flag and selection once, from the               destructured CliArguments; bundling them would re-introduce a struct the               driver already owns"
)]
/// Resolves one request to its ordered candidate chain.
///
/// The order is the ladder's law: the record and edit lanes own the explicit input models, the single-run models
/// (`-n`/`-s`/`--diff`) come next, the sharded value lane is a routing attempt that declines to serial, and the serial
/// tail is one [`jqf_sdk:execute`] call. The chain is exhaustive by construction — [`Route:Execute`] is always last and
/// never declines.
pub(crate) fn resolve(
    input_selection: CliInputSelection,
    output_selection: CliOutputSelection,
    edit: bool,
    diff_pair: Option<(&Path, &Path)>,
    null_input: bool,
    slurp: bool,
    raw_input: bool,
    follow: bool,
    stream_stdin: bool,
    stream_input_family: bool,
    streamed_null_first: bool,
    stream_events_requested: bool,
    compiled: &CompiledProgram,
    single_document_input: bool,
) -> Vec<Route> {
    let mut chain: Vec<Route> = Vec::with_capacity(15);
    // A RECORD input owns its own physical stream and serves every input model itself (`-n`/`-s` included, D3), so the
    // adjacent-value single-run routes below stay off the chain for it rather than sitting dead behind a route that
    // never declines. `--diff` is the exception: it owns its OWN two files and never touches stdin, so a record format
    // on a side is the exactly-one-document law, not a per-record drive — the diff lane must serve, or an NDJSON side
    // silently ran the diff program once per record and published nothing. The streamed `-n` input-family lane owns its
    // shape before the record route: a null-input program pulling `input`/`inputs` over one strict-NDJSON or
    // adjacent-JSON source is served by [`Route:NullFirst`] through the on-demand cursor, so the whole-input record
    // read never happens.
    let record_input =
        !streamed_null_first && !follow && diff_pair.is_none() && input_selection.dialect.record_kind().is_some();
    if follow {
        // The follow route serves the request by itself; the whole-input lanes it would otherwise share the chain with
        // are unreachable behind it.
        chain.push(Route::Follow);
    } else if stream_stdin
        && input_selection.dialect.has_record_route()
        && !null_input
        && !slurp
        && !raw_input
        && !edit
        && diff_pair.is_none()
        && !compiled.uses_input_family()
    {
        // The seekability rule: a non-seekable stdin with a record-route input publishes per value instead of reading
        // whole. The per-record model only — `-s` reads to completion, `-R` rewrites the whole input, and the edit/diff
        // lanes own their own input models.
        chain.push(Route::Stream);
    } else if stream_input_family {
        // The streaming input-family lane: the same route, dispatched to the demand-driven `input`/`inputs` drives
        // inside its adjacent arm. The fact is computed once in the driver (with the read deferral), so the chain
        // cannot drift from the bytes: when it holds, stdin was NOT read whole and the stream route must serve.
        chain.push(Route::Stream);
    } else if record_input {
        chain.push(Route::Record);
    }
    // The bounded `--stream` event route: a JSON input with `--stream`/`--stream-errors` (and the original,
    // non-rewritten program) is served here, before the ordinary ladder — the events ARE the input model, exactly as
    // `-n`/`-s` own theirs. The route handles both the per-event model and `-s`'s collection; `-n`/`-R` take precedence
    // and never reach here (the flag is ignored under them, the adopted law).
    if stream_events_requested {
        chain.push(Route::StreamEvents);
    }
    if edit {
        chain.push(Route::Edit);
    }
    if diff_pair.is_some() {
        chain.push(Route::Diff);
    }
    if null_input && !record_input {
        chain.push(Route::NullFirst);
    }
    if slurp && !raw_input && !record_input {
        // the adopted precedence, `-n` beats `-R`, and `-R` beats `-s` — `jq -sR '.'` is the whole input as ONE string,
        // not an array wrapping it. With `-R` the raw transform already consumed the slurp into its whole-input
        // spelling, so the ordinary ladder runs over the transformed bytes instead.
        chain.push(Route::Slurped);
    }
    if !single_document_input
        && input_selection.format != CliFormat::Yaml
        && input_selection.format != CliFormat::Jqft
        && input_selection.format != CliFormat::CborSeq
        && output_selection.format != CliFormat::Render
    {
        // A SINGLE-DOCUMENT input (TOML/CBOR) is not adjacent JSON values: the sharded lane must never scan it for JSON
        // value boundaries, so it is gated off for that format entirely. The input-model fact comes from the codec's
        // `route_capabilities` declaration (039 item 1a) — the absence of `RouteCapability:AdjacentValues` — not from a
        // hand-written format list. YAML and CBOR-sequence are adjacent-value formats too, but the sharded lane's
        // cut-scanner and worker encoding are JSON-hardcoded end-to-end (`jqf-runtime`'s `ValueDriveSpec` carries no
        // format/dialect field at all) — it understands `{`/`}`/`[`/`]` nesting, not YAML's `---`/`...` document
        // grammar or CBOR major-type item cuts, so both are excluded by format, not by the input model. Render output
        // stays on the serial ladder because the value-lane relay serves JSON-family targets only
        // (`PlanDecision:OutputFormat` in the value route's planner: only Json|Ndjson|JsonSeq travel it); a worker
        // cannot encode render dialects per value.
        chain.push(Route::ValueLane);
    }
    chain.push(Route::Execute);
    chain
}

/// The incremental `--stream` event drive's one gate, spelled ONCE: plain `--stream` over at most one JSON source takes
/// the drive that reads the SOURCE itself unless the program pulls the input family — its shared event cursor is
/// whole-input by construction, so it stays on the whole-input event route. The driver's read-deferral arm and the
/// request fact MUST stay conjunct-equal (the read is deferred exactly when the incremental drive serves), so both
/// consult this predicate instead of re-spelling the conjunction and drifting.
pub(crate) fn stream_events_incremental_route(stream_events_incremental: bool, compiled: &CompiledProgram) -> bool {
    stream_events_incremental && !compiled.uses_input_family()
}
