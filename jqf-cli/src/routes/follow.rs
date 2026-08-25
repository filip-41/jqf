//! The `--follow` route: the record route driven over an incrementally-fed live stream (a growing file or a pipe),
//! never a whole input buffer.
//!
//! # The mode's contract
//!
//! `--follow` is an INPUT-path change, not a route change. Each cycle feeds the completed record range to the SAME
//! `execute_record_request` the whole-input record route uses — the same framer, the same payload ladder, the same
//! planner — so the parallel workers and ordered morsels engage on a large burst exactly as they would over a whole
//! file, and a small trickle stays on the serial path below the observed break-even. The whole-input path is untouched:
//! this module is entered only when `--follow` names it.
//!
//! The framing law is the framing law: a record is COMPLETE only when its physical terminator has been seen. Everything
//! after the last line feed is HELD — never framed, never faulted, never emitted — until more bytes arrive. This is
//! what makes a truncated trailing record harmless in a live tail. At pipe EOF the held tail is finalized through the
//! recovering dialect's own static law (a complete final value without a terminator is a record with an advisory; a
//! truncated one is an error issue), which is what keeps the pipe's exit byte-identical to a whole-input recovering
//! run.
//!
//! # The cycle
//!
//! 1. Read whatever new bytes are available (blocking on a pipe, a stat+seek
//!    poll on a regular file).
//! 2. Cut the retained buffer at the last line feed — the one cut the NDJSON
//!    framer makes exact. Frame `[0, cut)`; hold `[cut, len)`.
//! 3. Drive the record route over the completed range, then drop those bytes
//!    from the retained buffer so a long tail's memory stays bounded by the current partial record, not by the whole
//!    stream.
//!
//! A follow cycle's framer numbers offsets, line numbers, and record ordinals WITHIN the cycle (the source re-bases to
//! the cycle), so the facade re-bases them to the absolute file position before they reach stderr. The ordinal is
//! `base + relative`, the same accumulation line and offset already use.
//!
//! # The live-window arm
//!
//! An INPUT-FAMILY program (one that pulls `inputs`/`input`) is served by a different drive: the program runs ONCE over
//! the live stream, with the followed records fed through the input family as they arrive — the same single-run
//! streaming drive `-n` uses over a pipe (`execute_null_first_sequence_streaming`), with the follower as its byte
//! source. This is the shape the rolling/streaming builtin family (`windows`, `moving_avg`, `ewma`, `deltas`, …) needs:
//! its `foreach` state persists across the WHOLE stream, so a live tail is a true rolling window with bounded memory.
//! The per-record drive cannot serve it — `inputs` is empty under the per-record model, which is how the advertised
//! `--follow 'ewma(0.2; inputs|.ms)'` composition silently published nothing. NDJSON only: CSV's bytes are framed
//! records, not adjacent JSON, so an input-family program over a CSV tail is a usage error instead of a silent empty
//! answer.

use std::io::{self, Read, Seek as _, SeekFrom, Write as _};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt as _;
use std::time::Duration;

use jqf_codec_json::ndjson::NdjsonProfile;
use jqf_runtime::records::RecordInputKind;

use crate::errors::{CliFailure, ExitClass};
use crate::plan::{RouteContext, RouteOutcome};
use crate::record_route;
use crate::routes::record::{
    CycleOutcome, count_newlines, drive_range, physical_record_count, refuse_headered_csv, refuse_stream_stateful_yaml,
    require_record_capable_output,
};

/// How often a regular-file follow re-stats the file for growth.
///
/// A sleep+stat poll is the portable primitive for "wait for a file to grow" on this tree (no polling dependency is
/// committed); 100 ms keeps a live tail within a human-perceptible frame of the appended record while costing almost
/// nothing when the file is idle.
const FOLLOW_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The read chunk for both sources: 64 KiB, the same size the whole-stdin reader uses, so a follow read pays the same
/// syscall shape.
const READ_CHUNK: usize = 64 * 1024;

/// The live input being followed.
enum FollowSource {
    /// A regular file: read to EOF, then poll for growth by stat+seek. The run never ends on its own — like `tail -f`,
    /// it waits for the file to grow (or a signal). `path` is kept for ROTATION detection: the poll re-stats the PATH
    /// and compares inode identity with the fd, so a rename+recreate (default logrotate) is followed across the switch
    /// (, tail -F semantics).
    File {
        file: std::fs::File,
        /// The absolute byte offset this follower has consumed from the file.
        offset: u64,
        /// The path the follower was opened from; re-statted every poll to detect rotation.
        path: std::path::PathBuf,
    },
    /// A pipe-like stream: a named FIFO (the owned handle [`Self:open`] opened — it is carried here, never dropped) or
    /// stdin when no path was named. Reads block until data arrives and report EOF when every writer closes, which is
    /// what gives a FIFO its producer-restart semantics: a disconnect is EOF, and a restart is a new stream.
    Pipe { file: Option<std::fs::File> },
}

/// What one follow read produced.
enum FollowEvent {
    /// New bytes were appended to the retained buffer.
    Appended,
    /// No new bytes (a regular file that has not grown).
    NoData,
    /// The followed file shrank beneath the read cursor; the stream restarts from byte 0 as a fresh stream.
    Restarted,
    /// The followed file was rotated (the path now names a different inode).
    ///
    /// The old fd was drained into `retained`, the path was reopened, and the stream restarts at byte 0. The caller
    /// finalizes the old stream's tail (the drain + any held partial) with the OLD base, then resets. `drain_capped`
    /// says the drain stopped at the held-record cap, so the renamed file's bytes beyond the stop were neither
    /// delivered nor drivable — the caller diagnoses that loss.
    Rotated { drain_capped: bool },
    /// The pipe reached EOF; the held tail must be finalized.
    Eof,
}

impl FollowSource {
    /// Opens the followed input: the named positional file, or stdin.
    fn open(path: Option<&std::path::Path>) -> Result<Self, CliFailure> {
        match path {
            Some(path) => {
                let file = std::fs::File::open(path).map_err(|error| CliFailure::Message {
                    class: ExitClass::Usage,
                    message: format!("cannot open {}: {error}", path.display()),
                })?;
                let regular = file
                    .metadata()
                    .map_err(|error| CliFailure::Message {
                        class: ExitClass::Usage,
                        message: format!("cannot stat {}: {error}", path.display()),
                    })?
                    .is_file();
                if regular {
                    Ok(FollowSource::File {
                        file,
                        offset: 0,
                        path: std::path::PathBuf::from(path),
                    })
                } else {
                    // A named FIFO or other streaming special file: the open handle is CARRIED (reading it is what
                    // follows the FIFO), and reads block until a writer appears, exactly like stdin.
                    Ok(FollowSource::Pipe { file: Some(file) })
                }
            }
            None => {
                // Stdin follows as a stream: a redirected regular file is read to EOF and finalized, which makes `jqf
                // --follow` over a complete piped NDJSON stream byte-identical to the whole-input run over the same
                // bytes.
                Ok(FollowSource::Pipe { file: None })
            }
        }
    }

    /// Whether the PATH now names a different inode than the fd — the rename+recreate signature of default logrotate. A
    /// path that is temporarily absent (the rename happened, the create has not) is NOT a rotation: the old fd keeps
    /// being followed until the new file appears. Non-unix platforms have no inode identity to compare; rotation
    /// detection is a unix-only capability there (recorded narrowing).
    #[cfg(unix)]
    fn rotated(file: &std::fs::File, path: &std::path::Path) -> io::Result<bool> {
        let Ok(path_meta) = std::fs::metadata(path) else {
            return Ok(false);
        };
        let fd_meta = file.metadata()?;
        Ok(path_meta.dev() != fd_meta.dev() || path_meta.ino() != fd_meta.ino())
    }

    /// Non-unix: rotation detection is unavailable, so the follower keeps the fd-only behavior (the plan's evidence
    /// records this as a documented narrowing for platforms without inode identity).
    #[cfg(not(unix))]
    fn rotated(_file: &std::fs::File, _path: &std::path::Path) -> io::Result<bool> {
        Ok(false)
    }

    /// Reopens the followed path after rotation (the new file exists — the rotation check saw it). A failure here is a
    /// genuine I/O error.
    fn reopen(path: &std::path::Path) -> io::Result<std::fs::File> {
        std::fs::File::open(path)
            .map_err(|error| io::Error::other(format!("cannot reopen {} after rotation: {error}", path.display())))
    }

    /// The live-window byte source: reads whatever is available into `buf`, BLOCKING until data arrives.
    ///
    /// A pipe blocks on the read and reports EOF once (`Ok(0)`) when every writer closes. A regular file stat-polls for
    /// growth — the same sleep+stat primitive the per-cycle loop uses — and never reports EOF: like `tail -f`, the run
    /// waits for the file to grow (or a signal).
    fn read_into(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            FollowSource::File { file, offset, path } => loop {
                // Rotation: the path now names a different inode. Drain the old fd's tail one read at a time (tail -F
                // semantics), and switch to the new file only once the old fd is exhausted.
                if Self::rotated(file, path)? {
                    let read = match file.read(buf) {
                        Ok(0) => {
                            // The old fd is drained: reopen by path, reset the cursor, and follow the new file from
                            // byte 0.
                            *file = Self::reopen(path)?;
                            *offset = 0;
                            crate::eprint_line_buffered(&format!(
                                "jqf: follow: {}: file rotated; reopened",
                                path.display()
                            ));
                            // stderr is BUFFERED when it is not a terminal, so an advisory emitted mid-tail must be
                            // flushed here — a pipe consumer watching diagnostics has no process exit to rely on.
                            crate::flush_stderr();
                            continue;
                        }
                        Ok(read) => read,
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error) => return Err(error),
                    };
                    return Ok(read);
                }
                let len = file.metadata()?.len();
                if len < *offset {
                    *offset = 0;
                }
                if len > *offset {
                    file.seek(SeekFrom::Start(*offset))?;
                    let mut total = 0usize;
                    while total < buf.len() {
                        let read = match file.read(&mut buf[total..]) {
                            Ok(0) => break,
                            Ok(read) => read,
                            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                            Err(error) => return Err(error),
                        };
                        total += read;
                    }
                    *offset = offset.saturating_add(total as u64);
                    return Ok(total);
                }
                // The idle poll: drain any buffered stderr line first, so a pipe consumer sees an advisory within one
                // poll interval instead of whenever the buffer next fills or the run ends.
                crate::flush_stderr();
                std::thread::sleep(FOLLOW_POLL_INTERVAL);
            },
            FollowSource::Pipe { file } => {
                if let Some(file) = file {
                    read_pipe_into(file, buf)
                } else {
                    let mut stdin = io::stdin().lock();
                    read_pipe_into(&mut stdin, buf)
                }
            }
        }
    }

    /// Reads whatever is available into `retained`, reporting what happened and how many bytes were appended (the
    /// caller's per-cycle input charge: every byte this route pulls from the followed source crosses the ledger exactly
    /// once, when it is read).
    fn advance(
        &mut self,
        retained: &mut Vec<u8>,
        scratch: &mut [u8],
        kind: RecordInputKind,
    ) -> io::Result<(FollowEvent, u64)> {
        match self {
            FollowSource::File { file, offset, path } => {
                // Rotation: the path now names a different inode. Drain the old fd's ENTIRE remaining tail into
                // `retained` (tail -F semantics — records written to the renamed file before its writer closed must
                // still be delivered), reopen by path, and report the rotation so the caller finalizes the old stream
                // and restarts at byte 0.
                if Self::rotated(file, path)? {
                    let mut appended = 0u64;
                    let mut drain_capped = false;
                    // The per-chunk complete-span walk is only consumed by the held-cap check below. The delimited
                    // walks are a forward scan over the WHOLE retained buffer, so a large drain must not pay one per
                    // chunk: skip the walk when the cap is disarmed (the check answers false either way), and when
                    // armed rely on the span's monotonicity — the cut never moves backward as bytes append, so a suffix
                    // at or under the cap cannot hide a breach.
                    let cap = crate::rss::held_record_cap();
                    let mut last_cut = 0usize;
                    loop {
                        let read = match file.read(scratch) {
                            Ok(0) => break,
                            Ok(read) => read,
                            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                            Err(error) => return Err(error),
                        };
                        retained.extend_from_slice(&scratch[..read]);
                        appended = appended.saturating_add(read as u64);
                        if let Some(cap) = cap
                            && (retained.len() - last_cut) as u64 > cap
                        {
                            let cut = crate::routes::stream::complete_span(kind, retained);
                            last_cut = cut;
                            if crate::routes::stream::held_exceeds_cap(retained, cut) {
                                drain_capped = true;
                                break;
                            }
                        }
                    }
                    *file = Self::reopen(path)?;
                    *offset = 0;
                    return Ok((FollowEvent::Rotated { drain_capped }, appended));
                }
                let len = file.metadata()?.len();
                if len < *offset {
                    *offset = 0;
                    return Ok((FollowEvent::Restarted, 0));
                }
                if len == *offset {
                    return Ok((FollowEvent::NoData, 0));
                }
                file.seek(SeekFrom::Start(*offset))?;
                let mut total = 0usize;
                // Same gating as the rotation drain above: the walk serves only the held-cap check, and the span's
                // monotonicity lets an under-cap suffix skip it.
                let cap = crate::rss::held_record_cap();
                let mut last_cut = 0usize;
                loop {
                    let read = match file.read(scratch) {
                        Ok(0) => break,
                        Ok(read) => read,
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error) => return Err(error),
                    };
                    retained.extend_from_slice(&scratch[..read]);
                    total += read;
                    if let Some(cap) = cap
                        && (retained.len() - last_cut) as u64 > cap
                    {
                        let cut = crate::routes::stream::complete_span(kind, retained);
                        last_cut = cut;
                        if crate::routes::stream::held_exceeds_cap(retained, cut) {
                            break;
                        }
                    }
                }
                *offset = offset.saturating_add(total as u64);
                Ok((FollowEvent::Appended, total as u64))
            }
            FollowSource::Pipe { file } => {
                // A named FIFO: read the carried handle. EOF is a real disconnect — every writer closed the FIFO. Stdin
                // (None) is a redirected pipe, exactly as pre-existing follow semantics defined it.
                if let Some(file) = file {
                    read_pipe_loop(file, retained, scratch)
                } else {
                    let mut stdin = io::stdin().lock();
                    read_pipe_loop(&mut stdin, retained, scratch)
                }
            }
        }
    }
}

/// Reads one blocking chunk from a pipe-like source into `retained`. EOF is reported once; everything else appends and
/// reports `Appended`.
fn read_pipe_loop(source: &mut dyn Read, retained: &mut Vec<u8>, scratch: &mut [u8]) -> io::Result<(FollowEvent, u64)> {
    loop {
        let read = match source.read(scratch) {
            Ok(0) => return Ok((FollowEvent::Eof, 0)),
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        retained.extend_from_slice(&scratch[..read]);
        return Ok((FollowEvent::Appended, read as u64));
    }
}

/// Reads one blocking chunk from a pipe-like source into `buf`. EOF is reported once as `Ok(0)`; everything else
/// returns the count appended. The live-window drive's byte source (a pipe never returns less than a blocking read
/// gives it).
fn read_pipe_into(source: &mut dyn Read, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        match source.read(buf) {
            Ok(count) => return Ok(count),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

/// The live-window drive: the input-family program runs ONCE over the followed stream, with the records served through
/// `inputs`/`input` and the follower as the byte source.
///
/// This is the `-n` single-run streaming drive ([`jqf_sdk:execute_null_first_sequence_streaming`]) with the follower in
/// place of the pipe: the rolling-window builtins' `foreach` state persists across the whole stream, values publish as
/// they arrive (the per-item unbuffered sink, the flush-cadence law), and the cursor's parse window is the only
/// resident state, so a long tail stays bounded. The drive ends when the program stops pulling (a bare `input` answers
/// the first record and finishes — the `-n 'input'` law) or the pipe closes (EOF finalizes the held tail through the
/// adjacent parser's own lossy law); a regular-file tail never ends on its own, exactly like the per-record arm.
fn run_live_window(
    ctx: &mut RouteContext<'_, '_>,
    mut follower: FollowSource,
    label: &str,
) -> Result<RouteOutcome, CliFailure> {
    // Per-item flushing: a live tail must publish a record before the next one is read (the streaming input-family
    // arm's own unbuffered law — an engine-internal pull can block on the source with no seam for a pre-read flush, so
    // every published item flushes).
    ctx.sink.unbuffered = true;
    let label = label.to_string();
    let read_label = label.clone();
    // This arm reads its own input, so it charges what it actually pulled — every byte crosses the ledger exactly once,
    // even when the drive ends in a raise (a long tail must not report zero input for gigabytes read).
    let tally = crate::routes::stream::ReadTally::default();
    let read = crate::routes::stream::tallied(
        &tally,
        move |scratch: &mut [u8]| -> Result<usize, jqf_sdk::ReadFailure> {
            follower
                .read_into(scratch)
                .map_err(|error| jqf_sdk::ReadFailure::new(format!("cannot read {read_label}: {error}")))
        },
    );
    let request = crate::routes::base_request(ctx, jqf_sdk::Input::Streaming(Box::new(read)))
        .with_resources(ctx.resources)
        .with_label(&label)
        .with_null_input();
    let driven = jqf_sdk::execute(request, ctx.sink)
        .map_err(|failure| crate::record_and_render_failure(ctx.diagnostics, ctx.diagnostics_buffer, &failure));
    // Charge even when the drive failed: the ledger saw every read. The drive's own error keeps precedence over an
    // accounting refusal.
    let charged = crate::routes::stream::charge_input(ctx.resources, tally.get());
    driven.and(charged)?;
    ctx.sink
        .output
        .flush()
        .map_err(|error| format!("cannot flush stdout: {error}"))?;
    // The epilogue the sibling drives print (the stream route's own): cost snapshot, diagnostic records, and the
    // precision line — a live window's accounted-vs-RSS receipts are attributable like any other route's.
    if ctx.diagnostics {
        if let Some(buffer) = ctx.diagnostics_buffer {
            buffer.record_cost(&crate::reported_cost_snapshot(ctx.resources));
            crate::print_diagnostic_records(buffer);
        }
        crate::eprint_line_buffered(&format!(
            "jqf: precision_boundary_events={} declined_deferrals={}",
            ctx.resources.precision_boundary_events(),
            jqf_codec_core::declined_deferrals(),
        ));
    }
    crate::flush_stderr();
    crate::record_route(ctx.diagnostics_buffer, "follow");
    Ok(RouteOutcome::Served)
}

/// Runs the `--follow` route: incremental reads, per-cycle record drives.
///
/// The route serves the request itself ([`RouteOutcome:Served`]); the whole input is never materialized, so the
/// retained buffer holds only the held partial record plus the current cycle's bytes. Each cycle feeds the completed
/// range to the shared [`drive_range`], the same drive the streaming-stdin route uses.
#[expect(
    clippy::too_many_lines,
    reason = "one linear live-tail loop: read, cut, drive, drain, repeat — the D3 over-ceiling \
              arm keeps the cycle's refusal law in the same loop that drains the cycle's bytes"
)]
pub(crate) fn run(ctx: &mut RouteContext<'_, '_>) -> Result<RouteOutcome, CliFailure> {
    let profile = ctx
        .input_selection
        .dialect
        .ndjson_profile()
        .unwrap_or(NdjsonProfile::Recovering);
    // The record route's output guard: a document-shaped target must be served or rejected, never silently encoded as
    // JSON. The set is the record route's own `route_capabilities` declaration — the follow route IS the record route
    // fed incrementally and serves exactly its output set (json-seq included; a hand-written copy here once drifted).
    require_record_capable_output(ctx.output_selection, ctx.catalog)?;
    refuse_stream_stateful_yaml("the followed stream", &ctx.output_selection)?;
    let follow_label;
    let label = match ctx.follow_input {
        Some(path) => {
            follow_label = path.to_string_lossy();
            follow_label.as_ref()
        }
        None => "<stdin>",
    };
    // The input kind's EXACT cut, resolved from the request's dialect: the RFC 4180 quote-state walk is the cut for a
    // quoted-record format (a record ends at a line feed only outside a quoted field), and the streaming-stdin path
    // already drives it through the same `drive_range`.
    let kind = ctx
        .input_selection
        .dialect
        .record_kind()
        .unwrap_or(RecordInputKind::Ndjson);
    // The headered delimited dialects are not servable by a live tail: the header row is a whole-stream fact, and the
    // cycle drive re-opens the framer and the payload provider on every refill, so each cycle would consume its own
    // first record as a header and key the rest by that record's values — wrong bytes on any stream longer than one
    // chunk. Refuse before a byte is read; a seekable stdin redirect reads whole and serves it.
    if matches!(
        kind,
        RecordInputKind::Csv {
            header: true,
            tsv: false
        }
    ) {
        return Err(refuse_headered_csv("--follow", "CSV", ctx.input_selection.dialect.id()));
    }
    if matches!(
        kind,
        RecordInputKind::Csv {
            header: true,
            tsv: true
        }
    ) {
        return Err(refuse_headered_csv("--follow", "TSV", "tsv.utf8-header@1"));
    }
    let mut follower = FollowSource::open(ctx.follow_input)?;
    // The live-window arm: an input-family program runs ONCE over the live stream with records served through
    // `inputs`/`input` — the same single-run streaming drive `-n` uses over a pipe, fed by the follower. The
    // rolling/streaming builtin family (`windows`, `moving_avg`, `ewma`, `deltas`, …) is exactly this shape: its
    // `foreach` state persists across the WHOLE stream, so a live tail is a true rolling window with bounded memory. A
    // per-record drive would reset the state every record (`inputs` is empty under the per-record model — how the
    // advertised `--follow 'ewma(0.2; inputs|.ms)'` composition silently published nothing). NDJSON only: CSV's bytes
    // are framed records, not adjacent JSON, so an input-family program over a CSV tail is a usage error instead of a
    // silent empty answer.
    if ctx.compiled.uses_input_family() {
        if kind != RecordInputKind::Ndjson {
            return Err(CliFailure::Message {
                class: ExitClass::Usage,
                message: format!(
                    "an input-family program (one that pulls `inputs`/`input`) under --follow \
                     is served for NDJSON tails only; {kind:?} records are framed, not adjacent \
                     JSON, so the single-run streaming drive cannot serve them"
                ),
            });
        }
        return run_live_window(ctx, follower, label);
    }
    let mut retained: Vec<u8> = Vec::new();
    let mut scratch = vec![0u8; READ_CHUNK];
    let mut stream_base_offset: u64 = 0;
    let mut stream_base_line: u64 = 0;
    let mut stream_base_record: u64 = 0;
    let mut error_issues: u64 = 0;
    // The whole run's last-record law: a per-value error at the end of one cycle must not kill the live tail, but if
    // the true last record of the stream failed, the run exits with the runtime class.
    let mut last_value_error = false;
    loop {
        let (event, appended) = follower
            .advance(&mut retained, &mut scratch, kind)
            .map_err(|error| CliFailure::from(format!("cannot read {label}: {error}")))?;
        // This route reads its own input, so it charges each cycle's bytes here — the same law the streaming-stdin
        // route applies per read. Every byte the followed source delivered crosses the ledger exactly once, whether its
        // record drives this cycle or a later one.
        if appended > 0 {
            crate::routes::stream::charge_input(ctx.resources, appended)?;
        }
        match event {
            FollowEvent::Eof => {
                // A pipe closed: finalize the held tail through the recovering dialect's own static law, then exit. A
                // complete final value without a terminator becomes a record with an advisory; a truncated one becomes
                // an ordered error issue — exactly the whole-input recovering behavior over the same bytes. A drive
                // over an empty tail carries no record, so when the tail is empty the true last record lives in an
                // earlier cycle.
                let finalized = if retained.is_empty() {
                    false
                } else {
                    match drive_range(
                        ctx,
                        &retained,
                        kind,
                        profile,
                        stream_base_offset,
                        stream_base_line,
                        stream_base_record,
                        &mut error_issues,
                    ) {
                        Ok(outcome) => matches!(outcome, CycleOutcome::LastValueFailed),
                        // The sibling cycle sites' survival law: a refused finalize reports and goes on with whatever
                        // the run already drove — the exit class stays with the true last record that actually
                        // completed.
                        Err(drive_failure) if crate::rss::is_memory_refusal(&drive_failure) => {
                            crate::eprint_line_buffered(&format!("jqf: follow: {label}: {drive_failure}"));
                            crate::flush_stderr();
                            crate::rss::release_freed_pages();
                            false
                        }
                        Err(drive_failure) => return Err(drive_failure),
                    }
                };
                if error_issues > 0 || finalized || (retained.is_empty() && last_value_error) {
                    return Err(CliFailure::Reported);
                }
                record_route(ctx.diagnostics_buffer, "follow");
                return Ok(RouteOutcome::Served);
            }
            FollowEvent::Restarted => {
                // The file shrank beneath our cursor; treat it as a fresh stream from byte 0. The exit facts reset with
                // the bases: the discarded bytes' issues and last-record verdict belong to the stream that no longer
                // exists, and carrying them would fail the NEW stream's clean records hours later.
                retained.clear();
                stream_base_offset = 0;
                stream_base_line = 0;
                stream_base_record = 0;
                error_issues = 0;
                last_value_error = false;
            }
            FollowEvent::Rotated { drain_capped } => {
                // The path now names a different inode (default logrotate: rename + recreate). `advance` drained the
                // OLD fd's tail into `retained` and reopened; finalize the old stream — the drain plus any held partial
                // record — with the OLD base, per the recovering dialect's own tail law, then restart the stream at
                // byte 0 of the new file. A torn record at the seam is finalized as an ordered issue exactly like a
                // truncated tail at EOF: the new file's bytes do not continue it. The rotation itself is announced on
                // stderr, exactly as the live-window arm announces it in `read_into`; the line is flushed so a pipe
                // consumer sees the switch promptly.
                crate::eprint_line_buffered(&format!("jqf: follow: {label}: file rotated; reopened"));
                crate::flush_stderr();
                if !retained.is_empty() {
                    match drive_range(
                        ctx,
                        &retained,
                        kind,
                        profile,
                        stream_base_offset,
                        stream_base_line,
                        stream_base_record,
                        &mut error_issues,
                    ) {
                        // The old stream ENDS at the rotation: its last-record verdict is not carried anywhere (the new
                        // stream starts clean below), so only the drive's own side effects matter here.
                        Ok(_) => {}
                        // Same survival law as every cycle drive below: the tail must live through a rotation whose
                        // finalize crossed the ceiling.
                        Err(drive_failure) if crate::rss::is_memory_refusal(&drive_failure) => {
                            crate::eprint_line_buffered(&format!("jqf: follow: {label}: {drive_failure}"));
                            crate::flush_stderr();
                            crate::rss::release_freed_pages();
                        }
                        Err(drive_failure) => return Err(drive_failure),
                    }
                }
                retained.clear();
                stream_base_offset = 0;
                stream_base_line = 0;
                stream_base_record = 0;
                // The new file is a new stream: its exit facts start clean.
                error_issues = 0;
                last_value_error = false;
                // A rotation drain that stopped at the held-record cap cut the OLD file short: its bytes beyond the
                // stop were neither delivered nor drivable (the fd is gone). Diagnose the loss with the same message
                // the normal-cycle held-partial law prints — silent truncation is never acceptable.
                if drain_capped && let Some(cap) = crate::rss::held_record_cap() {
                    crate::eprint_line_buffered(&format!(
                        "jqf: follow: {label}: unterminated record exceeds the \
                         held-buffer cap of {cap} bytes"
                    ));
                    crate::flush_stderr();
                }
            }
            FollowEvent::Appended | FollowEvent::NoData => {}
        }
        // The input kind's exact cut (shared with the serve and stream routes): everything up to the last complete
        // record boundary is a complete record stream; everything after it is a held partial record. The boundary is
        // the newline for NDJSON and the RFC 4180 quote-state walk's record end for CSV.
        let complete_end = crate::routes::stream::complete_span(kind, &retained);
        if let Err(failure) = crate::routes::stream::govern_held_partial(&retained, complete_end) {
            // The held-partial law: an over-cap unterminated tail (or an RSS sample that crossed the ceiling mid-hold)
            // is refused with a diagnostic and the live tail goes on — the same survival law a cycle-crossing memory
            // refusal already keeps. Drive any complete prefix first so a mixed chunk is not dropped.
            if complete_end > 0 {
                let cycle_bytes = &retained[..complete_end];
                match drive_range(
                    ctx,
                    cycle_bytes,
                    kind,
                    profile,
                    stream_base_offset,
                    stream_base_line,
                    stream_base_record,
                    &mut error_issues,
                ) {
                    Ok(outcome) => {
                        last_value_error = matches!(outcome, CycleOutcome::LastValueFailed);
                    }
                    Err(drive_failure) if crate::rss::is_memory_refusal(&drive_failure) => {
                        crate::eprint_line_buffered(&format!("jqf: follow: {label}: {drive_failure}"));
                        crate::flush_stderr();
                        crate::rss::release_freed_pages();
                    }
                    Err(drive_failure) => return Err(drive_failure),
                }
                let newlines = count_newlines(cycle_bytes);
                stream_base_line = stream_base_line.saturating_add(newlines);
                // One ordinal per framed physical unit — the same walk the cut used, so the base advances exactly by
                // what this cycle drove.
                stream_base_record = stream_base_record.saturating_add(physical_record_count(kind, cycle_bytes));
                stream_base_offset = stream_base_offset.saturating_add(complete_end as u64);
                retained.drain(..complete_end);
            }
            crate::eprint_line_buffered(&format!("jqf: follow: {label}: {failure}"));
            crate::flush_stderr();
            crate::rss::release_freed_pages();
            retained.clear();
            continue;
        }
        if complete_end > 0 {
            let cycle_bytes = &retained[..complete_end];
            let cycle = drive_range(
                ctx,
                cycle_bytes,
                kind,
                profile,
                stream_base_offset,
                stream_base_line,
                stream_base_record,
                &mut error_issues,
            );
            match cycle {
                Ok(outcome) => {
                    last_value_error = matches!(outcome, CycleOutcome::LastValueFailed);
                }
                Err(failure) if crate::rss::is_memory_refusal(&failure) => {
                    // D3 : a cycle that crosses the physical ceiling reports a per-cycle diagnostic and the live tail
                    // goes on — one poison record must never kill the tail. The refused drive consumed the cycle's
                    // records, so the cycle is drained like a clean one (never re-driven; it would only refuse again),
                    // and the release step returns the freed pages so a long tail does not compound one record's
                    // footprint. The exit class stays with the true last record, exactly as for a per-value error: a
                    // refused MID-stream cycle the tail survived does not force the failure class by itself.
                    crate::eprint_line_buffered(&format!("jqf: follow: {label}: {failure}"));
                    crate::flush_stderr();
                    crate::rss::release_freed_pages();
                }
                Err(failure) => return Err(failure),
            }
            stream_base_line = stream_base_line.saturating_add(count_newlines(cycle_bytes));
            stream_base_record = stream_base_record.saturating_add(physical_record_count(kind, cycle_bytes));
            stream_base_offset = stream_base_offset.saturating_add(complete_end as u64);
            retained.drain(..complete_end);
        }
        if matches!(follower, FollowSource::File { .. }) {
            // The idle poll: drain any buffered stderr line first, so a pipe consumer sees an advisory within one poll
            // interval instead of whenever the buffer next fills or the run ends.
            crate::flush_stderr();
            std::thread::sleep(FOLLOW_POLL_INTERVAL);
        }
    }
}
