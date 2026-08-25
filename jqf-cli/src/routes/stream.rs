//! The streaming-stdin route: a NON-SEEKABLE stdin (pipe, FIFO, or socket) with a record-route input publishes per
//! value instead of reading whole — the default jqf used to hang on.
//!
//! # The seekability rule
//!
//! Stream when stdin is non-seekable AND the input has a record route; everything else keeps the whole-read routes.
//! That phrasing is load-bearing: `jqf … < file` redirects a regular file, which is seekable, so every whole-document
//! fast route survives untouched — the shape every benchmark measures. A pipe or FIFO (or socket) is not seekable and
//! streams per record. A terminal is neither: it reads whole, because there is no writer pushing bytes and per-line
//! framing would break a multi-line document a user is typing.
//!
//! # Two drives, one route
//!
//! The default RFC 8259 input is ADJACENT values (never inferred as NDJSON), so its arm drives the SDK's streaming
//! adjacent-value loop ([`jqf_sdk:execute_sequence_streaming`]): the same per-value decode the whole-read sequence
//! runs, publishing each complete value as its bytes arrive. The four record dialects (ndjson, json-seq, csv) own the
//! physical stream, so their arm is the follow route's cycle drive — the same [`drive_range`] — over a retained buffer
//! cut at the framing byte that is exact for the kind.
//!
//! Both arms end at EOF (a pipe closes), where the held tail is finalized through the dialect's own static law, which
//! is what keeps a complete piped stream byte-identical to the whole-input run over the same bytes.
//!
//! # The recorded costs
//!
//! - Piped input genuinely loses the whole-document fast routes: `cat
//!   big.ndjson | jqf` is slower than `jqf < big.ndjson`. That is correct — the whole-read path is naturally the faster
//!   one — and it must never be filed as a regression.
//! - The default-json arm is SERIAL: the sharded adjacent-value lane needs
//!   byte ranges a growing stream cannot hand out. A large burst still publishes per value, just without the parallel
//!   relay.
//! - A value that ends exactly at the window's end is HELD until the next
//!   byte proves it complete or invalid; at stream end it is finalized exactly as the whole-input run finalizes it. A
//!   genuinely invalid value that LOOKS truncated (`1.e`) therefore reports its error when the next byte arrives or the
//!   stream ends, whichever comes first — never a wrong answer, only a later one.
//! - Input-family programs (`input`/`inputs`) under the DEFAULT adjacent-JSON
//!   input are served by the demand-driven cursor drives: values are pulled from the pipe as the program demands them, so
//!   an early-exit query answers promptly and bytes it never demands are never parsed (the previously recorded whole-read
//!   narrowing, closed). The record dialects and `--stream` still read input-family programs whole: their decode tables
//!   and the event rewrite are whole-input by construction.

use std::io::{self, Read as _, Write as IoWrite};

use jqf_codec_json::ndjson::NdjsonProfile;
use jqf_runtime::records::RecordInputKind;
use jqf_sdk::{Input, ReadFailure, Report};

use crate::errors::CliFailure;
use crate::plan::{RouteContext, RouteOutcome};
use crate::routes::record::{
    CycleOutcome, count_newlines, drive_range, physical_record_count, refuse_headered_csv, refuse_stream_stateful_yaml,
    require_record_capable_output,
};
use crate::routes::value::whole_requirement;
use crate::{record_and_render_failure, record_route};

/// The read chunk of the framed (record-kind) arm: 64 KiB, the same size the whole-stdin reader and the follow route
/// use, so a streaming run pays the same syscall shape as the read-whole runs.
const READ_CHUNK: usize = 64 * 1024;

/// The bytes one streaming arm has pulled from stdin.
///
/// This route reads its own input, so it charges its own input — the whole-read routes charge the eager read where that
/// read happens. The SDK's pull callback is `'static` (it is stored behind the engine's host-extension seam), so it
/// cannot borrow the request account; the count travels here and reaches the ledger when the drive returns.
pub(crate) type ReadTally = std::rc::Rc<std::cell::Cell<u64>>;

/// Wraps a pull callback so every byte it delivers lands in `tally`.
pub(crate) fn tallied(
    tally: &ReadTally,
    mut read: impl FnMut(&mut [u8]) -> Result<usize, ReadFailure> + 'static,
) -> impl FnMut(&mut [u8]) -> Result<usize, ReadFailure> + 'static {
    let tally = ReadTally::clone(tally);
    move |scratch| {
        let count = read(scratch)?;
        tally.set(tally.get().saturating_add(count as u64));
        Ok(count)
    }
}

/// Charges bytes this route read to the request's input account.
pub(crate) fn charge_input(resources: &jqf_resource::ResourceContext<'_>, bytes: u64) -> Result<(), CliFailure> {
    resources.charge_input(bytes).map_err(|error| {
        CliFailure::from(format!(
            "error: cannot account the input: {}",
            crate::errors::resource_note(error)
        ))
    })
}

/// Serves the request with the streaming-stdin route.
///
/// The route is resolved only for a NON-SEEKABLE stdin (pipe/FIFO/socket) with a record-route input under the
/// per-record model, so it serves or fails — it never declines. It reports [`RouteOutcome:Served`] and runs its own
/// epilogue, like the record and follow routes.
pub(crate) fn run(ctx: &mut RouteContext<'_, '_>) -> Result<RouteOutcome, CliFailure> {
    match ctx.input_selection.dialect.record_kind() {
        // The default RFC 8259 input: ADJACENT values, framed by the decode itself, never a newline framer.
        None => run_adjacent(ctx),
        // The headered CSV dialect is not servable by the cycle drive: each refill re-opens the framer and the payload
        // provider, so every cycle would consume its own first record as a header and key the rest by that record's
        // values. A seekable stdin redirect reads whole and serves it; a pipe cannot. Refuse before a byte is read.
        Some(RecordInputKind::Csv {
            header: true,
            tsv: false,
        }) => Err(refuse_headered_csv(
            "streaming stdin",
            "CSV",
            ctx.input_selection.dialect.id(),
        )),
        Some(RecordInputKind::Csv {
            header: true,
            tsv: true,
        }) => Err(refuse_headered_csv("streaming stdin", "TSV", "tsv.utf8-header@1")),
        Some(kind) => run_framed(ctx, kind),
    }
}

/// The default-json arm: the streaming adjacent-value drive over stdin.
///
/// The drive publishes each complete value as its bytes arrive, holds a value that ends exactly at the window's end,
/// and at EOF finalizes the held tail through the whole-input route's own decode — the same bytes, the same failures,
/// the same line numbers (absolute from the stream start). Under `--stream` the arm is the INCREMENTAL EVENT drive
/// instead: the events themselves stream, with only the parser's path stack and the current fgets-shaped chunk
/// resident.
fn run_adjacent(ctx: &mut RouteContext<'_, '_>) -> Result<RouteOutcome, CliFailure> {
    let mut stdin = io::stdin().lock();
    if ctx.stream_events_requested {
        return run_stream_events(ctx, stdin);
    }
    if ctx.compiled.uses_input_family() {
        return run_input_family(ctx, stdin);
    }
    let requirement = whole_requirement(ctx)?;
    let tally = ReadTally::default();
    let request = crate::routes::base_request(
        ctx,
        Input::Streaming(Box::new(tallied(&tally, move |scratch: &mut [u8]| {
            stdin
                .read(scratch)
                .map_err(|error| ReadFailure::new(format!("cannot read stdin: {error}")))
        }))),
    )
    .with_resources(ctx.resources)
    .with_requirement(&requirement);
    let outcome = jqf_sdk::execute(request, ctx.sink)
        .map_err(|error| record_and_render_failure(ctx.diagnostics, ctx.diagnostics_buffer, &error))?;
    //: per-value codec failures (RawNulByte) are promotable
    // warnings under `--strictness strict`.
    let Report::Sequence(report) = crate::routes::outcome_report(outcome, "stream")? else {
        return Err(CliFailure::from("the streaming drive published a non-sequence report"));
    };
    charge_input(ctx.resources, tally.get())?;
    ctx.resources.note_strictness_warnings(report.codec_value_errors());
    // The drive's own sink flush law covers every refill; the final bytes land here, at the stream's end.
    ctx.sink
        .output
        .flush()
        .map_err(|error| format!("cannot flush stdout: {error}"))?;
    record_route(ctx.diagnostics_buffer, "stream");
    Ok(RouteOutcome::Completed)
}

/// The input-family arm: the demand-driven `input`/`inputs` drives over the pipe — values are parsed from stdin as the
/// driver or the program pulls them, never ahead of demand. `-n` runs the program once over `null` with the cursor
/// attached; otherwise the driver pulls the value stream and the program's own pulls advance the same cursor.
///
/// The sink turns per-item flushing ON for this arm: an engine-internal pull can block on the pipe with no seam for a
/// pre-read flush (the drive's own flush law only covers DRIVER pulls), so every published item flushes — the
/// flush-cadence law at item grain.
fn run_input_family(
    ctx: &mut RouteContext<'_, '_>,
    mut stdin: std::io::StdinLock<'static>,
) -> Result<RouteOutcome, CliFailure> {
    ctx.sink.unbuffered = true;
    let label = ctx.source.label().to_string();
    let tally = ReadTally::default();
    let read = tallied(&tally, move |scratch: &mut [u8]| {
        loop {
            match stdin.read(scratch) {
                Ok(count) => return Ok(count),
                // A signal-interrupted read retries, like the framed arm's.
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(ReadFailure::new(format!("cannot read stdin: {error}"))),
            }
        }
    });
    let mut request = crate::routes::base_request(ctx, Input::Streaming(Box::new(read)))
        .with_resources(ctx.resources)
        .with_label(&label);
    if ctx.null_input {
        request = request.with_null_input();
    } else {
        request = request.with_input_family();
    }
    jqf_sdk::execute(request, ctx.sink)
        .map_err(|failure| record_and_render_failure(ctx.diagnostics, ctx.diagnostics_buffer, &failure))?;
    charge_input(ctx.resources, tally.get())?;
    ctx.sink
        .output
        .flush()
        .map_err(|error| format!("cannot flush stdout: {error}"))?;
    record_route(ctx.diagnostics_buffer, "stream");
    Ok(RouteOutcome::Completed)
}

/// The `--stream` arm of the streaming-stdin route: the incremental event drive over the pipe — the one shape that
/// keeps the input unmaterialized, which is the flag's promise. The events publish as they land; a parse refusal is
/// terminal with the prefix standing (exit 5), exactly like the whole-input event route's.
fn run_stream_events(
    ctx: &mut RouteContext<'_, '_>,
    mut stdin: std::io::StdinLock<'static>,
) -> Result<RouteOutcome, CliFailure> {
    let tally = ReadTally::default();
    let request = crate::routes::base_request(
        ctx,
        Input::Streaming(Box::new(tallied(&tally, move |scratch: &mut [u8]| {
            stdin
                .read(scratch)
                .map_err(|error| ReadFailure::new(format!("cannot read stdin: {error}")))
        }))),
    )
    .with_resources(ctx.resources)
    .as_stream_events(ctx.stream_errors);
    jqf_sdk::execute(request, ctx.sink)
        .map_err(|error| record_and_render_failure(ctx.diagnostics, ctx.diagnostics_buffer, &error))?;
    charge_input(ctx.resources, tally.get())?;
    ctx.sink
        .output
        .flush()
        .map_err(|error| format!("cannot flush stdout: {error}"))?;
    record_route(ctx.diagnostics_buffer, "stream-events");
    Ok(RouteOutcome::Completed)
}

/// The record-dialect arm: the follow route's cycle drive over stdin, ending at EOF.
///
/// Each cycle feeds the completed record range — the framing byte's exact cut for the kind — to the shared
/// [`drive_range`], then drains it, so a long stream's retained memory stays bounded by the current partial record plus
/// one chunk. The held tail is finalized at EOF through the dialect's own static law, exactly like the whole-input run
/// over the same bytes.
fn run_framed(ctx: &mut RouteContext<'_, '_>, kind: RecordInputKind) -> Result<RouteOutcome, CliFailure> {
    // The record route's output guard: a document-shaped target must be served or rejected, never silently encoded as
    // JSON. The set is the record route's own `route_capabilities` declaration, not a second hand-written copy.
    require_record_capable_output(ctx.output_selection, ctx.catalog)?;
    refuse_stream_stateful_yaml("a streaming stdin", &ctx.output_selection)?;
    let profile = ctx
        .input_selection
        .dialect
        .ndjson_profile()
        .unwrap_or(NdjsonProfile::Recovering);
    let mut retained: Vec<u8> = Vec::new();
    let mut scratch = vec![0u8; READ_CHUNK];
    let mut stream_base_offset: u64 = 0;
    let mut stream_base_line: u64 = 0;
    let mut stream_base_record: u64 = 0;
    let mut error_issues: u64 = 0;
    let mut last_value_error = false;
    let mut stdin = io::stdin().lock();
    loop {
        // Blocking read: the cycle advances as soon as ANY bytes arrive.
        let read = match stdin.read(&mut scratch) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(CliFailure::from(format!("cannot read stdin: {error}"))),
        };
        // This arm reads stdin itself, so it charges the cycle's bytes here — the same law the callback arms apply
        // through their tally.
        charge_input(ctx.resources, read as u64)?;
        retained.extend_from_slice(&scratch[..read]);
        let complete_end = complete_span(kind, &retained);
        govern_held_partial(&retained, complete_end)?;
        if complete_end > 0 {
            last_value_error = matches!(
                drive_range(
                    ctx,
                    &retained[..complete_end],
                    kind,
                    profile,
                    stream_base_offset,
                    stream_base_line,
                    stream_base_record,
                    &mut error_issues,
                )?,
                CycleOutcome::LastValueFailed
            );
            let complete = &retained[..complete_end];
            stream_base_line = stream_base_line.saturating_add(count_newlines(complete));
            // One ordinal per framed physical unit — the same walk the cut used, so the base advances exactly by what
            // this cycle drove.
            stream_base_record = stream_base_record.saturating_add(physical_record_count(kind, complete));
            stream_base_offset = stream_base_offset.saturating_add(complete_end as u64);
            retained.drain(..complete_end);
        }
    }
    // A pipe closed: finalize the held tail through the dialect's own static law. A complete final record without a
    // terminator becomes a record with an advisory; a truncated one becomes an ordered error issue — exactly the
    // whole-input behavior over the same bytes. An empty tail means the true last record lives in an earlier cycle.
    let finalized = if retained.is_empty() {
        false
    } else {
        matches!(
            drive_range(
                ctx,
                &retained,
                kind,
                profile,
                stream_base_offset,
                stream_base_line,
                stream_base_record,
                &mut error_issues,
            )?,
            CycleOutcome::LastValueFailed
        )
    };
    // The whole-run epilogue, exactly like the record route's own: the cost snapshot is recorded here because this
    // route serves itself.
    if ctx.diagnostics
        && let Some(buffer) = ctx.diagnostics_buffer
    {
        buffer.record_cost(&crate::reported_cost_snapshot(ctx.resources));
        crate::print_diagnostic_records(buffer);
        crate::eprint_line_buffered(&format!(
            "jqf: precision_boundary_events={} declined_deferrals={}",
            ctx.resources.precision_boundary_events(),
            jqf_codec_core::declined_deferrals(),
        ));
    }
    // The recovering-dialect exit law: ONE error-severity issue anywhere in the stream forces the failure class even
    // when every later record succeeded. The `--seq` route SKIPS this check (its parse errors are never fatal), so its
    // exit is the program's own last-record result alone. The per-value last-record law is the follow route's: a
    // failure on the TRUE last record of the stream forces the class too.
    if (ctx.force_issue_exit && error_issues > 0) || finalized || (retained.is_empty() && last_value_error) {
        return Err(CliFailure::Reported);
    }
    record_route(ctx.diagnostics_buffer, "stream");
    Ok(RouteOutcome::Served)
}

/// After a read extends `retained`, enforce the held-partial law:
///
/// (a) the unterminated tail cannot exceed the armed RSS ceiling — a single record that never sends its terminator is
/// refused, never grown past the dial; (b) the RSS governor is sampled once per read, even when `complete_end == 0` (a
/// terminator-less flood never enters a drive cycle, so the cycle-only sample was a hole).
///
/// Shared by serve, follow, and this stream route: one law, one helper.
pub(crate) fn govern_held_partial(retained: &[u8], complete_end: usize) -> Result<(), crate::errors::CliFailure> {
    let held = retained.len().saturating_sub(complete_end) as u64;
    if let Some(cap) = crate::rss::held_record_cap()
        && held > cap
    {
        return Err(crate::errors::CliFailure::from(format!(
            "unterminated record exceeds the held-buffer cap of {cap} bytes"
        )));
    }
    crate::rss::check_point()
}

/// Whether the unterminated tail already exceeds the armed held-record cap. Inner read loops use this to stop extending
/// before the next chunk; the official refusal still comes from [`govern_held_partial`].
pub(crate) fn held_exceeds_cap(retained: &[u8], complete_end: usize) -> bool {
    crate::rss::held_record_cap().is_some_and(|cap| (retained.len().saturating_sub(complete_end) as u64) > cap)
}

/// The length of the longest COMPLETE record prefix of `bytes`, for the incremental record drives. A PROPOSER, not a
/// framer: the framing codec owns correctness over any range this names, and a wrong cut would publish bytes that
/// serial would not — so every cut below is the framing law's own exact boundary.
pub(crate) fn complete_span(kind: RecordInputKind, bytes: &[u8]) -> usize {
    match kind {
        // The NDJSON framer ends every record at the first line feed at or after its start, so the last line feed is
        // the exact cut: everything before it is a complete record stream, everything after it is the held partial
        // record.
        RecordInputKind::Ndjson => bytes
            .iter()
            .rposition(|&byte| byte == b'\n')
            .map_or(0, |index| index + 1),
        // A unit is RS (0x1E) plus bytes up to the next raw RS, and a raw RS ALWAYS boundaries — even mid-string or
        // mid-container. So the last RS starts the only possibly-incomplete unit: everything before it is complete, it
        // and its tail are held.
        RecordInputKind::JsonSeq => bytes.iter().rposition(|&byte| byte == 0x1E).map_or(0, |index| index),
        RecordInputKind::Csv { tsv: true, .. } => tsv_complete_span(bytes),
        RecordInputKind::Csv { tsv: false, .. } => csv_complete_span(bytes),
    }
}

/// The TSV complete-span walk, aligned with the framing authority (`jqf-codec/delimited/src/boundary.rs`, quote =
/// none): a record ends at LF or CRLF; a lone CR is a TERMINAL FRAMING FAULT for the framer, never a terminator.
/// Walking only to the first terminator left later records unframed, and cutting after a bare CR framed records the
/// framer refuses — so the walk carries the framer's law exactly: a CR as the buffer's LAST byte may be a CRLF whose
/// line feed arrives in the next read (hold), a CR with a known next byte names the WHOLE buffer so the drive hands the
/// framer the fault at its exact absolute offset.
fn tsv_complete_span(bytes: &[u8]) -> usize {
    let mut cursor = 0usize;
    let mut cut = 0usize;
    while cursor < bytes.len() {
        let mut index = cursor;
        let mut record_end: Option<usize> = None;
        let mut fault = false;
        let mut hold = false;
        while index < bytes.len() {
            match bytes[index] {
                b'\n' => {
                    record_end = Some(index + 1);
                    break;
                }
                b'\r' => match bytes.get(index + 1) {
                    Some(&b'\n') => {
                        record_end = Some(index + 2);
                        break;
                    }
                    Some(_) => {
                        fault = true;
                        break;
                    }
                    // A CR as this chunk's last byte may be a CRLF whose LF arrives in the next read: HOLD (cut
                    // unchanged) and decide then — cutting here would open the next cycle on a phantom record boundary.
                    None => {
                        hold = true;
                        break;
                    }
                },
                _ => index += 1,
            }
        }
        if fault {
            return bytes.len();
        }
        if hold {
            break;
        }
        match record_end {
            Some(end) => {
                cursor = end;
                cut = end;
            }
            None => break,
        }
    }
    cut
}

/// The last complete RFC 4180 record boundary: a record ends at a line feed only OUTSIDE a quoted field, so the walk
/// carries the quote state (a doubled quote is a literal quote; a lone quote mid-unquoted-field is NON-STRUCTURAL and
/// never toggles) exactly like the framer's own scanner.
///
/// A carriage return as the buffer's LAST byte is neither fault nor terminator: it may be a CRLF whose line feed
/// arrives in the next read, so the walk holds (cut unchanged) and decides on the next read. A bare carriage return
/// with a known next byte IS a TERMINAL framing fault for CSV (there is no recovering dialect): the walk stops and
/// names the WHOLE buffer, so the drive hands the framer the fault at its exact absolute offset — the same outcome the
/// whole-input run produces, and terminal either way, so no later byte can change it.
fn csv_complete_span(bytes: &[u8]) -> usize {
    let mut cursor = 0usize;
    let mut cut = 0usize;
    while cursor < bytes.len() {
        let mut index = cursor;
        let mut in_quotes = false;
        // The framer's own quote law (jqf-codec/delimited/src/boundary.rs): a quote OPENS quoted state only at a field
        // start (record start, or the byte after a delimiter outside quotes); inside quotes a quote closes unless
        // doubled; a lone quote mid-unquoted-field is NON-STRUCTURAL and never toggles. Mirroring it here is what keeps
        // the cut identical to the frame.
        let mut at_field_start = true;
        let mut record_end: Option<usize> = None;
        let mut fault = false;
        let mut hold = false;
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
                    Some(_) => {
                        fault = true;
                        break;
                    }
                    // A CR as this chunk's last byte may be a CRLF whose LF arrives in the next read: HOLD (cut
                    // unchanged) and decide then — never kill the stream on a split terminator.
                    None => {
                        hold = true;
                        break;
                    }
                },
                _ => {
                    if !in_quotes {
                        at_field_start = bytes[index] == b',';
                    }
                    index += 1;
                }
            }
        }
        if fault {
            return bytes.len();
        }
        if hold {
            break;
        }
        match record_end {
            Some(end) => {
                cursor = end;
                cut = end;
            }
            None => break,
        }
    }
    cut
}

#[cfg(test)]
mod tsv_span_tests {
    use super::tsv_complete_span;

    #[test]
    fn last_complete_record_not_just_the_first() {
        assert_eq!(tsv_complete_span(b"a\tb\nc\td\npartial"), 8);
        assert_eq!(tsv_complete_span(b"a\tb\r\nc\td\r\npartial"), 10);
    }

    #[test]
    fn crlf_split_across_reads_holds_instead_of_cutting() {
        // A CR as the chunk's last byte may be a CRLF whose LF arrives in the next read: hold (cut unchanged). Cutting
        // after the CR would open the next cycle on a leading LF — a phantom record boundary and an ordinal bump
        // against serial.
        assert_eq!(tsv_complete_span(b"a\tb\r"), 0);
        assert_eq!(tsv_complete_span(b"r1\na\tb\r"), 3);
        //...and the completed terminator frames on the next read.
        assert_eq!(tsv_complete_span(b"a\tb\r\n"), 5);
    }

    #[test]
    fn bare_cr_is_a_fault_that_names_the_whole_buffer() {
        // The framing authority (boundary.rs, quote = none): a lone CR is a terminal framing fault, never a terminator.
        // The walk names the whole buffer so the drive hands the framer the fault at its exact absolute offset — the
        // whole-input run's outcome over the same bytes.
        assert_eq!(tsv_complete_span(b"a\tb\rc\td"), 7);
        assert_eq!(tsv_complete_span(b"a\nb\rc\td\n"), 8);
    }

    #[test]
    fn no_terminator_holds_everything() {
        assert_eq!(tsv_complete_span(b"no terminator"), 0);
    }
}

#[cfg(test)]
mod csv_span_tests {
    use super::csv_complete_span;

    #[test]
    fn quoted_records_cut_at_their_out_of_quote_terminator() {
        assert_eq!(csv_complete_span(b"a,\"b\nc\",d\nnext"), 10);
        assert_eq!(csv_complete_span(b"a,\"b\"\"c\"\r\nnext"), 10);
    }

    #[test]
    fn crlf_split_across_reads_holds_instead_of_faulting() {
        // A CR as the chunk's last byte may be a CRLF whose LF arrives in the next read: hold (cut unchanged), never
        // kill the stream.
        assert_eq!(csv_complete_span(b"a,b\r"), 0);
        assert_eq!(csv_complete_span(b"r1\na,b\r"), 3);
        //...and the completed terminator frames on the next read.
        assert_eq!(csv_complete_span(b"a,b\r\n"), 5);
    }

    #[test]
    fn lone_quote_mid_field_is_not_structural() {
        // The framer's law: a `"` toggles only at field start. `a,b"x` must frame at its line feed instead of stalling
        // inside a phantom quoted field forever.
        assert_eq!(csv_complete_span(b"a,b\"x\nnext"), 6);
        assert_eq!(csv_complete_span(b"a,b\"x"), 0);
    }

    #[test]
    fn a_quote_still_opens_a_quoted_field_at_field_start() {
        assert_eq!(csv_complete_span(b"\"a\nb\",c\nnext"), 8);
    }
}
