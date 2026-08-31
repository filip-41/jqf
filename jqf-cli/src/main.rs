//! Thin stdin/stdout facade for the strict-JSON and NDJSON verticals.
//!
//! Usage: `jqf [OPTIONS] [PROGRAM]`
//!
//! `PROGRAM` is at most one jq-style filter argument; omitting it runs the identity filter. Formats are NEVER
//! auto-detected: JSON/RFC 8259 stays the input and output default — including the adopted adjacent-value stdin, which
//! is not NDJSON — and NDJSON must be selected explicitly.

mod allocator;
mod args;
mod colour;
mod config;
mod discovery;
mod errors;
mod explain;
mod input;
mod loader;
mod output;
mod plan;
mod provenance;
mod routes;
mod rss;

use std::env;
use std::fs;
use std::io::{self, IsTerminal as _, Write as IoWrite};
use std::process::ExitCode;
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use crate::args::{
    BindingKind, CliArguments, CliBinding, CliCommand, CliFormat, CliInputDialect, CliInputSelection, CliOutputDialect,
    PositionalArg, help_text, parse_arguments, short_help_text,
};
use crate::errors::{CliFailure, ExitClass, compile_failure, resource_note};
use crate::input::{
    io_error_text, parse_single_json_value, raw_input_bytes, rawfile_value, read_binding_file, read_combined_input,
    read_input_file, slurpfile_value,
};
use crate::loader::{CliModuleLoader, host_environment};
use crate::output::{CliStderr, CliWriter, SplitDestination, StdoutSink};
use crate::plan::{COOPERATIVE_CREDITS, RouteContext, RouteOutcome};
use jqf_codec_core::{DecodeRequest, DiagnosticPolicy, ItemByteOwner, PreservationRequest, ValidationMode};
use jqf_codec_json::JsonEncodeOptions;
use jqf_codec_json::ndjson::NdjsonEncodeOptions;
use jqf_codec_yaml::YamlTargetSchema;
use jqf_data::{DialectId, FormatId};
use jqf_engine::{CodecRequirementPolicy, CompileOptions, EngineCompileError, try_compile_program};
use jqf_resource::policy::ProjectionKind;
use jqf_resource::{RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_sdk::{CodecCatalog, FacadeFraming, PipelinePolicy};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

/// The CLI has NO output ceiling: published bytes stream to the host as the run produces them, exactly as they do under
/// the adopted process, and the RSS governor (default 80% of effective memory, `--max-rss`) is the operator's opt-in
/// physical bound. The old 256 MiB output constant refused work every competitor completed and aborted mid-stream with
/// a partial output — the same failure the input and memory ceilings had, and the same reason they were removed: an
/// output ceiling is a bootstrap number, never a representation law.
///
/// The memory ceiling is opt-in to exactly the same degree: it is one an operator opts INTO with `--max-memory-bytes`,
/// never one jqf imposes on a document the machine could plainly hold. The old 512 MiB default refused work every
/// competitor completed — `[.[]|.a]|add` over a 26.9 MB array of a million objects needs 565 MiB of ledger, and jqf's
/// peak RSS on it (618.7 MB) is BELOW the adopted (716.6 MB) — so the refusal was the number, not the representation.
///
/// Accounting itself is unaffected: every byte is still admitted, every category is still attributed and the authorized
/// peak is still observable. Only the denial is gone, so the ledger keeps its diagnostic value at every ceiling.
pub(crate) const DEFAULT_MAX_MEMORY_BYTES: u64 = u64::MAX;
/// Decode/encode nesting ceiling, matching parser cap exactly: jq accepts a document nested 10,000 deep and rejects
/// 10,001, and so does jqf.
///
/// It is a ceiling, not a reservation. Every consumer of `ResourceLimits:max_nesting_depth` either compares against it
/// in O(1) (`RequestAccount:enter_depth`, one counter compare per level) or clamps its preallocation with `.min(16)`
/// and grows on demand, so raising it costs nothing on a shallow document — measured, not assumed (perf-floor-trims.md
/// T5). The frame stacks it bounds are all heap vectors driven iteratively (the JSON parser's `frames`, the encoder's
/// `frames`, the descent walk's executor frames), so document depth never becomes native stack depth.
const MAX_NESTING_DEPTH: u32 = 10_000;

/// Builds the request's resource limits from the CLI dials. One computation, shared by the threaded request ledger and
/// the ambient counting-allocator ledger, so the two can never disagree about a ceiling.
///
/// No input ceiling: the retained input is what every route borrows, and bounding it with a bootstrap constant refused
/// work jq completes. No output ceiling either: the ceiling is a bootstrap number, not a representation law (see the
/// module doc); the RSS governor bounds physical memory. The serial and parallel lanes both charge the output bytes
/// against the output limit, so a host that sets one is honored on every route.
fn request_limits(max_memory_bytes: u64, max_spill_bytes: u64, max_spill_disk_bytes: u64) -> ResourceLimits {
    let mut limits = ResourceLimits::new(u64::MAX, u64::MAX, max_memory_bytes, max_spill_bytes, MAX_NESTING_DEPTH);
    // The cumulative spill-disk ceiling: opt-in, 0 = unset. It rides the same switch as the spill budget — a disk
    // ceiling without a store is moot, and the store installs only when the budget is set.
    if max_spill_disk_bytes > 0 {
        limits = limits.with_max_spill_disk_bytes(max_spill_disk_bytes);
    }
    limits
}
/// The process-wide BUFFERED stderr channel.
///
/// Every diagnostic, per-value error, record issue, and `stderr/0` write goes through this one channel. the adopted C
/// `stderr` is unbuffered, so it pays one `write(2)` per error; jqf measured the same spelling at ~6 us of a ~7 us
/// per-error cost on the 400k-error NDJSON lane (`.[] |.missing` over single-field records, 2.81 s vs the adopted 0.84
/// s), with the syscall the dominant half — so the CLI buffers and flushes:
///
/// - on every write when stderr is a TERMINAL (the adopted interactive visibility
///   law: an error appears before the prompt returns);
/// - automatically at the channel's capacity (64 KiB, `BufWriter`'s own
///   growth);
/// - explicitly at process exit, after one-time diagnostics lines, and before
///   any point where the process may block (stdin is read up front, so the diagnostics sites are the only such points).
///
/// Bytes are never reordered and never dropped: the compat corpus compares stderr only by prefix (`stderrparity` rows),
/// and the exit-class law lives in the return paths, not in the channel. A failed stderr write never aborts a run —
/// that is the adopted law and the sink's, and buffered writes make it even harder to trip.
struct BufferedStderr {
    writer: io::BufWriter<io::Stderr>,
    terminal: bool,
}

fn stderr_channel() -> &'static Mutex<BufferedStderr> {
    static CHANNEL: OnceLock<Mutex<BufferedStderr>> = OnceLock::new();
    CHANNEL.get_or_init(|| {
        Mutex::new(BufferedStderr {
            writer: io::BufWriter::with_capacity(64 * 1024, io::stderr()),
            terminal: io::stderr().is_terminal(),
        })
    })
}

/// Locks the buffered stderr channel, recovering a poisoned mutex.
///
/// A panic inside a request thread poisons the channel; the process must keep writing diagnostics regardless, so the
/// poison is taken over rather than propagated.
fn stderr_channel_locked() -> MutexGuard<'static, BufferedStderr> {
    stderr_channel().lock().unwrap_or_else(PoisonError::into_inner)
}

/// Writes raw bytes to the buffered stderr channel, flushing immediately when stderr is a terminal. Never fails: a
/// stderr failure must not abort a run.
fn eprint_buffered(bytes: &[u8]) {
    let mut channel = stderr_channel_locked();
    let _ = channel.writer.write_all(bytes);
    if channel.terminal {
        let _ = channel.writer.flush();
    }
}

/// Records the run's terminal failure into the diagnostic stream (when one exists), then renders the CLI failure —
/// every drive's error path leaves the failure record behind before the error escapes. Prints the retained diagnostic
/// records under `--diagnostics`: one `jqf: diag` NDJSON line per record (the SDK's own serialization, so the CLI and
/// bindings agree byte for byte), plus the terminal failure's rendered text when the run failed. A no-op when no stream
/// was retained. The retained records print only under `--diagnostics`, never under `--explain` alone, so the explain
/// surface stays JSON-free.
fn record_and_render_failure(
    print_records: bool,
    diagnostics: Option<&jqf_sdk::Diagnostics>,
    error: &jqf_sdk::Failure,
) -> CliFailure {
    if let Some(diagnostics) = diagnostics {
        if let Some(failure) = error.pipeline_failure() {
            diagnostics.record_failure(failure);
        }
        if print_records {
            print_diagnostic_records(diagnostics);
        }
    }
    crate::errors::render_failure(error)
}

/// Records which rung served the request (when a stream exists).
fn record_route(diagnostics: Option<&jqf_sdk::Diagnostics>, route: &str) {
    if let Some(diagnostics) = diagnostics {
        diagnostics.record_route_named(route);
    }
}

fn print_diagnostic_records(diagnostics: &jqf_sdk::Diagnostics) {
    let records = diagnostics.records();
    for record in &records {
        eprint_line_buffered(&format!("jqf: diag {}", jqf_sdk::record_json(record)));
    }
    // The one-line "what happened" view: per-code counts, sorted by name.
    if !records.is_empty() {
        let mut counts: Vec<(&str, u16, usize)> = Vec::new();
        for record in &records {
            if let Some(slot) = counts.iter_mut().find(|(name, _, _)| *name == record.code_name()) {
                slot.2 += 1;
            } else {
                counts.push((record.code_name(), record.code, 1));
            }
        }
        counts.sort_unstable_by_key(|(name, _, _)| *name);
        let summary = counts
            .iter()
            .map(|(name, _, count)| format!("{name}={count}"))
            .collect::<Vec<_>>()
            .join(" ");
        eprint_line_buffered(&format!("jqf: diag_counts: {summary}"));
    }
    if let Some(failure) = diagnostics.failure() {
        eprint_line_buffered(&format!(
            "jqf: failure {}: {}",
            failure.code_name(),
            jqf_sdk::render_record(&failure)
        ));
    }
    flush_stderr();
}

/// Writes one line (adding the newline) to the buffered stderr channel.
pub(crate) fn eprint_line_buffered(line: &str) {
    let mut channel = stderr_channel_locked();
    let _ = channel.writer.write_all(line.as_bytes());
    let _ = channel.writer.write_all(b"\n");
    if channel.terminal {
        let _ = channel.writer.flush();
    }
}

/// The mismatch dial's warn report (`052` W2): the adopted answer and exit code, plus a capped, aggregated line on
/// stderr after the run — per-cell counts, never one line per event. Suppressed cells (an `alternative`/`?`/`try`
/// around the site) never count. A run whose cells all fired nothing says nothing. When a diagnostic sink is installed
/// (`--diagnostics`/`--explain`), the same report rides the informational diagnostic channel as ONE
/// [`MISMATCH_WARN`](jqf_resource:diag:codes:MISMATCH_WARN) record, so bindings read exactly what the CLI prints.
fn print_mismatch_warn_report(
    resources: &jqf_resource::ResourceContext<'_>,
    diagnostics: Option<&jqf_sdk::Diagnostics>,
) {
    let counts = resources.take_mismatch_report();
    let cells = counts
        .iter()
        .enumerate()
        .filter(|(_, count)| **count > 0)
        .map(|(index, count)| {
            let name = jqf_resource::policy::MISMATCH_CELL_NAMES
                .get(index)
                .copied()
                .unwrap_or("<unknown-cell>");
            format!("{name}={count}")
        })
        .collect::<Vec<_>>();
    if cells.is_empty() {
        return;
    }
    let summary = cells.join(" ");
    eprint_line_buffered(&format!("jqf: warning: mismatch report: {summary}"));
    if let Some(diagnostics) = diagnostics {
        let mut record = jqf_resource::diag::DiagnosticRecord::new(jqf_resource::diag::codes::MISMATCH_WARN);
        record.operand = Some(&summary);
        {
            use jqf_resource::diag::DiagnosticSink as _;
            diagnostics.record(record);
        }
    }
    flush_stderr();
}

/// Flushes the buffered stderr channel. Called at process exit and after one-time diagnostics so a blocked or exiting
/// process never loses them.
pub(crate) fn flush_stderr() {
    let mut channel = stderr_channel_locked();
    let _ = channel.writer.flush();
}

/// The cross-format encode policy's warnings: one line per projection KIND that actually occurred, with its count.
/// EVENT-based, so a run that projected nothing says nothing, and NOT source-format-based — after decode unification an
/// encoder cannot know where a value came from.
///
/// Printed at the one place every run has finished and the request context is still alive — success AND failure alike
/// (a run that projected 10k temporals and then died still owes the lossy report; the ledger saw every event).
/// Unconditional, unlike the `--diagnostics` counters beside it, because a projection is LOSSY — and promoting exactly
/// these to errors is all a future `--strict-encode` has to do.
fn print_projection_warnings(resources: &jqf_resource::ResourceContext<'_>) {
    for (kind, count) in resources.projection_event_summary() {
        // The warning names the escape hatch: `--types-as-strings` reads every extended kind (temporals, bytes) as its
        // plain text instead.
        let hint = if kind == ProjectionKind::Temporal {
            "; --types-as-strings reads temporals as their plain text"
        } else {
            ""
        };
        eprint_line_buffered(&format!("jqf: warning: {count} {}{hint}", kind.summary(count)));
        // A projection loss is a promotable warning under `--strictness strict` (the `--strict-encode` the comment
        // above names).
        resources.note_strictness_warnings(1);
    }
}

/// A parse-class compile error under `--explain` prints the recovered-tree outline before the ordinary compile-error
/// stderr. Other compile classes (unsupported construct, undefined call) have no recovery tree to show. The
/// explain-surface law stays JSON-free: no diag stream, no counters, no cost line — there was no run.
fn compile_failure_maybe_explain(error: &EngineCompileError, source: &str, explain: bool) -> CliFailure {
    if explain && matches!(error, EngineCompileError::Parse(_)) {
        for line in explain::render_parse_failure(source) {
            eprint_line_buffered(&line);
        }
        flush_stderr();
    }
    compile_failure(error, source)
}

/// Writes a per-value error line to the buffered stderr channel without building a `String` or going through `fmt` —
/// the hot path for error-heavy streams, where the constant prefix is the same every time.
fn eprint_value_error(input_line: u64, frame_note: &str, message: &str) {
    eprint_value_error_at("<stdin>", input_line, frame_note, message);
}

/// Writes a per-value error line naming a source OTHER than `<stdin>` — the serve daemon's per-connection label — in
/// the same `jqf: error (at <source>:N)frame: message` shape the ordinary sink renders, so a serve session's stderr
/// lines parse exactly like a run's.
pub(crate) fn eprint_value_error_at(source: &str, input_line: u64, frame_note: &str, message: &str) {
    let mut channel = stderr_channel_locked();
    let _ = channel.writer.write_all(b"jqf: error (at ");
    if input_line == jqf_sdk::UNKNOWN_INPUT_LINE {
        // UNKNOWN location for a single-run error that never touched the input (`-n '1|.b'`): `(at <unknown>)`, no
        // file:line.
        let _ = channel.writer.write_all(b"<unknown>");
    } else {
        let _ = channel.writer.write_all(source.as_bytes());
        let _ = channel.writer.write_all(b":");
        let _ = write_decimal(&mut channel.writer, input_line);
    }
    let _ = channel.writer.write_all(b")");
    let _ = channel.writer.write_all(frame_note.as_bytes());
    let _ = channel.writer.write_all(b": ");
    let _ = channel.writer.write_all(message.as_bytes());
    let _ = channel.writer.write_all(b"\n");
    if channel.terminal {
        let _ = channel.writer.flush();
    }
}

/// Writes a record-issue line (the recovering dialect's ordered issues).
fn eprint_record_issue(severity: &str, ordinal: u64, offset: u64, code: &str, detail: &str) {
    let mut channel = stderr_channel_locked();
    let _ = channel.writer.write_all(b"jqf: record ");
    let _ = channel.writer.write_all(severity.as_bytes());
    let _ = channel.writer.write_all(b" (record ");
    let _ = write_decimal(&mut channel.writer, ordinal);
    let _ = channel.writer.write_all(b", byte ");
    let _ = write_decimal(&mut channel.writer, offset);
    let _ = channel.writer.write_all(b"): ");
    let _ = channel.writer.write_all(code.as_bytes());
    let _ = channel.writer.write_all(b": ");
    let _ = channel.writer.write_all(detail.as_bytes());
    let _ = channel.writer.write_all(b"\n");
    if channel.terminal {
        let _ = channel.writer.flush();
    }
}

/// Writes `value` in decimal to `out`, returning nothing (errors are the channel's problem, never the caller's).
fn write_decimal<W: IoWrite>(out: &mut W, value: u64) -> io::Result<()> {
    let mut digits = [0u8; 20];
    let mut remaining = value;
    let mut index = digits.len();
    loop {
        index -= 1;
        digits[index] = b'0' + (remaining % 10) as u8;
        remaining /= 10;
        if remaining == 0 {
            break;
        }
    }
    out.write_all(&digits[index..])
}
/// Container-span frontier selection for the on-demand campaign's S2 cutover.
///
/// The shipped default is LAZY for programs that do not consume the whole document (`JQF_CONTAINER_SPANS` unset): the
/// decoder defers container subtrees below the root to validated source spans, materialized only when something touches
/// them. Unbounded count and element-iteration rows (`PATH | length`, `[C[] | .id]`) default EAGER so the prune hint is
/// armed. Bounded prefixes (`limit`/`first`/`nth`) stay lazy. Identity, `..`, and whole-document `|=` stay eager via
/// [`CompiledProgram:consumes_whole_document`].
///
/// The environment variable remains the explicit override, because the standing force-lazy differential must run BOTH
/// arms from one binary: `JQF_CONTAINER_SPANS=0` forces eager, a positive value forces the frame depth. It is
/// deliberately undocumented in `--help`.
///
/// The switch is set through `jqf-codec-core`, not through a codec: deferral policy is format-neutral, so this stays
/// correct for any format that grows the capability without the CLI learning a second spelling.
const CONTAINER_SPAN_FRONTIER_VAR: &str = "JQF_CONTAINER_SPANS";

/// Path a run writes its container-span counters to, if set.
///
/// A file rather than a stream: the differential compares stdout and stderr byte for byte, so instrumentation must not
/// touch either. One line, two fields: the spans published, and the decodes that asked for deferral and DECLINED it.
/// The second is what tells a reader whether the mechanism generalizes over real documents or only over number-clean
/// ones.
const CONTAINER_SPAN_TRACE_VAR: &str = "JQF_CONTAINER_SPANS_TRACE";

/// The explicit container-span frontier override from the environment, when set. The override travels through the
/// request's `CodecRequirementPolicy` into the lowered access requirement (the engine's root lowering reads it), so no
/// process-global knob exists: when unset, the engine decides from the program's consumption class (the S2 route
/// decision) — depth 1 (lazy) for programs that provably do not consume the whole document, depth 0 (eager) for
/// everything else.
fn container_span_frontier_override() -> Result<Option<u32>, String> {
    let Some(value) = env::var_os(CONTAINER_SPAN_FRONTIER_VAR) else {
        return Ok(None);
    };
    let text = value
        .to_str()
        .ok_or_else(|| format!("{CONTAINER_SPAN_FRONTIER_VAR} is not valid UTF-8"))?;
    let depth: u32 = text
        .trim()
        .parse()
        .map_err(|_| format!("{CONTAINER_SPAN_FRONTIER_VAR}: expected a frontier depth, got {text:?}"))?;
    Ok(Some(depth))
}

/// Writes the run's committed container-span count where the harness asked.
fn write_container_span_trace() {
    let Some(path) = env::var_os(CONTAINER_SPAN_TRACE_VAR) else {
        return;
    };
    let _ = fs::write(
        path,
        format!(
            "{} {}\n",
            jqf_codec_core::committed_container_spans(),
            jqf_codec_core::declined_deferrals(),
        ),
    );
}

/// jq dies silently on SIGPIPE with 141; Rust's std routes SIGPIPE to `SIG_IGN`, which turns a broken pipe into an
/// `EPIPE` write error and a noisy `stdout failed: Broken pipe` diagnostic, exit 2. Restore SIGPIPE to its process
/// default so `jqf … | head` terminates exactly as quietly as jq. Harmless elsewhere: SIGPIPE only fires when writing
/// to a pipe whose reader has closed, never for a file destination.
#[cfg(unix)]
fn restore_sigpipe_default() {
    // Safety: `signal(SIGPIPE, SIG_DFL)` is async-signal-safe and touches no
    // Rust state; it restores the process disposition std replaced with SIG_IGN at startup.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

/// Non-Unix platforms have no SIGPIPE; the disposition is left untouched.
#[cfg(not(unix))]
fn restore_sigpipe_default() {}

/// The async-signal-safe half of the spill store's lifecycle.
///
/// The store unlinks every run file at creation, so a signal death leaves at most an empty `0700` directory. These
/// handlers remove it — one `rmdir` on a pre-rendered NUL-terminated path, then a re-raise under the default
/// disposition so the process dies with the signal's own wait status (Ctrl-C stays 130, a broken pipe stays the quiet
/// 141 jq gives). `Drop` cannot do this job: no signal runs it.
///
/// The handlers are installed ONLY when a spill store exists (`--max-spill-bytes N`), so the SIGPIPE default-restore
/// above stays the only disposition on the ordinary path, and `jqf … | head` keeps dying exactly as quietly as before.
#[cfg(unix)]
mod spill_cleanup {
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

    /// The NUL-terminated spill directory path, or null when no store exists. Set once at store construction, read only
    /// from the handler. The `CString` is leaked deliberately: the handler must not allocate, and a few dozen bytes per
    /// process is the price of a cleanup path that cannot fail.
    static SPILL_DIR: AtomicPtr<libc::c_char> = AtomicPtr::new(std::ptr::null_mut());

    /// Whether the handlers are installed — shared by the install latch and [`unregister`]'s no-op guard, so a
    /// stand-down without an install is a no-op rather than a clobber of whatever disposition preceded jqf.
    static INSTALLED: AtomicBool = AtomicBool::new(false);

    /// Registers `dir` for signal cleanup and installs the handlers. Called once, right after the spill store is
    /// constructed; the directory may not exist yet (the store creates it lazily at the first run), which is fine — a
    /// handler `rmdir` of a missing path is a no-op.
    pub(crate) fn register(dir: &Path) {
        // Unix paths cannot contain NUL; if one ever did, skip cleanup rather than register a truncated path.
        let Ok(cstring) = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()) else {
            return;
        };
        let ptr = Box::into_raw(cstring.into_boxed_c_str()).cast::<libc::c_char>();
        SPILL_DIR.store(ptr, Ordering::Release);
        install_handlers_once();
    }

    /// Stands the handlers down once the store is gone: the directory no longer exists, so a signal after this point
    /// has nothing to clean and the default disposition serves it exactly as before any store was installed (the quiet
    /// SIGPIPE 141 included). Idempotent, and a no-op when nothing is registered.
    pub(crate) fn unregister() {
        if !INSTALLED.load(Ordering::SeqCst) {
            return;
        }
        SPILL_DIR.store(std::ptr::null_mut(), Ordering::Release);
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGPIPE] {
            unsafe {
                libc::signal(signal, libc::SIG_DFL);
            }
        }
    }

    fn install_handlers_once() {
        if INSTALLED.swap(true, Ordering::SeqCst) {
            return;
        }
        // The three deaths Drop cannot see. SIGTERM is free — the same rmdir-and-re-raise serves a kill from a job
        // scheduler or a `kill`.
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGPIPE] {
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = cleanup_and_reralse as *const () as libc::sighandler_t;
                libc::sigemptyset(&raw mut action.sa_mask);
                libc::sigaction(signal, &raw const action, std::ptr::null_mut());
            }
        }
    }

    extern "C" fn cleanup_and_reralse(signal: libc::c_int) {
        // Async-signal-safe by construction: one rmdir on a path guaranteed to be EMPTY (the store unlinks every run
        // file at creation, so only the empty directory can survive), no allocation, no locks, no output.
        let ptr = SPILL_DIR.load(Ordering::Acquire);
        if !ptr.is_null() {
            unsafe {
                libc::rmdir(ptr);
            }
        }
        // Restore the default disposition and re-raise so the process dies with the caller's expected signal (and thus
        // exit status) instead of silently surviving on a handled signal.
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
            libc::raise(signal);
        }
    }
}

/// The cost snapshot the diagnostics and `--explain` blocks report.
///
/// One ledger: the request account, shared into ambient when the CLI is metering. Heap charges and grant holds are
/// already on `resources`.
pub(crate) fn reported_cost_snapshot(resources: &jqf_resource::ResourceContext<'_>) -> jqf_resource::UsageSnapshot {
    resources.snapshot()
}

fn main() -> ExitCode {
    restore_sigpipe_default();
    let code = main_run();
    // Everything the run wrote to stderr — per-value errors, record issues, diagnostics, halt payloads — leaves the
    // process here, on every exit path.
    flush_stderr();
    write_container_span_trace();
    code
}

/// The request thread's stack size. One parser, shared with the FFI handle: see [`jqf_sdk:request_stack_bytes`].
fn request_stack_bytes() -> Result<usize, String> {
    jqf_sdk::request_stack_bytes()
}

/// Stands the spill signal handlers down when the request that installed them ends. Declared at the run function's
/// top-level scope so the guard type lives with the other request-scoped state.
#[cfg(unix)]
struct SpillSignalGuard(bool);
#[cfg(unix)]
impl Drop for SpillSignalGuard {
    fn drop(&mut self) {
        if self.0 {
            spill_cleanup::unregister();
        }
    }
}

fn main_run() -> ExitCode {
    // Parse argv on the MAIN thread before validating 23 codec registrations or reserving the 256 MiB request stack.
    // `--help`/`-V` and a usage error must not pay that cold start. Filename detection and edit-capability lookup need
    // the catalog and wait until the request thread (or the discovery path below) builds it.
    match parse_arguments(None) {
        Ok(CliCommand::Help) => {
            return exit_from(write_stdout(help_text()));
        }
        Ok(CliCommand::ShortHelp) => {
            return exit_from(write_stdout(short_help_text()));
        }
        Ok(CliCommand::Version) => {
            return exit_from(write_stdout(format!("jqf-{}\n", env!("CARGO_PKG_VERSION"))));
        }
        Ok(CliCommand::BuildConfiguration) => {
            return exit_from(write_stdout(format!("{}\n", provenance::BuildProvenance)));
        }
        Ok(CliCommand::ListBuiltins) => {
            return exit_from(write_discovery(&crate::discovery::list_builtins()).map(|()| 0));
        }
        Ok(CliCommand::ExplainCode(id)) => {
            return exit_from((|| {
                let page = crate::discovery::explain_code(id)?;
                write_discovery(&page)?;
                Ok(0)
            })());
        }
        Ok(CliCommand::ShowConfig(report)) => {
            return exit_from(write_stdout(report));
        }
        Ok(CliCommand::ListFormats) => {
            return exit_from(with_builtin_catalog(|catalog| {
                write_discovery(&crate::discovery::list_formats(catalog)).map(|()| 0)
            }));
        }
        Ok(CliCommand::HelpFormat(format)) => {
            return exit_from(with_builtin_catalog(|catalog| {
                write_discovery(&crate::discovery::help_format(format, catalog)).map(|()| 0)
            }));
        }
        Ok(CliCommand::HelpTopic(topic)) => {
            return exit_from(with_builtin_catalog(|catalog| {
                write_discovery(&crate::discovery::help_topic(topic, catalog)?).map(|()| 0)
            }));
        }
        Ok(CliCommand::Serve(args)) => {
            return spawn_request(move || routes::serve::run_daemon(args));
        }
        Ok(CliCommand::Run(_)) => {}
        Err(failure) => return exit_from(Err(failure)),
    }

    // The whole request runs on a big-stack thread: parse and lowering recurse over the PROGRAM tree on the call stack,
    // and the default main stack overflowed at ~700 nested constructors where jq parses thousands (ledger item 27). The
    // reservation is virtual address space, not committed memory: pages are touched only as the recursion actually uses
    // them. The request's arena types are deliberately not `Send`, so only the request's RESULT crosses the join
    // boundary.
    //
    // The stack is the SECOND line of defence, not the first. The original sizing argued a program could only arrive as
    // an argument (128-256 KiB of argv, so ~100k levels at worst) — but `-f file` has no such cap, and the measured
    // cost of one nested constructor is ~15 KiB of debug frames, so a 50,000-level program aborted this thread outright
    // (probe sweep 037 finding 1). The bound that makes deep program text safe is
    // `jqf_syntax:MAX_SYNTAX_NESTING_DEPTH`, refusing past 10,000 levels in the parser and again in the lowering walk;
    // this stack is sized to hold that ceiling with room to spare in an unoptimized build, and the stack-depth gate's
    // `program-nesting` lane keeps both halves honest.
    spawn_request(run)
}

fn write_stdout(text: impl AsRef<[u8]>) -> Result<u8, CliFailure> {
    io::stdout()
        .lock()
        .write_all(text.as_ref())
        .map_err(|error| format!("cannot write: {error}").into())
        .map(|()| 0)
}

fn spawn_request<F>(work: F) -> ExitCode
where
    F: FnOnce() -> Result<u8, CliFailure> + Send + 'static,
{
    let stack_bytes = match request_stack_bytes() {
        Ok(bytes) => bytes,
        Err(message) => {
            eprint_line_buffered(&format!("jqf: {message}"));
            flush_stderr();
            return ExitCode::from(2);
        }
    };
    let outcome = match std::thread::Builder::new()
        .stack_size(stack_bytes)
        .spawn(work)
        .map_err(|error| format!("cannot start the request thread: {error}"))
    {
        Ok(handle) => handle.join(),
        Err(message) => {
            eprint_line_buffered(&format!("jqf: {message}"));
            flush_stderr();
            return ExitCode::from(2);
        }
    };
    // A panic inside the request is an internal error, not a program rejection; the process must not pretend otherwise.
    // The line goes through the buffered channel, not a bare stderr write: per-value errors may still sit unflushed in
    // the buffer on a pipe, and a bare write here would land BEFORE them.
    let Ok(outcome) = outcome else {
        eprint_line_buffered("jqf: internal error: the request thread panicked");
        flush_stderr();
        return ExitCode::from(2);
    };
    // The W1 report prints after the request thread joined, on the success AND failure paths, so a run that died on its
    // input still says what its physical footprint was. Gated on `--diagnostics`; `--explain` alone stays counter-free
    // (the explain-surface law).
    rss::print_report_if_enabled();
    exit_from(outcome)
}

fn exit_from(outcome: Result<u8, CliFailure>) -> ExitCode {
    match outcome {
        Ok(code) => ExitCode::from(if input::missing_positional_file() { 2 } else { code }),
        Err(failure) => {
            // `halt`/`halt_error`: the message is written RAW to stderr (no `jqf:` prefix — that is the adopted
            // `halt_error` law), then the process exits with the requested code.
            if let CliFailure::Halt { message, .. } = &failure {
                if let Some(message) = message {
                    // jq writes the value compact with NO trailing newline (`halt_error` stderr is exactly the value).
                    eprint_buffered(message.as_bytes());
                }
                flush_stderr();
                // item 2: a missing positional file forces exit 2 even over halt's own code (`jq 'halt' missing` and
                // `jq 'halt_error(7)' missing` both exit 2).
                return ExitCode::from(if input::missing_positional_file() {
                    2
                } else {
                    failure.exit_code()
                });
            }
            // A per-value runtime error was already reported to stderr as the sequence continued past it; carry only
            // its exit class.
            if !matches!(failure, CliFailure::Reported) {
                eprint_line_buffered(&format!("jqf: {failure}"));
            }
            flush_stderr();
            // item 2: a missing positional file forces the adopted exit 2 over the run's own class — EXCEPT a compile
            // error's 3, which jq decides before any file is opened (compile error + missing file exits 3).
            let code = failure.exit_code();
            ExitCode::from(if input::missing_positional_file() && code != 3 {
                2
            } else {
                code
            })
        }
    }
}

/// Writes one discovery-surface page to stdout. The four list/explain commands share this shape — generate, write, exit
/// 0, no stdin read — so the write failure path lives here once.
fn write_discovery(page: &str) -> Result<(), CliFailure> {
    io::stdout()
        .lock()
        .write_all(page.as_bytes())
        .map_err(|error| format!("cannot write: {error}").into())
}

/// The published-item facts the driver extracts from the sink after a run: the item count, the last value's truthiness
/// (`-e`), and the last value's empty-array verdict (`--diff`'s equality law). One type so the tuple stays nameable
/// past clippy's `type_complexity` floor.
type ExitFacts = (u64, Option<bool>, Option<bool>);

/// Captures the ambient ledger's snapshot on the REQUEST thread — where the ambient scope is live — and stashes it for
/// the main thread's `--diagnostics` report, which prints after the request thread joins and the thread-local scope no
/// longer exists. The inner scope guard must drop AFTER the capture, so the snapshot is read while the account is still
/// installed.
struct AmbientLedger(Option<jqf_resource::ScopeGuard>);

impl Drop for AmbientLedger {
    fn drop(&mut self) {
        let (peak, current) = jqf_resource::with_account(jqf_resource::RequestAccount::snapshot)
            .map_or((0, 0), |snapshot| {
                (snapshot.memory_peak_bytes(), snapshot.memory_current_bytes())
            });
        rss::set_ambient(peak, current);
        drop(self.0.take());
    }
}

/// The success exit code `run` reports: 0 ordinarily, or the adopted `-e` exit law (0 truthy last value, 1 false/null,
/// 4 no output) when the flag is set. A failure still travels as [`CliFailure`], which carries its own exit class.
#[allow(clippy::similar_names, reason = "one declaration per codec through one seam")]
fn with_builtin_catalog<T>(f: impl FnOnce(CodecCatalog<'_, '_>) -> Result<T, CliFailure>) -> Result<T, CliFailure> {
    // The codec catalog is the CLI's route-fact source: the discovery surface, the route planner, and 's
    // extension-based input-format detection read each registration's declarations through it, so the input-model,
    // record, edit, and extension facts are never re-declared as `match` arms. Validated only after argv parse, and
    // only when the command needs it. The parallel per-format bindings below are deliberately similar names
    // (jsonc_registration, json5_registration,
    //...): one declaration per codec through one seam.
    let registration =
        jqf_codec_json::registration().map_err(|error| format!("invalid built-in JSON registration: {error:?}"))?;
    let jsonc_registration = jqf_codec_json::jsonc::registration()
        .map_err(|error| format!("invalid built-in JSONC registration: {error:?}"))?;
    let json5_registration = jqf_codec_json::json5::registration()
        .map_err(|error| format!("invalid built-in JSON5 registration: {error:?}"))?;
    let streams_registration = jqf_codec_json::ndjson::registration()
        .map_err(|error| format!("invalid built-in NDJSON registration: {error:?}"))?;
    let json_seq_registration = jqf_codec_json::seq::registration()
        .map_err(|error| format!("invalid built-in json-seq registration: {error:?}"))?;
    let toml_registration = jqf_codec_toml::registration_1_0()
        .map_err(|error| format!("invalid built-in TOML 1.0 registration: {error:?}"))?;
    let toml_11_registration = jqf_codec_toml::registration_1_1()
        .map_err(|error| format!("invalid built-in TOML 1.1 registration: {error:?}"))?;
    let csv_registration =
        jqf_codec_delimited::registration().map_err(|error| format!("invalid built-in CSV registration: {error:?}"))?;
    let tsv_registration = jqf_codec_delimited::registration_tsv()
        .map_err(|error| format!("invalid built-in TSV registration: {error:?}"))?;
    let cbor_registration =
        jqf_codec_cbor::registration().map_err(|error| format!("invalid built-in CBOR registration: {error:?}"))?;
    let cbor_seq_registration = jqf_codec_cbor::seq::registration()
        .map_err(|error| format!("invalid built-in cbor-seq registration: {error:?}"))?;
    // One registration carries every YAML dialect (123 X5, D4): the pre-123 three-registration contortion with disjoint
    // dialect pairs is gone.
    let yaml_registration =
        jqf_codec_yaml::registration().map_err(|error| format!("invalid built-in YAML registration: {error:?}"))?;
    let jqft_registration = jqf_codec_jqft::registration_jqft()
        .map_err(|error| format!("invalid built-in jqft registration: {error:?}"))?;
    let jqfjson_registration = jqf_codec_jqft::registration_jqfjson()
        .map_err(|error| format!("invalid built-in jqfjson registration: {error:?}"))?;
    let jqfb_image_registration = jqf_codec_jqft::registration_jqfb()
        .map_err(|error| format!("invalid built-in jqfb registration: {error:?}"))?;
    let xml_registration =
        jqf_codec_xml::registration().map_err(|error| format!("invalid built-in XML registration: {error:?}"))?;
    let html_registration =
        jqf_codec_html::registration().map_err(|error| format!("invalid built-in HTML registration: {error:?}"))?;
    let html_fragment_registration = jqf_codec_html::registration_fragment()
        .map_err(|error| format!("invalid built-in HTML fragment registration: {error:?}"))?;
    let properties_registration = jqf_codec_ini::registration()
        .map_err(|error| format!("invalid built-in properties registration: {error:?}"))?;
    let ini_registration =
        jqf_codec_ini::registration_ini().map_err(|error| format!("invalid built-in ini registration: {error:?}"))?;
    let dotenv_registration = jqf_codec_ini::registration_dotenv()
        .map_err(|error| format!("invalid built-in dotenv registration: {error:?}"))?;
    let messagepack_registration = jqf_codec_messagepack::registration()
        .map_err(|error| format!("invalid built-in MessagePack registration: {error:?}"))?;
    let render_registration =
        jqf_codec_render::registration().map_err(|error| format!("invalid built-in render registration: {error:?}"))?;
    let registrations = [
        &registration,
        &jsonc_registration,
        &json5_registration,
        &streams_registration,
        &json_seq_registration,
        &toml_registration,
        &toml_11_registration,
        &csv_registration,
        &tsv_registration,
        &cbor_registration,
        &cbor_seq_registration,
        &yaml_registration,
        &jqft_registration,
        &jqfjson_registration,
        &jqfb_image_registration,
        &xml_registration,
        &html_registration,
        &html_fragment_registration,
        &properties_registration,
        &ini_registration,
        &dotenv_registration,
        &messagepack_registration,
        &render_registration,
    ];
    let index = jqf_sdk::CatalogIndex::build(&registrations);
    let catalog = CodecCatalog::new(&registrations).with_index(&index);
    f(catalog)
}

fn run() -> Result<u8, CliFailure> {
    with_builtin_catalog(run_with_catalog)
}

#[allow(
    clippy::too_many_lines,
    reason = "one linear invocation: argument parsing, resource/account setup, compile, then the \
              whole-document and located routes with their materializing sequence fallback — \
              splitting it would thread the same dozen locals through helpers"
)]
fn run_with_catalog(catalog: CodecCatalog<'_, '_>) -> Result<u8, CliFailure> {
    // The catalog-less pre-pass in `main_run` dispatched every non-Run command and returned before this thread spawned,
    // so only a run request gets here. This second parse's job is exactly the catalog-dependent work that pass could
    // not do: file-name format detection and edit-capability lookup.
    let CliCommand::Run(arguments) = parse_arguments(Some(catalog))? else {
        unreachable!("non-run commands were dispatched before the request thread spawned");
    };
    let CliArguments {
        mut program,
        program_file,
        null_input,
        raw_input,
        slurp,
        // `-r` is validated and resolved into `output_selection.raw_strings` (with `-j` implying it) inside argument
        // parsing; the CLI body reads the selection, never the raw flag.
        bindings,
        max_memory_bytes,
        max_spill_bytes,
        max_spill_disk_bytes,
        max_iterations,
        max_rss,
        library_paths,
        input: input_selection,
        output: output_selection,
        parallel: parallel_selection,
        mismatch_policy,
        strictness,
        json_facts,
        types_as_strings,
        diagnostics,
        explain,
        exit_status,
        seed,
        unbuffered,
        colour,
        stream,
        stream_errors,
        follow,
        seq_recovering,
        positional_args,
        plan_out,
        plan_file,
        edit,
        edit_check,
        edit_expand_alias,
        output_path,
        in_place,
        input_files,
        no_atomic,
        diff_pair,
        diff_old_selection,
        diff_new_selection,
        schema_file,
        split_exp,
        split_exp_file,
        config_source,
    } = arguments;
    let host_bindings = !bindings.is_empty() || !library_paths.is_empty() || !positional_args.is_empty();
    // JQ_COLORS: parsed once per request, exactly as jq's the palette parser parses it at startup. A malformed value
    // falls back to the defaults and says so on stderr, UNCONDITIONALLY — jq prints the message even when colour never
    // engages (`-M` and piped runs both print it) and before every other diagnostic (it precedes a compile error). The
    // palette only renders when the decision below engages.
    let palette = if let Ok(palette) = colour::parse_jq_colors(std::env::var_os("JQ_COLORS").as_deref()) {
        palette
    } else {
        eprint_line_buffered("Failed to set $JQ_COLORS");
        flush_stderr();
        colour::Palette::default_palette()
    };
    // The RSS governor arms BEFORE any input is read: its check points sit at the request's own decision points (after
    // the input read, after compile, before each route attempt) and its `Control` rides the ledger's decision points
    // during every drive. The default is 80% of the detected effective memory; `--max-rss 0` disables it
    // (measure-only). The provenance line prints BEFORE the governor arms, so a bad `--max-rss` value dies at setup
    // WITH the line that says which binary died — under `--diagnostics` a setup failure is attributable like any other.
    if diagnostics || explain {
        // Printed BEFORE stdin is read, so a request that dies on its input still says which binary died: the build
        // path and the allocator are the two things that change wall clock without changing output.
        eprint_line_buffered(&format!("jqf: {}", provenance::BuildProvenance));
        // The config file(s) whose values were merged, when one was read (never under --no-config / JQF_NO_CONFIG). The
        // provenance line names the file so "why is it doing that?" has an answer.
        if let Some(source) = &config_source {
            eprint_line_buffered(&format!("jqf: config file(s): {source}"));
        }
        flush_stderr();
    }
    rss::configure(
        max_rss.unwrap_or(rss::MaxRss::Percent(rss::DEFAULT_CEILING_PERCENT)),
        max_rss.is_some(),
    )?;
    rss::set_report_enabled(diagnostics);
    // The input's diagnostics label: the first positional file when files were named, `<stdin>` otherwise. Multi-file
    // requests name each file in their own per-value `input_filename` provenance; this is the whole-source face used by
    // codec diagnostics and the single-source fallback.
    let input_file_labels: Vec<String> = input_files
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    let input_label = input_file_labels
        .first()
        .cloned()
        .unwrap_or_else(|| "<stdin>".to_owned());
    // jq processes its ARGUMENTS before its input: the `--slurpfile`/ `--rawfile` binding files are read (and must
    // open) before stdin is touched. Only the bytes are read here — value construction needs the request ledger, which
    // does not exist yet. The eager stdin read then happens BEFORE the account is created, so a request that fails at
    // setup has already consumed its input (the max-memory test's law), and the adopted `-n` additionally defers the
    // stdin read until after the compile, because the decision to read at all depends on whether the program calls the
    // input family: a program that never does reads NO input (exactly as the adopted `-n` never parses stdin for it —
    // no bootstrap limit, no parse error), while an input-using program still needs the stream for `input`/`inputs`.
    // `--in-place` defers to its per-file loop, which reads each file just before editing it.
    let mut binding_files: Vec<Option<Vec<u8>>> = Vec::with_capacity(bindings.len());
    for binding in &bindings {
        let bytes = match binding {
            CliBinding::SlurpFile(name, file) => Some(read_binding_file("--slurpfile", name, file)?),
            CliBinding::RawFile(name, file) => Some(read_binding_file("--rawfile", name, file)?),
            CliBinding::Arg(..) | CliBinding::ArgJson(..) => None,
        };
        binding_files.push(bytes);
    }
    // The `--schema` file is read with the binding files — before stdin is touched, exactly like the adopted
    // argument-file precedence. Only the bytes are read here; value construction (one strict JSON document) needs the
    // request ledger and happens beside the bindings.
    let schema_bytes: Option<Vec<u8>> = match &schema_file {
        Some(path) => Some(std::fs::read(path).map_err(|error| CliFailure::Message {
            class: ExitClass::Usage,
            message: format!("Could not open {}: {}", path.display(), io_error_text(&error)),
        })?),
        None => None,
    };
    // `--diff` reads its two files itself; stdin is never touched (the null-input model). `--follow` never reads the
    // input WHOLE: its route tails a growing file or pipe incrementally, so an eager read here would defeat the mode
    // before it started. `-n` defers (below) until the compile decides whether the program reads the input family;
    // `--in-place` reads each file inside its own per-file run. The W4 seekability decision: a NON-SEEKABLE stdin
    // (pipe/FIFO/socket) with a record-route input publishes per value instead of reading whole. The predicate must
    // agree with `plan:resolve`'s Route:Stream guards — a drift would eagerly consume the pipe the stream route was
    // about to read. `-s`/`-R` read to completion, the edit/diff lanes own their own input models, and `--follow` is
    // the flag-scoped live route; none of them streams by default. When no file is named, stdin is the source and the
    // descriptor itself decides (a regular file redirect stays seekable and keeps every whole-read fast route). `-n` is
    // NOT excluded here: `plan:resolve`'s per-record arm re-guards it, and the streaming input-family lane (below,
    // after compile) is exactly the `-n inputs` shape jq answers on demand.
    let stream_stdin = !follow
        && !edit
        && input_files.is_empty()
        && !slurp
        && !raw_input
        && diff_pair.is_none()
        // `--stream`: a JSON input on a pipe reaches the streaming path — the incremental event drive feeds the events
        // as they arrive, which is the flag's bounded promise. A NON-JSON `--stream` request keeps the whole-read
        // materializing `tostream` rewrite (diagnosed at its rewrite site), so it stays out of the streaming path
        // exactly as before. The seekability rule is untouched for every other request.
        && !(stream && input_selection.format != CliFormat::Json)
        && input_selection.dialect.has_record_route()
        && input::stdin_is_streaming_source();
    // The bounded `--stream` event route's facts (/080), computed HERE because the eager-read decision below needs
    // them: `--stream` over a JSON input (without `-R`) is served by the event route, and plain `--stream` (no
    // `-n`/`-s`, at most one file) by the INCREMENTAL drive — the source itself is read in the adopted fgets-shaped
    // chunks, so a document larger than memory streams with only the path stack resident (the adopted `--stream` parses
    // incrementally regardless of seekability; jqf previously read the source WHOLE into the retained buffer first, so
    // the bounded promise failed exactly where jq succeeds — a 224 MB document peaked at ~318 MB RSS vs the adopted ~3
    // MB). `-n`/`-s` keep the whole-input event route: their models collect events, and a streamed collection is a
    // future item (the corpus pins their output bytes, which the whole-input drive already matches). ponytail: `-n
    // --stream` still reads whole (jq streams it); stream the null-first event drive when the collection-shaped models
    // need it.
    let stream_events_requested = stream && !raw_input && input_selection.format == CliFormat::Json;
    let stream_events_incremental = stream_events_requested && !null_input && !slurp && input_files.len() <= 1;
    // Limits are computed once, before the eager read, and the same value builds the request account that (when
    // installed) is also the ambient accountant. The install happens before the read so the read's buffer growth is
    // governed by the same ceiling the request runs under.
    let limits = request_limits(max_memory_bytes, max_spill_bytes, max_spill_disk_bytes);
    // The probe switch (not a product path) prices the ledger's share by switching every memory
    // admission/commit/release off. Applied to the limits before the account is built, so the one ledger the request
    // and the ambient slot share is already probing.
    let probe = std::env::var_os("JQF_PROBE_UNACCOUNTED").is_some();
    let limits = if probe { limits.with_probe_unaccounted() } else { limits };
    // The ambient install's predicate: the counting allocator exists to ENFORCE a ceiling and to REPORT a peak. A run
    // that sets neither pays nothing for it. The default memory ceiling is infinite, so with no memory ceiling, no
    // spill ceiling, and no `--diagnostics`/`--explain` report, charging every allocation would meter a ceiling that
    // can never refuse. A finite `--max-memory-bytes`, any spill ceiling, and `--diagnostics`/`--explain` force the
    // install. The probe forces it too: its contract is "install the ledger, switch its accounting off" — a probe that
    // skipped the install would measure a world without the ledger. `--max-rss` never reads the ledger. Parallel does
    // not force the install: a worker thread installs its own child account, and the request thread with an infinite
    // ceiling has no trip to latch. Default-parallel RSS is the `--max-rss` governor, not the ledger; installing the
    // parent ambient on every NDJSON identity would meter a ceiling that cannot refuse. A finite `--max-memory-bytes`
    // already forces the install.
    let ambient_needed = max_memory_bytes != DEFAULT_MAX_MEMORY_BYTES
        || max_spill_bytes > 0
        || max_spill_disk_bytes > 0
        || diagnostics
        || explain
        || probe;
    // One ledger: the request account is created here and, when the predicate is true, a shared handle is installed as
    // the thread's ambient accountant. Heap charges and grant holds then land on the same cells, so a cost snapshot
    // does not have to splice two accounts. Release clamps a free that was never charged (an allocation made before the
    // scope, or freed after it), which is what makes sharing safe. When the predicate is false the scope guard is
    // `None`.
    let account = RequestAccount::try_new(limits)
        .map_err(|error| format!("error: cannot create request account: {}", resource_note(error)))?;
    let _ambient_ledger = AmbientLedger(if ambient_needed {
        Some(jqf_resource::install(account.try_share().map_err(|error| {
            format!(
                "error: cannot share request account for ambient ledger: {}",
                resource_note(error)
            )
        })?))
    } else {
        None
    });
    // The eager stdin read: `--follow`/`--diff`/`--in-place` never read whole, and the STREAM route reads stdin itself,
    // incrementally — an eager read here would defeat it before it started. `-n` still defers to the compile-time
    // input-family decision below.
    let eager_input: Option<(Vec<u8>, Vec<jqf_source::SourceFileRange<'_>>)> =
        if null_input || diff_pair.is_some() || follow || in_place || stream_stdin || stream_events_incremental {
            None
        } else {
            Some(read_combined_input(&input_files, &input_file_labels)?)
        };
    // The retained-input split (§4): the eager read and the binding files are the bytes every route borrows, charged to
    // `max_input_bytes` — the term that made the accounted-vs-RSS ratios 122×–1004×. The governor attributes them so a
    // refusal and the report can say which piece fired.
    let mut retained_input: u64 = eager_input.as_ref().map_or(0, |(bytes, _)| bytes.len() as u64)
        + binding_files
            .iter()
            .flatten()
            .fold(0, |total, bytes| total + bytes.len() as u64)
        + schema_bytes.as_ref().map_or(0, Vec::len) as u64;
    rss::set_retained_input(retained_input);
    // The first CLI decision point: a request whose retained input alone exceeds the ceiling refuses before any program
    // runs (the giant-input case — refusing here is the pillar, not a compat hazard, because the machine could not have
    // held the run anyway). The follow route is exempt, as at the later decision points: it never reads whole, and its
    // per-cycle refusal law (D3) owns the ceiling — a debug binary under a tiny --max-rss starts over the ceiling and
    // must still serve the tail.
    if !follow {
        rss::check_point()?;
    }
    let cli_stderr = CliStderr;
    // The diagnostic record stream: `--diagnostics` retains the full capped stream (route, cost, raises, precision);
    // `--explain` retains it too, because the explain route block reads the same `ROUTE_SELECTED` records back out of
    // it. The default is Off — no sink, zero cost. Declared BEFORE the request context because the sink must outlive it
    // (the same rule as the stderr channel).
    let diagnostics_buffer = jqf_sdk::Diagnostics::new(if diagnostics || explain {
        jqf_codec_core::DiagnosticPolicy::All
    } else {
        jqf_codec_core::DiagnosticPolicy::Off
    });
    let mut resources = ResourceContext::new(
        account,
        &rss::GOVERNOR,
        WorkMeter::try_new_v1(COOPERATIVE_CREDITS).ok_or_else(|| "invalid cooperative work quantum".to_owned())?,
    )
    .map_err(|error| format!("request cancelled before start: {error:?}"))?
    .with_environment(host_environment()?)
    .with_stderr(&cli_stderr)
    .with_host_extension(Box::new(jqf_engine::ModuleLoaderHandle::new(Box::new(
        CliModuleLoader::new(library_paths),
    ))));
    // The retained input reaches the ledger HERE, where the bytes were read, and nowhere else. A drive borrows this
    // slice and the route ladder can hand it to several drives in turn (the round-trip and range-locate rungs both
    // validate the whole document before declining), so a charge inside a drive would count one read two or three
    // times. The streaming routes read their own bytes and charge their own reads for the same reason.
    resources
        .charge_input(retained_input)
        .map_err(|error| format!("error: cannot account the input: {}", resource_note(error)))?;
    // The mismatch dial (`052`): lenient is the default and IS jq; warn and strict come from `--mismatch-policy`. The
    // policy is a request field, set here before any lane runs, exactly like the rand seed below it.
    if let Some(policy) = mismatch_policy {
        resources = resources.with_mismatch_policy(policy);
    }
    // The edit lane's alias escape hatch (/ lane C3): the flag accepts the anchor-rewrite semantics for an edit through
    // an alias; the refusal stays the default. A request field, set before any lane runs, exactly like the mismatch
    // dial above it.
    if edit_expand_alias {
        resources = resources.with_edit_expand_alias(true);
    }
    // D5: `--types-as-strings` reads every extended temporal kind as its canonical text (a request field, set before
    // any lane runs, exactly like the mismatch dial above it).
    if types_as_strings {
        resources = resources.with_types_as_strings(true);
    }
    if let Some(policy) = strictness {
        resources = resources.with_strictness(policy);
    }
    // `--seed N`: primes the rand family's deterministic draw state so a repeated run answers byte-identically. Absent,
    // the request's draw state stays untouched and every draw keeps its ordinary impure uuid v4 entropy.
    if let Some(seed) = seed {
        resources = resources.with_rand_seed(seed);
    }
    // The diagnostic sink is installed ONCE, before any lane runs: every drive (sequence, record, execute) shares the
    // request context, so every engine raise site and precision boundary writes into the same retained stream.
    if let Some(diagnostics) = &diagnostics_buffer {
        resources.set_diagnostics(diagnostics);
    }
    // The external sort's host half (deferred item 17): installed whenever the request carries a spill budget, so
    // `--max-spill-bytes N` is the only switch between the in-memory floor and the spill path. A construction failure
    // is a hard refusal, never a silent fallback to the unbounded in-memory floor: the flag is an explicit memory
    // contract, and dropping it would be exactly the growth it exists to bound. The signal-handler stand-down guard is
    // declared BEFORE the store so it drops AFTER it: the store's own Drop removes the directory while the handlers are
    // still armed, then the handlers stand down.
    #[cfg(unix)]
    let mut _spill_signal_guard = SpillSignalGuard(false);
    let spill_store = if max_spill_bytes > 0 {
        Some(jqf_runtime::spill::TempDirSpillStore::try_new(None).map_err(|error| {
            format!(
                "error: cannot create the spill store required by --max-spill-bytes \
                 (the ceiling cannot be honored without it): {error}"
            )
        })?)
    } else {
        None
    };
    if let Some(store) = &spill_store {
        #[cfg(unix)]
        {
            spill_cleanup::register(&store.temp_dir());
            _spill_signal_guard.0 = true;
        }
        resources = resources.with_spill_store(store);
    }
    // The S2 route decision's override: JQF_CONTAINER_SPANS forces a frontier depth (the force-lazy differential's
    // switch); unset lets the engine's requirement lowering decide from the program's consumption class. The override
    // rides the request's policy into the lowered requirement — no process global.
    let mut policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    match container_span_frontier_override() {
        Ok(Some(depth)) => policy = policy.with_lazy_frontier(depth),
        Ok(None) => {}
        Err(message) => return Err(CliFailure::usage(&message)),
    }
    // The zero-argument identity path compiles `.` once per invocation rather than lowering a root requirement
    // directly: one uniform compile seam, so the program (its `?` flags) always threads into `execute_sequence`'s run.
    // `-f` reads the program from a file; the adopted message for an unreadable one is `Could not open <path>:
    // <reason>` at exit 2.
    let source_program: String = match &program_file {
        Some(path) => {
            let bytes = std::fs::read(path).map_err(|error| CliFailure::Message {
                class: ExitClass::Usage,
                message: format!("Could not open {}: {}", path.display(), io_error_text(&error)),
            })?;
            let text = String::from_utf8(bytes).map_err(|_| CliFailure::Message {
                class: ExitClass::Usage,
                message: "program is not valid UTF-8".to_owned(),
            })?;
            program = Some(text);
            program
                .as_deref()
                .ok_or_else(|| CliFailure::Message {
                    class: ExitClass::Usage,
                    message: "-f read a program file but none was stored".to_owned(),
                })?
                .to_owned()
        }
        None => program.as_deref().unwrap_or(".").to_owned(),
    };
    // the adopted `--stream` is an INPUT-PARSE mode: the parser yields `[path, leaf]` items, and the filter runs once
    // per item (`jq --stream P` ≡ `tostream | P` byte-for-byte against). For a JSON input the flag is served by the
    // BOUNDED event route: the engine's EventParser walks the retained text into `[path, leaf]` events one at a time
    // and the ORIGINAL program runs per event, so the document is never materialized — the "bounded" the old rewrite
    // could not offer (it read the input whole and ran `tostream` over the materialized document). The route also
    // serves `-s --stream` by collecting the events into one array, and `--stream-errors` by turning refusals into
    // `[message, path]` events.
    //
    // The rewrite survives for NON-JSON inputs (the event parser is the strict JSON grammar's; YAML/TOML/NDJSON keep
    // `tostream | P` over their own documents exactly as before) — and it is DIAGNOSED, never silent (plan 080: every
    // format either serves the event route or names the fallback).
    //
    // `-R` takes precedence: the adopted main.c installs the RAW input reader and never constructs the streaming parser
    // under it, so `-R --stream` is plain raw input. `-n` does NOT drop the flag — the adopted `-n` runs the filter
    // once over `null` and the input family drives the SAME streaming parser, so `jq -n --stream 'fromstream(inputs)'`
    // is the canonical streaming idiom: the event route serves it with the events as the shared input sequence. The
    // rewrite survives for NON-JSON inputs (the event parser is the strict JSON grammar's; YAML/TOML/NDJSON keep
    // `tostream | P` over their own documents exactly as before) — and it is DIAGNOSED, never silent (plan 080: every
    // format either serves the event route or names the fallback). The
    // `stream_events_requested`/`stream_events_incremental` facts were computed above the eager-read decision (the read
    // must know whether the source streams before it reads it); this block is the non-JSON half of that decision.
    if stream && !raw_input && input_selection.format != CliFormat::Json {
        // The materializing fallback, named (acceptance: no format silently materializes). The bounded event parser is
        // the strict JSON grammar's; another format's document is read whole and `tostream | P` runs over the
        // materialized value(s). Under `-n` there is nothing to rewrite — the adopted `-n` runs the filter once over
        // null and never parses the input unless the input family pulls it — so only the diagnostic fires.
        eprint_line_buffered(&format!(
            "jqf: warning: --stream on {} input is not served by the bounded event \
             route (it parses strict JSON input only); the input is read whole",
            input_selection.format.id()
        ));
        if !null_input {
            let rewritten = if slurp {
                format!("[.[] | tostream] | ({source_program})")
            } else {
                format!("tostream | ({source_program})")
            };
            program = Some(rewritten);
        }
    }
    // `--schema` is a per-input-value GATE, wired as a program rewrite exactly like `--stream`: each input value
    // validates against the `$__schema` document; a valid value runs the program, an invalid one publishes its ordered
    // error objects and terminates the run at exit 3 (`halt_error`'s law: the value goes raw to stderr, the process
    // exits with the code). Earlier valid values' outputs stand. args.rs rejects the combination with
    // --stream/--edit/--diff/--in-place, so this rewrite never composes with a stream or document-subject mode.
    if schema_file.is_some() {
        let rewritten = format!(
            "if schema_validate(.; $__schema) then ({source_program}) else \
             (schema_errors(.; $__schema) | halt_error(3)) end"
        );
        program = Some(rewritten);
    }
    // `--json-facts` wraps the whole program with the facts projection: every published value is rebuilt with its
    // attached facts visible in the JSON shape (see `--help facts`). The rewrite composes with the schema gate; args.rs
    // rejects the document-subject and stream modes up front. The AUTO default (xml/html input + JSON output) is a
    // rendering dial (args.rs: "the dial is a rendering choice, not a model change"). The rewrite is unconditional; the
    // specialized light routes it once forfeited are all deleted with the ladder collapse, so no pre-facts probe
    // remains.
    if json_facts {
        let rewritten = format!("({}) | json_facts", program.as_deref().unwrap_or("."));
        program = Some(rewritten);
    }
    let source_program = program.as_deref().unwrap_or(".");
    let source_label: String = program_file
        .as_ref()
        .map_or_else(|| String::from("<top-level>"), |path| path.display().to_string());
    // the adopted `--arg`/`--argjson`/`--slurpfile`/`--rawfile` bindings, in CLI order: `--arg` is the raw string
    // value, `--argjson` must be exactly one strict JSON value (any parse failure — malformed text or trailing content
    // — is the adopted `invalid JSON text passed to --argjson`, exit 2), `--slurpfile` reads every adjacent JSON value
    // from its file into an array, and `--rawfile` binds the file's raw bytes as a string.
    let mut value_bindings: Vec<(String, jqf_data::Value)> = Vec::new();
    // the adopted duplicate-name law, the FIRST POSITIONAL binding wins across ALL four flag kinds (`--argjson a 2
    // --arg a 1` answers `2`; `--slurpfile a f --arg a 1` answers the file's array), so a later same-name binding is
    // dropped whatever kind it is.
    let mut bound_names: Vec<String> = Vec::new();
    // The `--schema` document is bound FIRST as `$__schema` : the value-schema rewrite reads it, and
    // first-positional-wins keeps the binding against a user `--arg __schema` — the same rule that protects `$ARGS`
    // from a user binding.
    if let (Some(path), Some(bytes)) = (&schema_file, &schema_bytes) {
        let schema_label = path.to_string_lossy();
        let value = input::parse_schema_document(&schema_label, bytes, &resources)?;
        value_bindings.push(("$__schema".to_owned(), value));
        bound_names.push("$__schema".to_owned());
    }
    for (binding, file_bytes) in bindings.into_iter().zip(binding_files) {
        let (name, file, kind) = match &binding {
            CliBinding::Arg(name, value) => (name, None, BindingKind::Arg(value)),
            CliBinding::ArgJson(name, value) => (name, None, BindingKind::ArgJson(value)),
            CliBinding::SlurpFile(name, file) => (name, Some(file), BindingKind::SlurpFile(file_bytes)),
            CliBinding::RawFile(name, file) => (name, Some(file), BindingKind::RawFile(file_bytes)),
        };
        let key = format!("${name}");
        if bound_names.contains(&key) {
            continue;
        }
        bound_names.push(key.clone());
        let value = match kind {
            BindingKind::Arg(value) => {
                jqf_data::Value::try_string(value).map_err(|_| CliFailure::Message {
                    class: ExitClass::Usage,
                    message: "cannot allocate --arg value".to_owned(),
                })?
            }
            BindingKind::ArgJson(value) => parse_single_json_value(value, &resources)
                .ok_or_else(|| CliFailure::Message {
                    class: ExitClass::Usage,
                    message:
                        "invalid JSON text passed to --argjson\nUse jq --help for help with command-line options,\nor see the jq manpage, or online docs at https://jqlang.org"
                            .to_owned(),
                })?,
            // The file bytes were read before stdin; the parse and the value construction charge the request ledger.
            BindingKind::SlurpFile(Some(bytes)) => {
                let file_label = file.map(|path| path.to_string_lossy());
                slurpfile_value(
                    name,
                    file_label.as_deref().unwrap_or("?"),
                    &bytes,
                    &resources,
                )?
            }
            BindingKind::RawFile(Some(bytes)) => {
                let file_label = file.map(|path| path.to_string_lossy());
                rawfile_value(
                    name,
                    file_label.as_deref().unwrap_or("?"),
                    &bytes,
                    &resources,
                )?
            }
            // Unreachable: the early read filled every file-backed slot.
            BindingKind::SlurpFile(None) | BindingKind::RawFile(None) => {
                return Err(CliFailure::Message {
                    class: ExitClass::Usage,
                    message: "internal error: binding file was not read".to_owned(),
                });
            }
        };
        value_bindings.push((key, value));
    }
    // the adopted `$ARGS` builtin, available on EVERY request: `{"positional": [...], "named": {...}}`.
    // `--args`/`--jsonargs` fill `positional` in order (each value parsed under the mode active when it was seen), and
    // the winning `--arg`-family bindings fill `named`. jq OVERWRITES a user `--arg ARGS` with this object (`--arg ARGS
    // foo` makes `$ARGS` an object whose `.named.ARGS` is `"foo"`), so the object binding is inserted after the user
    // bindings and replaces any `$ARGS` they named.
    let positional = {
        let mut values = Vec::new();
        for positional in &positional_args {
            values.push(match positional {
                PositionalArg::String(text) => {
                    jqf_data::Value::try_string(text).map_err(|_| {
                        CliFailure::Message {
                            class: ExitClass::Usage,
                            message: "cannot allocate --args value".to_owned(),
                        }
                    })?
                }
                PositionalArg::Json(text) => parse_single_json_value(text, &resources)
                    .ok_or_else(|| CliFailure::Message {
                        class: ExitClass::Usage,
                        message:
                            "invalid JSON text passed to --jsonargs\nUse jq --help for help with command-line options,\nor see the jq manpage, or online docs at https://jqlang.org"
                                .to_owned(),
                    })?,
            });
        }
        jqf_data::Array::try_from_vec(values).map_err(|_| CliFailure::Message {
            class: ExitClass::Usage,
            message: "cannot allocate $ARGS.positional".to_owned(),
        })?
    };
    let mut named_builder =
        jqf_data::ObjectBuilder::try_with_capacity(value_bindings.len()).map_err(|_| CliFailure::Message {
            class: ExitClass::Usage,
            message: "cannot allocate $ARGS.named".to_owned(),
        })?;
    for (key, value) in &value_bindings {
        let name = key.strip_prefix('$').unwrap_or(key.as_str());
        let name = jqf_data::ObjectKey::try_from_str(name).map_err(|_| CliFailure::Message {
            class: ExitClass::Usage,
            message: "cannot allocate $ARGS.named key".to_owned(),
        })?;
        named_builder
            .try_insert_last(name, value.clone())
            .map_err(|_| CliFailure::Message {
                class: ExitClass::Usage,
                message: "cannot allocate $ARGS.named entry".to_owned(),
            })?;
    }
    let named = named_builder
        .try_finish()
        .map(jqf_data::Value::Object)
        .map_err(|_| CliFailure::Message {
            class: ExitClass::Usage,
            message: "cannot allocate $ARGS.named".to_owned(),
        })?;
    let mut args_builder = jqf_data::ObjectBuilder::try_with_capacity(2).map_err(|_| CliFailure::Message {
        class: ExitClass::Usage,
        message: "cannot allocate $ARGS".to_owned(),
    })?;
    for (name, value) in [("positional", jqf_data::Value::Array(positional)), ("named", named)] {
        let key = jqf_data::ObjectKey::try_from_str(name).map_err(|_| CliFailure::Message {
            class: ExitClass::Usage,
            message: "cannot allocate $ARGS key".to_owned(),
        })?;
        args_builder
            .try_insert_last(key, value)
            .map_err(|_| CliFailure::Message {
                class: ExitClass::Usage,
                message: "cannot allocate $ARGS entry".to_owned(),
            })?;
    }
    let args_value = args_builder
        .try_finish()
        .map(jqf_data::Value::Object)
        .map_err(|_| CliFailure::Message {
            class: ExitClass::Usage,
            message: "cannot allocate $ARGS".to_owned(),
        })?;
    value_bindings.retain(|(key, _)| key != "$ARGS");
    value_bindings.push((String::from("$ARGS"), args_value));
    let compile_started = std::time::Instant::now();
    // Fact assignment lowers in every compile; `--edit` selects the edit route, not a distinct compile mode.
    let compiled = try_compile_program(
        source_program,
        policy,
        CompileOptions {
            cli_vars: &value_bindings,
            split_exp: false,
            source_label: &source_label,
        },
        &resources,
    )
    .map_err(|error| compile_failure_maybe_explain(&error, source_program, explain))?;
    let compile_elapsed = compile_started.elapsed();
    // The `--split-exp` destination: the expression comes from the argument or from `--split-exp-file PATH` (read like
    // `-f`, with the same `Could not open` message at exit 2). It compiles ONCE through the split entry, which resolves
    // an unbound `$index` to a runtime slot the per-item drive seeds; the expression is STANDALONE — it sees no `--arg`
    // bindings (D18 reserves `$index` alone), so a `$name` inside it is the ordinary `$name is not defined`.
    let split_exp_text: Option<String> = match (&split_exp, &split_exp_file) {
        (Some(expr), None) => Some(expr.clone()),
        (None, Some(path)) => Some(std::fs::read_to_string(path).map_err(|error| CliFailure::Message {
            class: ExitClass::Usage,
            message: format!("Could not open {}: {}", path.display(), io_error_text(&error)),
        })?),
        // args.rs refuses the pair and the twice-given spellings before a byte is read; either other shape is an
        // internal inconsistency.
        _ => None,
    };
    let split_source_label = split_exp_file.as_ref().map(|path| path.display().to_string());
    let split_program: Option<jqf_engine::CompiledProgram> = match &split_exp_text {
        Some(expr) => {
            let options = CompileOptions {
                split_exp: true,
                source_label: split_source_label.as_deref().unwrap_or("<top-level>"),
                ..CompileOptions::new()
            };
            Some(
                jqf_engine::try_compile_program(expr, policy, options, &resources)
                    .map_err(|error| compile_failure_maybe_explain(&error, expr, explain))?,
            )
        }
        None => None,
    };
    // The second CLI decision point: the compiled program arena is live now. The follow route owns its own per-cycle
    // refusal law (D3): a live tail that starts over the ceiling — a debug binary under a tiny --max-rss, or a tail
    // whose first record over-allocates — reports per cycle and keeps serving. The generic pre-route checks would
    // pre-empt that law (docker battery catch, 2026-08-15: the linux-amd64 debug binary's startup RSS exceeds a 24M
    // ceiling, so the check fired before the route and killed the tail).
    if !follow {
        rss::check_point()?;
    }
    if explain {
        for line in explain::render_plan(source_program, &compiled, compile_elapsed) {
            eprint_line_buffered(&line);
        }
        flush_stderr();
    }
    // The plan surface: `--plan-out` writes this program's serialized routing-facts plan, `--plan-file` loads one and
    // requires it to match the freshly compiled program. Both run here, before any input is read, so a plan mismatch is
    // a startup error with no stdin consumed.
    if let Some(path) = &plan_out {
        std::fs::write(path, compiled.serialize_plan()).map_err(|error| CliFailure::Message {
            class: ExitClass::Usage,
            message: format!("cannot write plan to {}: {error}", path.display()),
        })?;
    }
    if let Some(path) = &plan_file {
        let bytes = std::fs::read(path).map_err(|error| CliFailure::Message {
            class: ExitClass::Usage,
            message: format!("cannot read plan file {}: {error}", path.display()),
        })?;
        let saved = jqf_engine::PlanRecord::deserialize(&bytes).map_err(|error| CliFailure::Message {
            class: ExitClass::Usage,
            message: format!("plan file {} is not a valid jqf plan: {error}", path.display()),
        })?;
        if saved != compiled.plan_record() {
            return Err(CliFailure::Message {
                class: ExitClass::Usage,
                message: format!(
                    "plan file {} does not match the compiled program's routing facts \
                     (the plan cannot drift from the route)",
                    path.display()
                ),
            });
        }
    }
    // The B1 law: a fact write (`PATH.@comment = …`, `PATH.&name = …`) splices fact lines into the RETAINED source, so
    // the format must be able to carry them. JSON — the one edit-lane format that cannot — would patch fact lines into
    // the file and then fail the verify re-decode, blaming the user's (fine) input for the format's own limitation.
    // Reject at argument time, where the `--edit` help text says the rejection lives. The `compiled.fact_writes` fact
    // is the compile's own answer (any `ProgramNode:FactAssign`, whatever role it writes), so the check cannot drift
    // from the lane that would serve the write — and the message names the CLASS, never one spelling of it.
    if edit && compiled.fact_writes() && input_selection.format == CliFormat::Json {
        return Err(CliFailure::usage(
            "JSON cannot carry this fact write: a `.@…`/`.&…` fact assignment \
             requires a fact-carrying format (toml, yaml, jsonc, json5, properties, ini, dotenv)",
        ));
    }
    // The `--follow` input-family law (gate 3, the live-window arm): an input-family program over a live tail runs
    // ONCE, with the records served through `inputs`/`input` (the rolling-window shape), so `-n --follow` is that same
    // drive — the program runs once over null and pulls the live records — and is allowed for exactly that shape. A
    // non-input `-n --follow` would run once over null and end the tail, so it stays a usage error (the args-level
    // refusal was narrowed to -R/-s, which read to completion). `~inputs` stays rejected outright: the named resident
    // needs the whole-input shared sequence the live stream does not host, and the follow route cannot set it up.
    if follow && compiled.uses_inputs_cursor() {
        return Err(CliFailure::usage(
            "`~inputs` is not served under --follow (the live stream feeds `inputs`; \
             the named resident needs the whole-input shared sequence)",
        ));
    }
    if follow && null_input && !compiled.uses_input_family() {
        return Err(CliFailure::usage(
            "-n over --follow's live stream is served only for programs that pull \
             `inputs`/`input` (a program that never reads the input would run once \
             over null and end the tail)",
        ));
    }
    // The streaming input-family lane: a NON-SEEKABLE stdin under the DEFAULT adjacent-JSON input serves
    // `input`/`inputs` by pulling values from the pipe ON DEMAND instead of reading it whole — `-n 'limit(1; inputs)'`
    // answers as soon as one value arrives and never reads past its demand, exactly like jq. The record dialects and
    // `--stream` keep the whole-read shared cursor (their decode tables and the event rewrite are whole-input by
    // construction), and `--in-place` owns its per-file model. This fact is computed ONCE and passed to `plan:resolve`,
    // so the read deferral below cannot drift from the route that serves it.
    let stream_input_family = stream_stdin
        && compiled.uses_input_family()
        && !stream
        && !in_place
        && input_selection.format == CliFormat::Json
        && input_selection.dialect.record_kind().is_none();
    // The streamed `-n` input-family lane : a null-input program that pulls `input`/`inputs` over ONE source — stdin or
    // a single named file — whose values the engine's streaming cursor decodes byte-for-byte (default RFC 8259 adjacent
    // values and strict NDJSON records) is served by pulling bytes ON DEMAND instead of reading whole. The whole read
    // at the deferral below was ~99% of this shape's peak RSS (§0a: 1.011× input on the 500 MB sum fold); the cursor's
    // window is O(one record), so retention drops to window + accumulator. Strict-NDJSON framing faults (blank line,
    // bare CR, two texts on one line) are NOT re-raised by this lane — the adjacent-value grammar accepts them, the
    // same decode every other streamed `-n inputs` surface already uses. Everything else keeps the whole-read:
    // json-seq's RS framing and CSV's quoted records are not adjacent JSON, recovering dialects carry their own fault
    // laws, multi-file requests owe per-file provenance and the missing-file walk over the combined buffer,
    // `-R`/`--stream` rewrite the byte stream before any route sees it.
    let streamed_null_first = null_input
        && compiled.uses_input_family()
        && !follow
        && diff_pair.is_none()
        && !in_place
        && !raw_input
        && !stream
        && input_files.len() <= 1
        && matches!(
            input_selection.dialect,
            CliInputDialect::Rfc8259 | CliInputDialect::NdjsonStrict
        );
    // Resolve the read deferral: only an input-family program reads under `-n`; the input is every positional file's
    // bytes joined into ONE retained stream (the adopted multi-file law, no separator) — or stdin, labelled `<stdin>`,
    // when no file was named — with the per-file ranges as the provenance `input_filename` and the per-file
    // `input_line_number` reset resolve against.
    let (mut input, mut file_ranges) = match eager_input {
        Some(combined) => combined,
        None => {
            if follow || diff_pair.is_some() || in_place {
                // `--follow` reads incrementally, `--diff` reads its own two files, `--in-place` reads each file inside
                // its own run; no whole buffer exists here. (The stream route reads the pipe itself; an input-family
                // program over a SEEKABLE or record-dialect input still reads whole below — that shared cursor is
                // whole-input by construction.)
                (Vec::new(), Vec::new())
            } else if crate::plan::stream_events_incremental_route(stream_events_incremental, &compiled) {
                // The incremental `--stream` event drive reads the source itself — a file or stdin, seekable or not —
                // in jq's fgets-shaped chunks; no whole buffer exists here, which is the bounded promise. An
                // INPUT-FAMILY program stays on the whole-input event route below: its shared event cursor is
                // whole-input by construction (the streaming drive serves no cursor; the differential pins its
                // per-event answers).
                (Vec::new(), Vec::new())
            } else if stream_input_family {
                // The streaming input-family lane reads the pipe itself, on demand; no whole buffer exists here.
                (Vec::new(), Vec::new())
            } else if streamed_null_first {
                // The streamed `-n` lane reads nothing here either: its route pulls the source bytes on demand, which
                // is exactly what keeps a fold's retention at window size.
                (Vec::new(), Vec::new())
            } else if compiled.uses_input_family() {
                // Every other input-family shape reads the whole stream even on a pipe: its shared cursor is
                // whole-input by construction, and the stream route never serves it.
                let combined = read_combined_input(&input_files, &input_file_labels)?;
                // §0b: this deferred read is a real ~1× residency — every route borrows these bytes for the whole
                // request — so it joins the retained-input split here, where it was read. The eager charge above saw
                // zero of it (`-n` skipped the eager read), and no other path reaches this arm (follow, diff, in-place,
                // the streaming lanes, and the streaming input-family lane all take earlier arms and charge their own
                // reads), so nothing is charged twice.
                retained_input += combined.0.len() as u64;
                rss::set_retained_input(retained_input);
                resources
                    .charge_input(combined.0.len() as u64)
                    .map_err(|error| format!("error: cannot account the input: {}", resource_note(error)))?;
                combined
            } else if stream_stdin {
                // The stream route reads the pipe itself, incrementally; no whole buffer exists here.
                (Vec::new(), Vec::new())
            } else {
                (Vec::new(), Vec::new())
            }
        }
    };
    // the adopted `-R`/`--raw-input`: the input becomes a stream of JSON strings — one per line, or (with `-s`) the
    // whole input as one string. Invalid UTF-8 is converted lossily (U+FFFD per invalid byte), exactly as jq renders
    // it. The transformed bytes are plain adjacent JSON, so the rest of the ladder runs unchanged over them.
    if raw_input {
        input = raw_input_bytes(&input, slurp);
        // The transform rewrites the byte stream, so the per-file ranges no longer describe it; `input_filename`
        // reports the whole-source label (matching the single-file `-R` behavior) instead of per-file facts.
        file_ranges.clear();
    }
    // `-R` rewrote the input into JSON string literals BEFORE any route decision, so a RECORD input selection no longer
    // describes these bytes: the physical framing the record route owns is gone, and `-Rs` produces one unterminated
    // string that no record framer would accept. The named format therefore stops describing the input exactly as it
    // stops describing YAML's (`--input-format yaml -R` publishes one JSON string per line, the YAML grammar never
    // consulted — 2026-08-04).
    let input_selection = if raw_input && input_selection.dialect.record_kind().is_some() {
        CliInputSelection {
            format: CliFormat::Json,
            dialect: CliInputDialect::Rfc8259,
            csv_delimiter: None,
        }
    } else {
        input_selection
    };
    // Record PAYLOADS are always strict JSON: the framer owns the physical stream, strict JSON owns every byte inside a
    // record. So for the three JSON-framed record dialects the payload format/dialect stay `json/rfc8259`; for every
    // other input the format/dialect come from the selection (each dialect's registered identity IS its codec dialect).
    let (format, dialect) = match input_selection.dialect {
        CliInputDialect::NdjsonStrict | CliInputDialect::NdjsonRecovering | CliInputDialect::JsonSeqStrict => {
            let format = FormatId::try_new(jqf_codec_json::FORMAT_ID)
                .map_err(|error| format!("invalid built-in format identity: {error}"))?;
            let dialect = DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID)
                .map_err(|error| format!("invalid built-in dialect identity: {error}"))?;
            (format, dialect)
        }
        dialect => {
            let format = FormatId::try_new(dialect.format().id())
                .map_err(|error| format!("invalid built-in format identity: {error}"))?;
            let dialect = DialectId::try_new(dialect.id())
                .map_err(|error| format!("invalid built-in dialect identity: {error}"))?;
            (format, dialect)
        }
    };
    let output_format = FormatId::try_new(output_selection.format.id())
        .map_err(|error| format!("invalid built-in output format identity: {error}"))?;
    let output_dialect = DialectId::try_new(output_selection.dialect.id())
        .map_err(|error| format!("invalid built-in output dialect identity: {error}"))?;
    let encode_options = NdjsonEncodeOptions::new(output_selection.terminator);
    let json_encode_options = JsonEncodeOptions {
        indent: output_selection.indent,
        raw_strings: output_selection.raw_strings,
        sort_keys: output_selection.sort_keys,
        ascii_output: output_selection.ascii_output,
        raw_output_nul: output_selection.raw_output_nul,
    };
    // The JSONC output profile mirrors the selected output dialect: the strict-comma profile on the named jqf dialects
    // (the edit lane's floor re-encode included), the trailing profile otherwise.
    let jsonc_options = jqf_codec_json::jsonc::JsoncEncodeOptions {
        style: json_encode_options,
        profile: match output_selection.dialect {
            CliOutputDialect::JsoncJqf | CliOutputDialect::JsoncDefaultJqf => {
                jqf_codec_json::jsonc::JsoncEncodeProfile::Default
            }
            _ => jqf_codec_json::jsonc::JsoncEncodeProfile::Trailing,
        },
    };
    // JSON5 has ONE output rendering — every named json5 dialect, the edit lane's floor included, writes the same bytes
    // — so the style options are the whole encoder request.
    let json5_encoder_options = json_encode_options;
    let json_seq_encode_options = jqf_codec_json::seq::JsonSeqEncodeOptions::new(
        json_encode_options,
        if output_selection.raw_output_nul {
            // `--raw-output0` implies `-j`'s no-LF law and replaces the LF with a NUL, exactly as the adopted `--seq
            // --raw-output0` writes `str\0` / `RS 123 \0`.
            jqf_codec_json::seq::JsonSeqSuffix::Nul
        } else if output_selection.no_newline {
            jqf_codec_json::seq::JsonSeqSuffix::NoSuffix
        } else {
            jqf_codec_json::seq::JsonSeqSuffix::Lf
        },
    );
    let csv_encode_options = jqf_codec_delimited::CsvEncodeOptions::try_new(input_selection.csv_delimiter)
        .map_err(|error| format!("invalid built-in CSV encode options: {error:?}"))?;
    let tsv_encode_options = jqf_codec_delimited::CsvEncodeOptions::try_new_tsv()
        .map_err(|error| format!("invalid built-in TSV encode options: {error:?}"))?;
    let yaml_target_schema = match input_selection.dialect {
        CliInputDialect::YamlFailsafe => YamlTargetSchema::Failsafe,
        CliInputDialect::YamlJson => YamlTargetSchema::Json,
        _ => YamlTargetSchema::Core,
    };
    let render_encode_options = output_selection.render.encode_options();
    // The request source over the retained combined bytes. The per-file ranges (when files were named) travel alongside
    // as `file_ranges`, threaded to the value drives as the `input_filename`/per-file-line provenance; `ResolvedSource`
    // itself stays LEAN (it is copied in every decode recursion, so a multi-file feature must not grow it).
    // `--in-place` builds its source per file inside the loop below instead.
    let combined_source = ResolvedSource::new(
        SourceRef::new(SourceId::new(0), SourceKind::Input),
        &input_label,
        &input,
        0,
    );
    // The typed options channel (123 X5): every arm hands the codec its own concrete options struct as `&dyn Any`; the
    // codec's factory downcasts to its own type at a compile-time-known boundary. CBOR and jqfjson declare no v1
    // options and carry none.
    let encode_options: Option<&(dyn core::any::Any + Send + Sync)> = match output_selection.format {
        CliFormat::Ndjson => Some(&encode_options),
        CliFormat::JsonSeq => Some(&json_seq_encode_options),
        CliFormat::Yaml => Some(&yaml_target_schema),
        CliFormat::Json => Some(&json_encode_options),
        CliFormat::Jsonc => Some(&jsonc_options),
        CliFormat::Json5 => Some(&json5_encoder_options),
        CliFormat::Csv => Some(&csv_encode_options),
        CliFormat::Tsv => Some(&tsv_encode_options),
        CliFormat::Jqft => Some(&jqf_codec_jqft::JqftEncodeOptions {
            with_source: output_selection.with_source,
        }),
        CliFormat::Jqfb => Some(&jqf_codec_jqft::JqfbEncodeOptions {
            with_source: output_selection.with_source,
        }),
        CliFormat::Cbor
        | CliFormat::CborSeq
        | CliFormat::Toml
        | CliFormat::Html
        | CliFormat::Jqfjson
        | CliFormat::Properties
        | CliFormat::Ini
        | CliFormat::Dotenv
        | CliFormat::Messagepack
        | CliFormat::Xml => None,
        CliFormat::Render => Some(&render_encode_options),
    };
    // For JSON output the facade owns the item newline, exactly as before. For NDJSON output the CODEC owns the
    // terminator and appends it inside its own Who owns the byte between items is the codec's own declaration now (item
    // G): each registration declares, per dialect, whether the facade appends the item suffix or the codec's own
    // encoding carries it. This single lookup replaces the per-format match that used to live here -- the TOML arm's
    // claim that "the facade owns the item newline like JSON" is now the declared row's fact, not a CLI comment. Only
    // the two style dials stay local: the adopted `-j`/`--join-output` drops the JSON facade suffix entirely, and
    // `--raw-output0` replaces it with a NUL (checked BEFORE `-j`, matching jq, where both orders terminate on NUL).
    let facade_framing = if output_selection.format == CliFormat::Json {
        FacadeFraming::item_suffix(
            jqf_runtime::JsonItemSuffix::from_dials(output_selection.raw_output_nul, output_selection.no_newline)
                .as_bytes(),
        )
    } else {
        match catalog.item_byte_owner(&output_format, &output_dialect).map_err(|_| {
            format!(
                "cannot resolve inter-item byte owner for {} {}",
                output_selection.format.id(),
                output_selection.dialect.id()
            )
        })? {
            ItemByteOwner::Codec => FacadeFraming::item_suffix(b""),
            ItemByteOwner::Facade => FacadeFraming::item_suffix(b"\n"),
        }
    };
    // The input-model fact comes from the codec's own route_capabilities declaration: a format whose registration does
    // not declare RouteCapability:AdjacentValues is exactly one text per source. The query names the CLI-facing dialect
    // (ndjson/json-seq/csv for the record inputs), not the record PAYLOAD format (strict JSON).
    let input_cli_format = FormatId::try_new(input_selection.dialect.format().id())
        .map_err(|error| format!("invalid built-in format identity: {error}"))?;
    let input_cli_dialect = DialectId::try_new(input_selection.dialect.id())
        .map_err(|error| format!("invalid built-in dialect identity: {error}"))?;
    let adjacent_values_input = catalog
        .route_capabilities(&input_cli_format, &input_cli_dialect)
        .map_err(|error| format!("cannot resolve input route capabilities: {error:?}"))?
        .contains(&jqf_codec_core::RouteCapability::AdjacentValues);
    let single_document_input = !adjacent_values_input;
    let pipeline_policy = PipelinePolicy {
        decode: DecodeRequest {
            validation: ValidationMode::Strict,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            dialect: &input_cli_dialect,
            options: None,
            // A single-document format (TOML) is exactly one text: the source is not a stream of adjacent values, so
            // the adjacent-value opt-in stays off.
            allow_adjacent_values: !single_document_input,
            // The skip set is the registration's own declaration (the codec-supplied law): a codec whose adjacent-value
            // stream carries insignificant trivia declares the set beside the capability that requires it, and the CLI
            // never re-declares a per-format list.
            value_separator: catalog
                .value_separators(&input_cli_format, &input_cli_dialect)
                .map_err(|error| format!("cannot resolve input value separators: {error:?}"))?,
        },
        encode_diagnostics: DiagnosticPolicy::ErrorsOnly,
        preservation: PreservationRequest::None,
        encode_options,
        cooperative_credits: COOPERATIVE_CREDITS,
        split: split_program.as_ref(),
        max_iterations,
    };
    // The route decision is resolved ONCE, from static request facts, into an ordered candidate chain (plan.rs): host
    // rungs (follow, stream, record, edit, serve, value-lane) either serve or decline, and the serial tail is one
    // Request → execute. The roundtrip / range-locate / input-family / floor order lives inside execute.
    //
    // The colour decision, resolved beside the route decision: the adopted switch law plus jqf's two extensions. `-M`
    // always wins; `-C` forces colour on, even under NO_COLOR and even into a file destination (the adopted `-C`
    // colours a redirected file); with neither, the DESTINATION's terminal-ness decides — a file destination is never a
    // terminal, and a non-empty NO_COLOR turns the default off. The data lanes (`--edit`/ `--diff`/`--in-place`) never
    // colour: they are in the business of preserving document bytes, and colour renders JSON-family output only.
    let data_lane = edit || in_place || diff_pair.is_some();
    let destination_is_tty = !data_lane && output_path.is_none() && io::stdout().is_terminal();
    let no_color_nonempty = std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty());
    // render.terminal@1's TREE shape joins the colourable surface: its frames are line-structured text with exact token
    // kinds, so the same rendering-of-decided-bytes law applies. The plain and table shapes have no lexical law and
    // stay monochrome.
    let terminal_tree_colourable = output_selection.format == CliFormat::Render
        && output_selection.dialect == CliOutputDialect::RenderTerminal
        && output_selection.render.shape.unwrap_or_default() == jqf_codec_render::TerminalShape::Tree;
    let colour_render: Option<colour::ColourRender> = colour::colour_engaged(
        colour,
        destination_is_tty,
        no_color_nonempty,
        data_lane,
        colour::is_json_family(output_selection.format) || terminal_tree_colourable,
    )
    .then_some(colour::ColourRender {
        palette,
        raw_arm: output_selection.raw_strings,
        ascii: output_selection.ascii_output,
        terminal_tree: terminal_tree_colourable,
    });
    let plan = plan::resolve(
        input_selection,
        output_selection,
        edit,
        diff_pair.as_ref().map(|(old, new)| (old.as_path(), new.as_path())),
        null_input,
        slurp,
        raw_input,
        follow,
        stream_stdin,
        stream_input_family,
        streamed_null_first,
        stream_events_requested,
        &compiled,
        single_document_input,
    );
    // The whole run body is ONE closure so a file destination can be committed (flush + atomic rename) only after every
    // lane succeeded; on failure the temp file is removed instead of being renamed over the target. The closure runs
    // once over the combined multi-file input, or once PER FILE under `--in-place` (each file's bytes read, edited, and
    // written back to itself independently — the 058 W2 ruling). It returns the published-item facts
    // `-e`/`--exit-status` judges.
    let run_started = std::time::Instant::now();
    // `--in-place` reads each positional file inside its own run, so those bytes join the retained input as they are
    // read — the same figure the eager read publishes to the diagnostics report, accumulated across the loop on top of
    // the base the eager stage already published (the Cell crosses the two closures; the request is single-thread).
    let inplace_retained_input = std::cell::Cell::new(retained_input);
    let exit_facts: Option<ExitFacts> = {
        let mut run_one = |source: ResolvedSource<'_>,
                           files: Option<&[jqf_source::SourceFileRange<'_>]>,
                           input_bytes: &[u8],
                           in_place_file: Option<&std::path::Path>|
         -> Result<ExitFacts, CliFailure> {
            // `--in-place` reads each positional file inside its OWN run, so those bytes join the request's input here
            // — every other route's input was charged where the eager read happened, and charging again here would
            // count it twice.
            if in_place_file.is_some() {
                resources
                    .charge_input(input_bytes.len() as u64)
                    .map_err(|error| format!("error: cannot account the input: {}", resource_note(error)))?;
                let total = inplace_retained_input.get() + input_bytes.len() as u64;
                inplace_retained_input.set(total);
                rss::set_retained_input(total);
            }
            // Document `--edit` publishes through its own buffered lane and writes the final bytes itself, so the
            // ordinary sink stays empty there. Record `--edit` (CSV/TSV/NDJSON) serves on the record route and
            // publishes through this sink, so `--in-place` / `--output` must still open the file destination —
            // otherwise the splice prints to stdout and never touches the file.
            let record_edit = edit && input_selection.dialect.record_kind().is_some();
            let file_destination = if edit && !record_edit {
                None
            } else {
                in_place_file.or(output_path.as_deref())
            };
            let issue_text = match input_selection.dialect {
                CliInputDialect::JsonSeqStrict => jqf_codec_json::seq::issue_text,
                _ => jqf_codec_json::ndjson::issue_text,
            };
            let mut sink = match file_destination {
                Some(path) => StdoutSink {
                    output: CliWriter::file(path, !no_atomic)?,
                    unbuffered,
                    exit_items: 0,
                    exit_last_truthy: None,
                    exit_last_empty_array: None,
                    issue_text,
                    colour: colour_render.clone(),
                    pending: Vec::new(),
                    rendered: Vec::new(),
                    split: split_program.as_ref().map(|_| SplitDestination::new(!no_atomic)),
                },
                None => StdoutSink {
                    output: CliWriter::stdout(),
                    unbuffered,
                    exit_items: 0,
                    exit_last_truthy: None,
                    exit_last_empty_array: None,
                    issue_text,
                    colour: colour_render.clone(),
                    pending: Vec::new(),
                    rendered: Vec::new(),
                    split: split_program.as_ref().map(|_| SplitDestination::new(!no_atomic)),
                },
            };
            let outcome = (|| -> Result<(), CliFailure> {
                let mut ctx = RouteContext {
                    catalog,
                    source,
                    files,
                    format: &format,
                    dialect: &dialect,
                    output_format: &output_format,
                    output_dialect: &output_dialect,
                    policy: pipeline_policy,
                    framing: facade_framing,
                    compiled: &compiled,
                    resources: &mut resources,
                    sink: &mut sink,
                    diagnostics,
                    diagnostics_buffer: diagnostics_buffer.as_ref(),
                    input: input_bytes,
                    input_selection,
                    output_selection,
                    parallel_selection,
                    exit_status,
                    // Colour engagement: a colour-engaged request plans the value and record lanes SERIAL, because the
                    // parallel relay publishes a byte stream without item boundaries — the colour pass renders per
                    // item, and the raw-text skip arrives in the item report. The serial drives publish per item with
                    // their reports, exactly as the `-e` and mismatch declines do.
                    colour: colour_render.is_some(),
                    host_bindings,
                    // The bounded `--stream` route's facts (/080): `--stream-errors` turns refusals into events, `-s`
                    // collects the events into one array before the run, and `-n` runs the program once over null with
                    // the events served through the input family.
                    stream_errors,
                    stream_events_requested,
                    // The incremental `--stream` event drive ( residual): the route reads the SOURCE itself in jq's
                    // fgets-shaped chunks instead of the retained buffer, so only the parser's path stack is resident.
                    // An input-family program is excluded — its shared event cursor is whole-input by construction.
                    stream_events_incremental: crate::plan::stream_events_incremental_route(
                        stream_events_incremental,
                        &compiled,
                    ),
                    null_input,
                    slurp,
                    // the adopted precedence, unchanged over a record stream: `-n` beats `-s` (`-R` never reaches here
                    // — it rewrote the input into JSON above, so the record route is not on the chain at all).
                    record_model: if null_input {
                        jqf_runtime::records::RecordRunModel::NullFirst
                    } else if slurp {
                        jqf_runtime::records::RecordRunModel::Slurped
                    } else {
                        jqf_runtime::records::RecordRunModel::PerRecord
                    },
                    // The json-seq framing profile: the flag-scoped RECOVERING route under `--seq` (with no explicit
                    // input selection), strict for an explicit json-seq selection. The seq route's exit law also
                    // changes: the adopted `--seq` parse errors are never fatal, so the record route skips its
                    // error-issues exit check.
                    json_seq_profile: if seq_recovering {
                        jqf_codec_json::seq::JsonSeqProfile::Recovering
                    } else {
                        input_selection
                            .dialect
                            .json_seq_profile()
                            .unwrap_or(jqf_codec_json::seq::JsonSeqProfile::Strict)
                    },
                    force_issue_exit: !seq_recovering,
                    follow_input: input_files.first().map(std::path::PathBuf::as_path),
                    streamed_null_first,
                    streamed_null_first_file: streamed_null_first
                        .then(|| input_files.first())
                        .flatten()
                        .map(std::path::PathBuf::as_path),
                    edit_check,
                    edit,
                    in_place: in_place_file,
                    output_path: output_path.as_deref(),
                    no_atomic,
                    diff_pair: diff_pair.as_ref().map(|(old, new)| (old.as_path(), new.as_path())),
                    diff_old_selection,
                    diff_new_selection,
                    single_document_input,
                    compile_policy: policy,
                };
                for route in &plan {
                    // The third CLI decision point: one per route attempt (and per file under `--in-place`), so a
                    // request that grew between drives refuses before the next drive starts. Same follow exemption as
                    // the second decision point: the follow route's cycles check internally.
                    if !follow {
                        rss::check_point()?;
                    }
                    // The `~inputs` resident is scoped to the `-n` null-first drive: a cursor over the shared input
                    // sequence collides with the per-element cursor-store reset, so every other model rejects before a
                    // byte is read (the compile carries the marker; the ruling is lower-time). The generic
                    // `~cursor(inputs)` spelling is NOT marked — it keeps the adopted inputs-under-per-input behavior,
                    // and only the named resident owns the `-n` contract.
                    if !null_input && ctx.compiled.uses_inputs_cursor() {
                        return Err(CliFailure::usage(
                            "`~inputs` is served only under `-n`/`--null-input` \
                             (a cursor over the input sequence needs the \
                             single-run drive)",
                        ));
                    }
                    match routes::run(*route, &mut ctx) {
                        Ok(RouteOutcome::Completed) => return routes::shared_epilogue(&mut ctx),
                        Ok(RouteOutcome::Served) => return Ok(()),
                        Ok(RouteOutcome::Declined) => {}
                        Err(failure) => return Err(failure),
                    }
                }
                Ok(())
            })();
            match outcome {
                Ok(()) => {
                    sink.output.commit()?;
                    Ok((sink.exit_items, sink.exit_last_truthy, sink.exit_last_empty_array))
                }
                Err(failure) => Err(failure),
            }
        };
        let facts = (|| -> Result<Option<ExitFacts>, CliFailure> {
            if in_place {
                // 058 W2's `--in-place` ruling: each positional file is edited INDEPENDENTLY — one run per file, that
                // file's bytes read as the input and its output written back to itself. A failing file aborts the whole
                // request (earlier files stand, later files are never touched); `-n`/`-s` are refused up front, so each
                // run sees exactly that file's stream.
                let mut facts = None;
                for (path, label) in input_files.iter().zip(input_file_labels.iter()) {
                    let file_bytes = read_input_file(path)?;
                    let file_bytes = if raw_input {
                        raw_input_bytes(&file_bytes, slurp)
                    } else {
                        file_bytes
                    };
                    let file_source = ResolvedSource::new(
                        SourceRef::new(SourceId::new(0), SourceKind::Input),
                        label,
                        &file_bytes,
                        0,
                    );
                    facts = Some(run_one(file_source, None, &file_bytes, Some(path.as_path()))?);
                }
                Ok(facts)
            } else {
                let files = if file_ranges.is_empty() {
                    None
                } else {
                    Some(file_ranges.as_slice())
                };
                run_one(combined_source, files, &input, None).map(Some)
            }
        })();
        match facts {
            Ok(facts) => facts,
            Err(failure) => {
                // The run's own reports print even when it died — both are aggregated in THIS request context, so a run
                // that died on its input still says what its cells saw and what the encoders lost.
                print_projection_warnings(&resources);
                print_mismatch_warn_report(&resources, diagnostics_buffer.as_ref());
                return Err(failure);
            }
        }
    };
    let run_elapsed = run_started.elapsed();
    // The mismatch dial's warn report (`052` W2): the adopted answer and exit code, plus ONE capped, aggregated line on
    // stderr after the run — per-cell counts, never one line per event. Printed here beside the projection warnings, at
    // the one place every run has finished and the request context is still alive.
    print_projection_warnings(&resources);
    print_mismatch_warn_report(&resources, diagnostics_buffer.as_ref());
    if explain {
        for line in explain::render_route(diagnostics_buffer.as_ref(), &resources, run_elapsed) {
            eprint_line_buffered(&line);
        }
        flush_stderr();
    }
    // `-e`/`--exit-status` decides the SUCCESS exit code from the LAST output value's truthiness (the adopted law): 0
    // truthy, 1 false/null, 4 no output at all. Under `--in-place` the "last" value is the LAST FILE's run. A failed
    // run keeps its own exit class — a runtime error is exit 5 even under `-e`, and `halt` keeps its code.
    let success_code = if exit_status {
        match exit_facts {
            // No item ever published: the adopted "no results" exit class.
            Some((0, _, _)) | None => 4,
            Some((_, Some(false), _)) => 1,
            // Truthy last value, or (defensively) an item with no verdict — unreachable under `-e`, because every
            // expression-output route reports the value truthiness and edit/diff are rejected.
            Some((_, Some(true) | None, _)) => 0,
        }
    } else if diff_pair.is_some() {
        // The diff exit law (jqf's own — diff is a jqf extension, so the verdict is jqf's to set): 0 when the two
        // documents are semantically equal, 1 when they differ. The fixed program emits ONE array of change records; an
        // empty array is the equality law (emit nothing on equality) made observable at the same boundary `-e` reads.
        // This branch is unreachable under `-e` (the combination is a usage error), and usage/runtime failures keep
        // their own classes.
        match exit_facts {
            Some((_, _, Some(false))) => 1,
            _ => 0,
        }
    } else {
        0
    };
    //: under `--strictness strict`, any promotable warning forces
    // the failure class (exit 5) even when the run otherwise succeeded — the record route's "one error-severity issue
    // forces the class" law generalized. The FATAL set and halt are exempt by construction.
    if resources.strictness() == jqf_resource::policy::StrictnessPolicy::Strict && resources.strictness_warnings() > 0 {
        return Err(CliFailure::Reported);
    }
    Ok(success_code)
}
