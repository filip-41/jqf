//! The C ABI for jqf's SDK: run a program over bytes, read the structured
//! diagnostic records back. Built as a cdylib for language bindings (ctypes
//! in Python, whatever the host speaks).
//!
//! Ownership rules (no pointers ever escape into the engine):
//! - `jqf_run` / `jqf_run_sequence` write up to `out_cap` published bytes into
//!   a caller-provided buffer and return the REQUIRED byte count as an
//!   `int64_t` (or `-1` on failure) — the `snprintf` convention. When the
//!   required count is `<= out_cap`, the buffer holds the complete output.
//!   When it is GREATER than `out_cap`, only the first `out_cap` bytes were
//!   written and the caller must re-call with a buffer of at least the
//!   required size; the required count is otherwise indistinguishable from
//!   an exact fit, so a caller that skips this check truncates silently. The
//!   count is 64-bit so a published stream larger than 2 GiB stays a
//!   positive answer instead of wrapping onto the failure sentinel. "Failure"
//!   is EVERY error that stops the run without a result — an early setup
//!   failure (a program that fails to parse, an invalid codec) as much as an
//!   uncaught runtime error partway through (`1/0` with no enclosing `try`).
//!   A run publishes INTO the caller's buffer AS it goes (there is no staging
//!   copy), so on a failed run the bytes published before the failure are
//!   already in `out` — and a streaming consumer has already received its
//!   chunks. The `-1` sentinel, never the buffer contents, is what decides
//!   failure; bindings must treat it as authoritative.
//!   A run whose own `try`/`catch` absorbs an error is not a failure: it
//!   completes normally and reports the caught value's required length like
//!   any other success, including `0` for a program that legitimately
//!   publishes nothing (`empty`). The diagnostic stream (below) still carries
//!   every occurrence either way, caught or not.
//! - `jqf_diag_count` / `jqf_diag_get` read the retained record stream of the
//!   LAST operation on this handle ONLY: each call to `jqf_run`/
//!   `jqf_run_sequence`/`jqf_run_compiled`/`jqf_run_sequence_compiled`/
//!   `jqf_compile` clears the handle's diagnostics before it runs, so a
//!   handle reused across operations never accumulates a prior operation's
//!   records. Every field is copied out as a C string or integer, and
//!   `jqf_diag_free_text` releases one copied record.
//! - The handle (`jqf_handle`) is one thread at a time; bindings create one
//!   per consumer.
//!
//! The handle's diagnostics buffer is a stable-address self-reference (leaked
//! once, re-boxed at drop), so the engine-side sink is installed and every
//! record — caught raises, precision boundaries, route, cost, the terminal
//! failure — flows into the same stream a binding polls.
//!
//! The codes module is generated from the same manifest the Rust and Python
//! sides use (`jqf-resource/src/diag/codes.toml`), so a binding's constants
//! and this ABI can never disagree about a code's meaning.
//!
//! # The program handle : compile once, run many
//!
//! `jqf_compile` turns a source program into a `jqf_program` — a `u32` id
//! into the HANDLE's own program table — and `jqf_run_compiled` /
//! `jqf_run_sequence_compiled` run that compiled program over input bytes,
//! skipping the parse/lower/classify half of `jqf_run` entirely. The id is a
//! HANDLE-LOCAL table index, deliberately not a pointer, and it is never
//! reused within the handle's lifetime: the whole surface has no
//! use-after-free or double-free hazard class — an id that names a freed
//! slot, a slot owned by a different handle, or a slot past the table is a
//! DEFINED `-1` (with a recorded setup diagnostic for the run entry points),
//! never undefined behavior. Programs cannot outlive their engine: `jqf_free`
//! drops every live program with it, releasing each compiled arena's ledger
//! residency while the request account is still alive. Ids grow
//! monotonically and are never reused. A compile/free loop reclaims each
//! freed slot on the spot (the table is a map, not a growing `Vec` of
//! emptied slots). A NULL handle or required out-parameter is a defined
//! `-1`; `jqf_new` writes `NULL` into `*out` before any failure return.
//!
//! The codec context — the registration, the catalog, the format and dialect
//! ids — is constructed ONCE in `jqf_new` and reused by every run, instead of
//! being rebuilt inside every call. The run path lowers the eager
//! whole-document requirement per run; a compiled slot retains the program
//! only.
//!
//! # Scope
//!
//! Bindings, input source, environment, encode options, and the
//! input-sequence route are in scope. Exotic CLI routes stay out.
//!
//! Every run entry point drives adjacent JSON values with the shared input
//! cursor, so `[inputs]` and `input` answer as they do under the CLI.
//! `jqf_new` installs the host process snapshot so `$ENV` / `env` answer
//! the host's variables. `jqf_compile_args` passes host data as `$name`
//! compile-time constants — JSON values, never spliced into the source.
//! `$ARGS` resolves on every compile. `jqf_new_limited` accepts encode
//! options applied to every run on the handle.
//!
//! The handle's run catalog is strict JSON (`json/rfc8259`). The resident
//! feed installs a separate record catalog (JSON, NDJSON, json-seq, CSV,
//! TSV, render, YAML, XML, HTML).
//!
//! # Diagnostics
//!
//! A `halt_error` retains `RAISE_HALT` with the message payload and
//! `halt_status`. A raised non-string retains compact JSON as the
//! `RAISE_PROGRAM` payload. `jqf_diag_get` surfaces locators. Per-value
//! sequence errors are length-carrying through `jqf_run_errors_count` /
//! `jqf_run_error_get` so an interior NUL survives. `jqf_diag_get` C-string
//! out-params drop interior NULs (a NULL pointer); `jqf_diag_get_text`
//! keeps them.
//!
//! # ABI
//!
//! Bindings check `jqf_abi_version()` at load and refuse a mismatch.
//! `make ffi-header-lint` fails when the checked-in header drifts from
//! these signatures. `jqf_new_limited` accepts ceilings, a deadline, and a
//! host control callback. Callbacks run on the request thread and must not
//! call any `jqf_*` on this handle. `(NULL, 0)` input and output are
//! defined. Programs carry a length; an embedded NUL is `MACHINE_SETUP`.
//! Every handle lives on a request thread sized by
//! [`jqf_sdk::request_stack_bytes`]; ABI calls hop there because
//! `ResourceContext` is `!Send`.
//!
//! Every failure of `jqf_compile` records a `MACHINE_SETUP` diagnostic before
//! returning `-1`, exactly like `jqf_run`'s setup failures: a caller reading
//! the sentinel off a freshly cleared, empty stream cannot tell a syntax
//! error from a transport bug without it. The compiled run entry points keep
//! the same law for an id that names no live program — that case is a caller
//! bug, and the stream says so.
//!
//! # The resident feed : the record route, incrementally
//!
//! `jqf_feed_open`/`jqf_feed_push`/`jqf_feed_poll`/`jqf_feed_finish`/
//! `jqf_feed_close` run ONE
//! compiled program over ONE framed record stream whose input arrives in
//! pieces. `push` appends input and frames complete records (a record is
//! complete only after its physical terminator; everything after the last
//! line feed is held); `poll` runs the program's compiled requirement over
//! ONE batch of the completed range through the record route's own drive and
//! publishes the batch into a caller buffer with the same `snprintf`
//! convention and the same `-1` sentinel; `finish` marks end of stream so
//! the held tail is delivered as the FINAL record under the profile's own
//! tail law (a clean drain to 0, never -1). The feed id is a handle-LOCAL `u32`
//! table id with the same never-reused, every-misuse-is-a-defined-`-1` law as
//! the program table; the feed selects its strict/recovering profile at open;
//! its diagnostics are the handle's stream cleared per poll; a strict fault
//! is the terminal `-1` with the faulting record retained; and a feed can
//! never outlive its engine — `jqf_free` drops every live feed, releasing its
//! retained-input residency while the account is still alive. See the design
//! note for the
//! pull-buffer ruling and the batch bound (`RECORD_BATCH_TARGET_BYTES`, the
//! record route's own number, not a new one).

use core::cell::Cell;
use core::ffi::{c_char, c_int, c_uint, c_void};
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

thread_local! {
    /// Set on the request thread while a `Work` closure runs. `hop` refuses
    /// nested ABI calls so a host callback cannot deadlock the rendezvous.
    static IN_REQUEST: Cell<bool> = const { Cell::new(false) };
}

use jqf_codec_core::{
    AccessRequirement, CodecRegistration, DecodeRequest, DiagnosticPolicy, PreservationRequest, ValidationMode,
};
use jqf_codec_json::ndjson::NdjsonProfile;
use jqf_codec_json::{JsonEncodeOptions, JsonIndent};
use jqf_data::{DialectId, FormatId, Value};
use jqf_engine::{CodecRequirementPolicy, CompileOptions, CompiledProgram, try_compile_program};
use jqf_resource::{
    Control, ControlOutcome, EnvironmentSnapshot, RequestAccount, ResourceContext, ResourceLimits, WorkMeter,
};
use jqf_runtime::feed::{FeedPoll, ResidentFeed};
use jqf_sdk::{CodecCatalog, Diagnostics, FacadeFraming, ItemSink, PipelineFailure, PipelinePolicy};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

/// The ABI version: bumped whenever an entry point's signature, meaning, or
/// the record-shape contract changes. Bindings check it at load
/// (`jqf_abi_version`) and refuse a mismatch — a signature drift on a stale
/// library is a memory corruption, not a graceful failure .
///
/// Version 2: program params carry explicit lengths and reject
/// embedded NULs; the run entry points drive the input-sequence route;
/// `jqf_compile_args` adds bindings; `jqf_new_limited` adds limits/control
/// and encode options; the sequence entry points' `out_errors` out-param is
/// replaced by `jqf_run_errors_count`/`jqf_run_error_get`; `jqf_diag_get`
/// gains the locator and halt-status out-params; `jqf_abi_version` itself.
pub const JQF_ABI_VERSION: u32 = 2;

/// Compiles the README examples as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;

/// The host control callback's return verdicts ([`JqfLimits::control_callback`]).
pub const JQF_CONTROL_CONTINUE: c_int = 0;
/// The host cancelled the request.
pub const JQF_CONTROL_CANCELLED: c_int = 1;
/// The host reports its deadline expired.
pub const JQF_CONTROL_DEADLINE: c_int = 2;
/// The host reports its physical-memory ceiling exceeded.
pub const JQF_CONTROL_MEMORY: c_int = 3;

/// The compile policy every run uses: strict validation, errors-only
/// diagnostics, no explicit container-span frontier (the engine's own route
/// decision).
const POLICY: CodecRequirementPolicy =
    CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);

/// One compiled program retained on a handle. The codec access requirement
/// is NOT retained: the run path lowers the eager whole-document requirement
/// per run (the input-sequence drive's law), and the record drive lowers its
/// own — retaining one pushdown-shaped requirement for nothing would be dead
/// state.
struct ProgramSlot {
    compiled: CompiledProgram,
}

/// One resident streaming feed retained on a handle : the record
/// route fed incrementally — push input in pieces, poll bounded batches of
/// output. The feed runs the compiled program its `program_id` names; every
/// poll re-validates that id against this handle's own table, so a program
/// freed under a live feed is a DEFINED poll failure, never a dangling
/// dereference.
struct FeedSlot {
    program_id: u32,
    feed: ResidentFeed,
}

/// The limits and cancellation contract one handle runs under .
///
/// Every numeric ceiling is the VALUE the request ledger charges against; a
/// dimension the caller does not want bounded is `u64::MAX` (`u32::MAX` for
/// the depth), exactly as `jqf_new`'s defaults are. `deadline_ms` counts from
/// the [`jqf_new_limited`] call and bounds every run on the handle; `0`
/// means no deadline. `control_callback` is called with `control_context`
/// from the engine's cooperative checkpoints on this handle's request thread.
/// It must not call any `jqf_*` on this handle. It returns one of the
/// `JQF_CONTROL_*` verdicts; an unknown
/// verdict is treated as continue. `NULL` for the whole struct means
/// "everything unlimited, no deadline, no callback" (the [`jqf_new`]
/// defaults).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct JqfLimits {
    /// The request's output ceiling in bytes (`u64::MAX` = unlimited).
    pub max_output_bytes: u64,
    /// The request's ledger memory ceiling in bytes (`u64::MAX` = unlimited).
    pub max_memory_bytes: u64,
    /// The request's spill-store ceiling in bytes (`u64::MAX` = unlimited).
    pub max_spill_bytes: u64,
    /// The request's nesting-depth ceiling (`u32::MAX` = unlimited; the CLI's
    /// standing ceiling is 10 000).
    pub max_nesting_depth: u32,
    /// The handle's deadline in milliseconds from construction; `0` = none.
    pub deadline_ms: u64,
    /// The host control callback, invoked at the engine's cooperative
    /// checkpoints; `NULL` = no callback.
    pub control_callback: Option<unsafe extern "C" fn(*mut c_void) -> c_int>,
    /// The opaque context passed to `control_callback`; must stay valid for
    /// the handle's lifetime (the host's contract).
    pub control_context: *mut c_void,
}

/// The JSON output formatting every run on a handle uses (.
///
/// Mirrors the CLI's JSON-only formatting flags; `NULL` for the whole struct
/// means compact output (the defaults `jqf_new` has always used).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct JqfEncodeOptions {
    /// `-1` compact (default), `-2` tab indentation, `0..=7` spaces per open
    /// container. Any other negative is compact; a value above 7 clamps to 7.
    pub indent: c_int,
    /// the reference's `-r`/`--raw-output`: a ROOT string item is written without quotes
    /// or escapes; every other item normally.
    pub raw_strings: u8,
    /// the reference's `-S`/`--sort-keys`: every object's member keys emitted in
    /// ascending byte order, recursively.
    pub sort_keys: u8,
    /// the reference's `-a`/`--ascii-output`: every character at or above `0x80` escaped.
    pub ascii_output: u8,
    /// the reference's `--raw-output0`: every item terminated with a NUL byte instead of
    /// a newline (and a root string written raw is rejected rather than
    /// silently mangled).
    pub raw_output_nul: u8,
}

impl Default for JqfEncodeOptions {
    fn default() -> Self {
        Self {
            indent: -1,
            raw_strings: 0,
            sort_keys: 0,
            ascii_output: 0,
            raw_output_nul: 0,
        }
    }
}

/// The per-handle cancellation/deadline observation .
///
/// The host callback is invoked from the engine's cooperative checkpoints on
/// the request thread; the deadline is a monotonic `Instant` the check
/// compares against. Both are host-side observations carried opaquely, the
/// same contract `ContinueControl` documents.
struct HostControl {
    deadline: Option<Instant>,
    callback: Option<unsafe extern "C" fn(*mut c_void) -> c_int>,
    context: *mut c_void,
}

impl Control for HostControl {
    fn check(&self) -> ControlOutcome {
        // The fast path: a handle with neither a callback nor a deadline must
        // not pay an `Instant::now()` per cooperative checkpoint.
        if self.callback.is_none() && self.deadline.is_none() {
            return ControlOutcome::Continue;
        }
        if let Some(callback) = self.callback {
            // SAFETY: the host's contract for `control_callback`/
            // `control_context` (documented on [`JqfLimits`]) — the context
            // stays valid and the callback reentrancy-safe for this handle's
            // lifetime, and the engine calls `check` from the request thread
            // at cooperative checkpoints. The verdict int is mapped below;
            // an unknown verdict is treated as continue (documented).
            let verdict = unsafe { callback(self.context) };
            // An unknown verdict is treated as continue (documented).
            match verdict {
                JQF_CONTROL_CANCELLED => return ControlOutcome::Cancelled,
                JQF_CONTROL_DEADLINE => return ControlOutcome::DeadlineExceeded,
                JQF_CONTROL_MEMORY => return ControlOutcome::MemoryExceeded,
                _ => {}
            }
        }
        if let Some(deadline) = self.deadline
            && Instant::now() >= deadline
        {
            return ControlOutcome::DeadlineExceeded;
        }
        ControlOutcome::Continue
    }
}

/// The host-state snapshot the process builtins read (`$ENV`, `env`), built
/// from the FFI process exactly as the CLI builds its own: the environment,
/// the working directory, and the reference's literal default search list. Without it
/// the host builtins answer their empty forms (`env` → `{}`).
fn host_environment() -> EnvironmentSnapshot {
    let vars = std::env::vars().collect();
    let cwd = std::env::current_dir()
        .ok()
        .map(|path| path.to_string_lossy().into_owned());
    let jq_origin = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|parent| parent.to_string_lossy().into_owned()));
    let search_list = ["~/.jq", "$ORIGIN/../lib/jq", "$ORIGIN/../lib"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    EnvironmentSnapshot::new(vars, cwd, search_list, jq_origin)
}

/// One run handle: owns the request ledger, the diagnostic stream, the codec
/// context, the program table, the last run's outcome, and the host control.
/// Not Sync — one consumer at a time.
///
/// The diagnostics buffer, the codec registrations, and the host control are
/// deliberately LEAKED once at construction and re-boxed at drop: the request
/// context borrows its sink and control with `'static` lifetimes (both must
/// outlive the context), the catalog borrows a stable-address registration
/// slice, and this handle is the only owner of all three, so each leak is a
/// stable-address self-reference — the engine-side records (a raise a `try`
/// absorbed, precision boundaries) flow through it, the control is observed,
/// and the drop releases each exactly once.
///
/// Field order is load-bearing: `programs` is declared FIRST so its slots'
/// ledger residencies release while the request account is still fully alive;
/// `feeds` is second for the same reason (a feed's retained-input residency
/// must also release before the account dies — the cannot-outlive-the-engine
/// law the program table has).
pub struct JqfHandle {
    /// The program table, keyed by the monotone `u32` ids the ABI hands out.
    /// A freed id is removed so its slot memory is released; ids are never
    /// reused, so a stale id cannot alias a later program.
    programs: HashMap<u32, ProgramSlot>,
    next_program_id: u32,
    /// The feed table, same law as the program table.
    feeds: HashMap<u32, FeedSlot>,
    next_feed_id: u32,
    /// Set when a caught panic may have torn the handle; every later entry
    /// returns `-1` rather than touching possibly-torn fields.
    poisoned: bool,
    resources: ResourceContext<'static>,
    diagnostics: &'static Diagnostics,
    diagnostics_owner: *mut Diagnostics,
    /// The codec registration's owner pointer — the registration itself is
    /// leaked once so the catalog's slice borrow has a stable address, and
    /// re-boxed exactly once at drop.
    registration_owner: *mut CodecRegistration<'static>,
    /// The one-element registration slice's owner pointer, same law.
    registrations_owner: *mut [&'static CodecRegistration<'static>; 1],
    /// The host control's owner pointer, same leak/re-box law : the
    /// request context borrows it with a `'static` lifetime.
    control_owner: *mut HostControl,
    /// The codec catalog, built once at [`jqf_new`] (it was rebuilt
    /// per run for no reason).
    catalog: CodecCatalog<'static, 'static>,
    /// The process-lifetime record catalog the resident feed's drives serve,
    /// installed once through
    /// [`jqf_runtime::records::install_record_catalog`].
    record_catalog: CodecCatalog<'static, 'static>,
    format: FormatId,
    dialect: DialectId,
    /// The JSON output formatting every run on this handle uses.
    encode_options: JsonEncodeOptions,
    /// The per-value sequence errors of the LAST run, one entry per reported
    /// error, retained exactly like the diagnostic records : the
    /// error TEXT, which may itself contain NUL bytes — read back through
    /// the length-carrying `jqf_run_error_get`, never a delimiter-joined C
    /// string.
    errors: Vec<String>,
    /// The last run's terminal halt status (`halt_error`'s `n`, or `0` for a
    /// bare `halt`), `None` when the last run did not halt . Read by
    /// `jqf_diag_get` for a `RAISE_HALT` record only.
    halt_status: Option<u32>,
}

impl Drop for JqfHandle {
    fn drop(&mut self) {
        // SAFETY: each owner pointer is what `jqf_new`/`jqf_new_limited`
        // took from its own `Box::leak`; this handle is that box's only
        // owner and drop runs exactly once, so the re-box neither
        // double-frees nor aliases. The `control_owner` re-box happens while
        // `resources` is still alive (the Drop body runs before the field
        // drops) — exactly the pattern the diagnostics and registration
        // re-boxes use.
        unsafe {
            drop(Box::from_raw(self.diagnostics_owner));
            drop(Box::from_raw(self.registration_owner));
            drop(Box::from_raw(self.registrations_owner));
            drop(Box::from_raw(self.control_owner));
        }
    }
}

/// The streaming entry point's chunk granularity: one sealed buffer per
/// callback delivery. Bounded on purpose — a consumer that forwards each
/// chunk as it arrives never holds more than this plus one encoded value,
/// which is the entire point of [`jqf_run_sequence_streaming`] (the staged
/// legacy path peaks at the WHOLE output; measured 608 MB against the feed
/// twin's 26 MB on a ~200 MB-output program). Mirrored by bindings
/// (`JQF_STREAM_CHUNK_BYTES` in `_ffi.py`).
pub const JQF_STREAM_CHUNK_BYTES: usize = 256 * 1024;

/// The host chunk consumer's verdicts ([`jqf_run_sequence_streaming`]).
pub const JQF_STREAM_CONTINUE: c_int = 0;
/// The host wants the run stopped: publication stops cleanly and the entry
/// reports `-1` with a cancellation record on the diagnostic stream.
pub const JQF_STREAM_CANCEL: c_int = 1;

/// The streaming entry point's chunk callback: `(context, bytes, len)`.
/// `bytes` is valid ONLY for the duration of the call — the consumer copies
/// or forwards before returning. A nonzero verdict stops the run. Must not
/// call any `jqf_*` on this handle.
pub type JqfStreamChunkFn = unsafe extern "C" fn(*mut c_void, *const u8, usize) -> c_int;

/// The input-sequence route's COUNTING sink: publishes into the CALLER's
/// buffer whatever fits and merely counts the overflow. The `snprintf`
/// convention makes this legal — the required count must be exact, but the
/// bytes beyond a too-small buffer are re-produced by the re-call that the
/// required count itself commands — and it removes the staging `Vec` and its
/// doubling transient entirely: a run's resident output footprint is the
/// caller's own buffer, not a second copy of the whole output. Per-value
/// errors ride out of band, retained as their reference error TEXTS (each
/// may contain NUL bytes; the length-carrying getter, not a joined C
/// string, is how they come back).
struct CountingSink<'a> {
    out: &'a mut [u8],
    /// Bytes copied into `out` so far (never more than `out.len()`).
    copied: usize,
    /// The REQUIRED byte count: every byte seen, copied or not.
    total: u64,
    errors: &'a mut Vec<String>,
}

impl ItemSink for CountingSink<'_> {
    type Error = &'static str;
    fn begin_item(&mut self, _index: u64) -> Result<(), Self::Error> {
        Ok(())
    }
    fn write(&mut self, bytes: &[u8]) -> Result<usize, Self::Error> {
        let fit = bytes.len().min(self.out.len() - self.copied);
        if fit > 0 {
            self.out[self.copied..self.copied + fit].copy_from_slice(&bytes[..fit]);
            self.copied += fit;
        }
        self.total += bytes.len() as u64;
        Ok(bytes.len())
    }
    fn finish_item(&mut self, _index: u64, _report: jqf_sdk::EncodedItemReport) -> Result<(), Self::Error> {
        Ok(())
    }
    fn report_value_error(&mut self, error: jqf_sdk::SequenceValueError) -> Result<(), Self::Error> {
        self.errors.push(error.message().to_owned());
        Ok(())
    }
}

/// The streaming entry point's sink: hands every published byte to the
/// host's chunk callback in bounded deliveries, never staging the whole
/// output. Sealed buffers carry up to `chunk_cap` bytes (the entry point
/// passes [`JQF_STREAM_CHUNK_BYTES`]); a single write LARGER than a fresh
/// buffer is delivered directly from the encoder's slice rather than
/// buffered — either way the sink never allocates past its committed bound,
/// so a consumer's footprint stays bounded even when ONE value is huge.
/// Values may straddle callbacks: chunks are FLOW units, not frame
/// boundaries (the output stream itself carries the value framing).
/// Per-value errors ride out of band exactly as the counting sink keeps
/// them.
struct StreamingSink<'a> {
    errors: &'a mut Vec<String>,
    /// The host callback; validated non-NULL at the entry point.
    chunk: JqfStreamChunkFn,
    context: *mut c_void,
    buf: Vec<u8>,
    /// This sink's chunk bound: sealed buffers seal at exactly this many
    /// bytes. A field, not a constant read, so tests can drive arbitrary
    /// boundary sizes (including 1) through the same sealing law.
    chunk_cap: usize,
    delivered: u64,
    cancelled: bool,
}

impl StreamingSink<'_> {
    /// Delivers one slice through the callback. A nonzero verdict marks the
    /// run cancelled (publication stops; the entry point records why).
    ///
    /// SAFETY: the callback contract (documented on
    /// [`jqf_run_sequence_streaming`]) requires `context` to be valid and
    /// `bytes` treated as read-only for the duration of the call; the
    /// pointer handed over aliases this sink's own buffer only between
    /// callback entry and return, and the sink does not touch it in that
    /// window.
    fn deliver(&mut self, bytes: &[u8]) -> Result<(), &'static str> {
        // SAFETY: `chunk` and `context` come from the entry point's `#
        // Safety` contract; `bytes` is a valid slice for `bytes.len()` bytes.
        let verdict = unsafe { (self.chunk)(self.context, bytes.as_ptr(), bytes.len()) };
        if verdict == JQF_STREAM_CANCEL {
            self.cancelled = true;
            return Err("stream cancelled by host");
        }
        // An unknown verdict is continue, the same law the control
        // callback keeps.
        self.delivered += bytes.len() as u64;
        Ok(())
    }

    /// Delivers the open partial buffer at the end of the run: a chunk
    /// boundary falls wherever the output happens to end, never at the
    /// chunk bound. An empty buffer delivers nothing.
    fn flush(&mut self) -> Result<(), &'static str> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let mut open = std::mem::take(&mut self.buf);
        self.deliver(&open)?;
        open.clear();
        self.buf = open;
        Ok(())
    }
}

impl ItemSink for StreamingSink<'_> {
    type Error = &'static str;
    fn begin_item(&mut self, _index: u64) -> Result<(), Self::Error> {
        Ok(())
    }
    fn write(&mut self, mut bytes: &[u8]) -> Result<usize, Self::Error> {
        if self.cancelled {
            return Err("stream cancelled by host");
        }
        let total = bytes.len();
        // A write at least a fresh buffer's size — one huge encoded value —
        // is delivered directly when no partial chunk is open so this sink
        // never grows past its committed bound. Values may still straddle
        // callbacks (see the struct doc).
        if !bytes.is_empty() && self.buf.is_empty() && bytes.len() >= self.chunk_cap {
            self.deliver(bytes)?;
            return Ok(total);
        }
        while !bytes.is_empty() {
            let room = self.chunk_cap - self.buf.len();
            let take = bytes.len().min(room);
            // Extending cannot reallocate: `take <= room` is spare capacity.
            self.buf.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.buf.len() == self.chunk_cap {
                let mut sealed = std::mem::take(&mut self.buf);
                self.deliver(&sealed)?;
                sealed.clear();
                self.buf = sealed;
            }
        }
        Ok(total)
    }
    fn finish_item(&mut self, _index: u64, _report: jqf_sdk::EncodedItemReport) -> Result<(), Self::Error> {
        Ok(())
    }
    fn report_value_error(&mut self, error: jqf_sdk::SequenceValueError) -> Result<(), Self::Error> {
        self.errors.push(error.message().to_owned());
        Ok(())
    }
}

/// Reads `len` bytes from `ptr`. A `(NULL, 0)` pair is the documented empty
/// input : accepted as an empty slice, NEVER `from_raw_parts` on a
/// null pointer. Any other combination is the caller's `# Safety` contract
/// violation and is not validated here.
fn read_input<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    if ptr.is_null() {
        &[]
    } else {
        // SAFETY: every entry point that passes here documents `ptr` as
        // valid for `len` bytes for the duration of the call.
        unsafe { core::slice::from_raw_parts(ptr, len) }
    }
}

/// The writable view of a caller buffer, or `&mut []` for `(NULL, 0)` — the
/// documented sizing probe : `jqf_feed_poll(h, id, NULL, 0)` must
/// return the required length, never dereference a null pointer.
fn write_output<'a>(ptr: *mut u8, cap: usize) -> &'a mut [u8] {
    if ptr.is_null() || cap == 0 {
        &mut []
    } else {
        // SAFETY: every entry point that passes here documents `ptr` as
        // valid for `cap` bytes for the duration of the call.
        unsafe { core::slice::from_raw_parts_mut(ptr, cap) }
    }
}

/// Publishes `bytes` into a caller buffer under the `snprintf` convention:
/// copies at most `out_cap` bytes (never into a NULL buffer) and returns the
/// REQUIRED count. The caller detects truncation by comparing the returned
/// count against `out_cap`.
fn publish(bytes: &[u8], out: *mut u8, out_cap: usize) -> i64 {
    let written = bytes.len().min(out_cap);
    if !out.is_null() && written > 0 {
        // SAFETY: `written <= out_cap`, which the caller declares as `out`'s
        // writable capacity; `bytes` is the handle's own `Vec`, so the ranges
        // cannot overlap.
        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), out, written) };
    }
    byte_count(bytes.len())
}

/// A raw pointer that may cross the hop. The hop is join-before-return, so
/// the pointed-to bytes stay valid for the whole job. Closures must capture
/// the wrapper (via [`SendPtr::get`]), never the inner raw pointer.
#[derive(Clone, Copy)]
struct SendPtr<T>(*mut T);
unsafe impl<T> Send for SendPtr<T> {}
impl<T> SendPtr<T> {
    fn get(self) -> *mut T {
        self.0
    }
}

#[derive(Clone, Copy)]
struct SendConstPtr<T>(*const T);
unsafe impl<T> Send for SendConstPtr<T> {}
impl<T> SendConstPtr<T> {
    fn get(self) -> *const T {
        self.0
    }
}

/// Host limits stripped of `!Send` raw pointers so they can travel to the
/// request thread. The control context's lifetime is the host's contract
/// (documented on [`JqfLimits`]).
struct SendHostLimits {
    max_output_bytes: u64,
    max_memory_bytes: u64,
    max_spill_bytes: u64,
    max_nesting_depth: u32,
    deadline_ms: u64,
    control_callback: Option<unsafe extern "C" fn(*mut c_void) -> c_int>,
    control_context: SendPtr<c_void>,
}

enum SessionCommand {
    Work(Box<dyn FnOnce(&mut JqfHandle) + Send>),
    Shutdown,
}

/// The C-facing handle: a channel to the request thread that owns
/// [`JqfHandle`]. `ResourceContext` is `!Send`, so the handle is created on
/// that thread and every ABI call hops there. ABI signatures stay the same.
struct JqfSession {
    tx: Sender<SessionCommand>,
    join: Option<JoinHandle<()>>,
}

fn session_ref<'a>(ptr: *mut c_void) -> Option<&'a JqfSession> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: non-null; every ABI entry treats this as a `jqf_new` session
    // or rejects it. The session outlives the hop (join-before-return).
    Some(unsafe { &*ptr.cast::<JqfSession>() })
}

/// Runs `work` on the request thread that owns the handle. `None` for NULL,
/// a dead session, a poisoned handle, or a panic on that thread.
fn hop<T: Send + 'static>(ptr: *mut c_void, work: impl FnOnce(&mut JqfHandle) -> T + Send + 'static) -> Option<T> {
    if IN_REQUEST.get() {
        return None;
    }
    let session = session_ref(ptr)?;
    let (reply_tx, reply_rx) = mpsc::sync_channel(0);
    session
        .tx
        .send(SessionCommand::Work(Box::new(move |handle| {
            if handle.poisoned {
                handle.diagnostics.clear();
                handle.diagnostics.record_setup_failure("handle poisoned");
                let _ = reply_tx.send(None);
                return;
            }
            if let Ok(value) = catch_unwind(AssertUnwindSafe(|| work(handle))) {
                let _ = reply_tx.send(Some(value));
            } else {
                handle.poisoned = true;
                let _ = reply_tx.send(None);
            }
        })))
        .ok()?;
    reply_rx.recv().ok().flatten()
}

fn hop_or<T: Send + 'static>(
    ptr: *mut c_void,
    fallback: T,
    work: impl FnOnce(&mut JqfHandle) -> T + Send + 'static,
) -> T {
    hop(ptr, work).unwrap_or(fallback)
}

fn request_loop(handle: &mut JqfHandle, rx: &Receiver<SessionCommand>) {
    while let Ok(cmd) = rx.recv() {
        match cmd {
            SessionCommand::Work(work) => {
                IN_REQUEST.set(true);
                work(handle);
                IN_REQUEST.set(false);
            }
            SessionCommand::Shutdown => break,
        }
    }
}

/// Kind / operand / payload field selectors for [`jqf_diag_get_text`].
pub const JQF_DIAG_TEXT_KIND: c_int = 0;
/// Operand field selector for [`jqf_diag_get_text`].
pub const JQF_DIAG_TEXT_OPERAND: c_int = 1;
/// Payload field selector for [`jqf_diag_get_text`].
pub const JQF_DIAG_TEXT_PAYLOAD: c_int = 2;

/// Creates a run handle (one ledger, one diagnostic stream, one codec
/// context, one program table) with unlimited resources and compact output —
/// equivalent to
/// `jqf_new_limited(NULL, NULL, out)`.
///
/// # Safety
///
/// `out` must be a valid `*mut *mut JqfHandle`; the caller owns the returned
/// handle and must release it with [`jqf_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jqf_new(out: *mut *mut c_void) -> c_int {
    // SAFETY: this function's `# Safety` contract requires `out` to be a
    // valid, aligned, writable slot for one handle pointer.
    unsafe { jqf_new_limited(core::ptr::null(), core::ptr::null(), out) }
}

/// Creates a run handle under an explicit resource policy (`limits`) and
/// host control, with the handle's JSON output formatting options
/// (`encode`).
/// `limits`/`encode` of `NULL` mean the defaults ([`jqf_new`]).
///
/// The deadline (`JqfLimits::deadline_ms`) starts here and bounds every run
/// on the handle: a program that exceeds it stops with a machine
/// `MACHINE_DEADLINE` record, and the engine's cooperative checkpoints
/// observe it even in a tight generator loop (`repeat(.)`).
///
/// # Safety
///
/// `limits`/`encode`, when non-null, must point at initialized
/// [`JqfLimits`]/[`JqfEncodeOptions`]; `out` must be a valid
/// `*mut *mut JqfHandle`; the caller owns the returned handle and must
/// release it with [`jqf_free`]; `limits.control_context` must stay valid
/// for the handle's lifetime and `limits.control_callback` must be safe to
/// call from the request thread at the engine's cooperative checkpoints.
///
/// # Panics
///
/// Only if a static codec registration or format id cannot be constructed.
/// Those are compile-time constants, so this is unreachable from host input.
/// A refused limit or a null `out` returns −1 and never panics.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jqf_new_limited(
    limits: *const JqfLimits,
    encode: *const JqfEncodeOptions,
    out: *mut *mut c_void,
) -> c_int {
    if out.is_null() {
        return -1;
    }
    // A failure must leave a defined NULL, never the caller's leftover bits.
    unsafe { *out = core::ptr::null_mut() };
    let result = catch_unwind(AssertUnwindSafe(|| spawn_session(limits, encode)));
    match result {
        Ok(Ok(session)) => {
            unsafe { *out = session.cast::<c_void>() };
            0
        }
        Ok(Err(())) | Err(_) => -1,
    }
}

/// Reads the ABI limits/encode on the caller thread, then spawns the
/// request thread and builds [`JqfHandle`] there. Only the session pointer
/// crosses back.
fn spawn_session(limits: *const JqfLimits, encode: *const JqfEncodeOptions) -> Result<*mut JqfSession, ()> {
    // SAFETY: `jqf_new_limited`'s `# Safety` contract requires `limits`/
    // `encode` to be initialized when non-null; a null pointer reads the
    // defaults. Fields are copied here because `JqfLimits` is `!Send`
    // (`control_context` is a raw pointer).
    let limits = unsafe { limits.as_ref() }.copied().unwrap_or(JqfLimits {
        max_output_bytes: u64::MAX,
        max_memory_bytes: u64::MAX,
        max_spill_bytes: u64::MAX,
        max_nesting_depth: u32::MAX,
        deadline_ms: 0,
        control_callback: None,
        control_context: core::ptr::null_mut(),
    });
    let encode = unsafe { encode.as_ref() }.copied().unwrap_or_default();
    let send_limits = SendHostLimits {
        max_output_bytes: limits.max_output_bytes,
        max_memory_bytes: limits.max_memory_bytes,
        max_spill_bytes: limits.max_spill_bytes,
        max_nesting_depth: limits.max_nesting_depth,
        deadline_ms: limits.deadline_ms,
        control_callback: limits.control_callback,
        control_context: SendPtr(limits.control_context),
    };
    let stack = match jqf_sdk::request_stack_bytes() {
        Ok(bytes) => bytes,
        Err(message) => {
            eprintln!("jqf: {message}");
            return Err(());
        }
    };
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::sync_channel(0);
    let join = thread::Builder::new()
        .name("jqf-request".into())
        .stack_size(stack)
        .spawn(move || match build_handle(&send_limits, encode) {
            Ok(mut handle) => {
                let _ = ready_tx.send(Ok(()));
                request_loop(&mut handle, &cmd_rx);
            }
            Err(()) => {
                let _ = ready_tx.send(Err(()));
            }
        })
        .map_err(|_| ())?;
    if let Ok(Ok(())) = ready_rx.recv() {
        Ok(Box::into_raw(Box::new(JqfSession {
            tx: cmd_tx,
            join: Some(join),
        })))
    } else {
        let _ = join.join();
        Err(())
    }
}

fn build_handle(limits: &SendHostLimits, encode: JqfEncodeOptions) -> Result<JqfHandle, ()> {
    let account = RequestAccount::try_new(ResourceLimits::new(
        // No input ceiling: the retained input is what every route
        // borrows (the CLI's own law for the same reason).
        u64::MAX,
        limits.max_output_bytes,
        limits.max_memory_bytes,
        limits.max_spill_bytes,
        limits.max_nesting_depth,
    ))
    .map_err(|_| ())?;
    let work = WorkMeter::try_new_v1(64).ok_or(())?;
    // The deadline counts from construction and bounds every run on the
    // handle . An overflowing deadline is treated as none —
    // unreachable in practice, defined in case.
    let deadline = (limits.deadline_ms != 0)
        .then(|| Instant::now().checked_add(Duration::from_millis(limits.deadline_ms)))
        .flatten();
    let control = Box::new(HostControl {
        deadline,
        callback: limits.control_callback,
        context: limits.control_context.0,
    });
    let control: &'static HostControl = Box::leak(control);
    let control_owner = core::ptr::from_ref::<HostControl>(control).cast_mut();
    let Ok(resources) = ResourceContext::new(account, control, work) else {
        // Reclaim the leaked control: a refused limit must not leave a
        // HostControl behind.
        drop(unsafe { Box::from_raw(control_owner) });
        return Err(());
    };
    let diagnostics = Box::new(Diagnostics::new(DiagnosticPolicy::All).ok_or(())?);
    let diagnostics: &'static Diagnostics = Box::leak(diagnostics);
    let diagnostics_owner = core::ptr::from_ref::<Diagnostics>(diagnostics).cast_mut();
    let resources = resources
        .with_diagnostics(diagnostics)
        .with_environment(host_environment());
    // The codec context is built ONCE here  — the registration
    // and catalog were rebuilt on every single run before, which is the
    // non-compile half of the per-call cold-start tax.
    let registration = Box::new(jqf_codec_json::registration().expect("the strict-JSON registration is static"));
    let registration: &'static CodecRegistration<'static> = Box::leak(registration);
    let registration_owner = core::ptr::from_ref::<CodecRegistration<'static>>(registration).cast_mut();
    // The array is leaked so the catalog's slice borrow has a stable
    // address; the owner pointer is taken from the array reference
    // BEFORE the coercion to a slice, so the re-box at drop reclaims the
    // exact allocation.
    let registrations_array: &'static [&'static CodecRegistration<'static>; 1] = Box::leak(Box::new([registration]));
    let array_owner = core::ptr::from_ref(registrations_array).cast_mut();
    let registrations: &'static [&'static CodecRegistration<'static>] = registrations_array;
    let catalog = CodecCatalog::new(registrations);
    // The resident feed's record drives need the full record catalog:
    // the feed frames NDJSON through the registered provider
    // and decodes every payload through the JSON ladder, so the host
    // installs the same nine-registration catalog the CLI's drives use.
    let record_catalog = jqf_runtime::records::install_record_catalog(
        jqf_codec_json::registration().expect("the strict-JSON registration is static"),
        jqf_codec_json::ndjson::registration().expect("the NDJSON registration is static"),
        jqf_codec_json::seq::registration().expect("the json-seq registration is static"),
        jqf_codec_delimited::registration().expect("the CSV registration is static"),
        jqf_codec_delimited::registration_tsv().expect("the TSV registration is static"),
        jqf_codec_render::registration().expect("the render registration is static"),
        jqf_codec_yaml::registration().expect("the YAML registration is static"),
        jqf_codec_xml::registration().expect("the XML registration is static"),
        jqf_codec_html::registration().expect("the HTML registration is static"),
    );
    let format = FormatId::try_new(jqf_codec_json::FORMAT_ID).expect("json format id");
    let dialect = DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("rfc8259 dialect id");
    Ok(JqfHandle {
        programs: HashMap::new(),
        next_program_id: 0,
        feeds: HashMap::new(),
        next_feed_id: 0,
        poisoned: false,
        resources,
        diagnostics,
        diagnostics_owner,
        registration_owner,
        registrations_owner: array_owner,
        control_owner,
        catalog,
        record_catalog,
        format,
        dialect,
        encode_options: json_encode_options(encode),
        errors: Vec::new(),
        halt_status: None,
    })
}

/// The handle's JSON encode options from the ABI struct. The indent
/// encoding is `-1` compact, `-2` tabs, `0..=7` spaces. Any other negative
/// is compact (not MAXIMUM); a value above 7 clamps to 7 spaces.
fn json_encode_options(encode: JqfEncodeOptions) -> JsonEncodeOptions {
    let indent = match encode.indent {
        -2 => JsonIndent::Tabs,
        n if n < 0 => JsonIndent::Compact,
        width => JsonIndent::Spaces(u8::try_from(width).unwrap_or(7).min(7)),
    };
    JsonEncodeOptions {
        indent,
        raw_strings: encode.raw_strings != 0,
        sort_keys: encode.sort_keys != 0,
        ascii_output: encode.ascii_output != 0,
        raw_output_nul: encode.raw_output_nul != 0,
    }
}

/// Exports the ABI version . A binding checks this at load and
/// refuses a mismatch: a stale `libjqf_sdk_ffi` with the same symbol names is
/// a signature drift that corrupts memory, and a version check turns that
/// into a clear load error.
#[unsafe(no_mangle)]
pub extern "C" fn jqf_abi_version() -> u32 {
    JQF_ABI_VERSION
}

/// Frees a run handle and every program compiled on it.
///
/// # Safety
///
/// `handle` must be a handle from [`jqf_new`], not already freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jqf_free(handle: *mut c_void) {
    if !handle.is_null() {
        // SAFETY: this function's `# Safety` contract requires `handle` to be
        // a live pointer from `jqf_new`, which produced it with
        // `Box::into_raw` on a `JqfSession` and has not been freed since, so
        // the re-box reclaims that exact allocation once. Shutdown joins the
        // request thread; `JqfHandle` drops there, releasing every live
        // program slot (and each compiled arena's ledger residency) while
        // the account is still alive.
        let _ = catch_unwind(AssertUnwindSafe(|| {
            let mut session = unsafe { Box::from_raw(handle.cast::<JqfSession>()) };
            let _ = session.tx.send(SessionCommand::Shutdown);
            if let Some(join) = session.join.take() {
                let _ = join.join();
            }
        }));
        // A panic must never unwind across the C boundary: the drop chain
        // (program slots, feeds, re-boxed owner pointers, account) runs
        // inside the guard, and a caught panic is discarded — unwinding
        // into a C caller is UB, and a void return has no sentinel to
        // report.
    }
}

/// The common tail of [`jqf_compile`] and [`jqf_compile_args`]: validates the
/// program bytes (never silently truncating at an embedded NUL),
/// binds `$ARGS` (the reference's law: the object binding replaces any user `$ARGS`
/// binding), compiles with the value bindings, lowers the codec requirement
/// once, and pushes the program slot.
///
/// On success writes the `u32` id into `*out_id` and returns 0. On failure
/// returns -1 with exactly one `MACHINE_SETUP` diagnostic retained.
fn compile_with_args(
    handle: &mut JqfHandle,
    program: *const u8,
    program_len: usize,
    value_bindings: &mut Vec<(String, Value)>,
    out_id: *mut u32,
) -> Result<(), String> {
    let diagnostics = handle.diagnostics;
    let setup_failure = |message: String| -> String {
        diagnostics.record_setup_failure(&message);
        message
    };
    // SAFETY: this function's `# Safety` contract requires `program` to be
    // readable for `program_len` bytes; `(NULL, 0)` is the documented empty
    // program .
    let program_bytes = read_input(program, program_len);
    if program_bytes.contains(&0) {
        return Err(setup_failure(
            "program contains a NUL byte; pass the exact program length".to_owned(),
        ));
    }
    let program = std::str::from_utf8(program_bytes).map_err(|_| setup_failure("program is not UTF-8".to_owned()))?;
    // the reference's `$ARGS` builtin, available on EVERY request: `{"positional": [],
    // "named": {…}}`. The FFI binding API is named-only (the positional side
    // has no ABI surface yet), so positional is always empty and the named
    // object is the value bindings — the same shape the CLI builds for
    // `--argjson`-only requests. The reference OVERWRITES a user `$ARGS` binding with
    // this object, so it is inserted last and replaces any `$ARGS` the host
    // named.
    let named = {
        let mut builder = jqf_data::ObjectBuilder::try_with_capacity(value_bindings.len())
            .map_err(|_| setup_failure("cannot allocate $ARGS.named".to_owned()))?;
        for (key, value) in value_bindings.iter() {
            let name = key.strip_prefix('$').unwrap_or(key.as_str());
            let name = jqf_data::ObjectKey::try_from_str(name)
                .map_err(|_| setup_failure("cannot allocate $ARGS.named key".to_owned()))?;
            builder
                .try_insert_last(name, value.clone())
                .map_err(|_| setup_failure("cannot allocate $ARGS.named entry".to_owned()))?;
        }
        builder
            .try_finish()
            .map_err(|_| setup_failure("cannot allocate $ARGS.named".to_owned()))?
    };
    let args_value = build_args_value(named).map_err(setup_failure)?;
    value_bindings.retain(|(key, _)| key != "$ARGS");
    value_bindings.push((String::from("$ARGS"), args_value));
    let id = handle.next_program_id;
    handle.next_program_id = handle
        .next_program_id
        .checked_add(1)
        .ok_or_else(|| setup_failure("program table overflow".to_owned()))?;
    let compiled = try_compile_program(
        program,
        POLICY,
        CompileOptions {
            cli_vars: value_bindings,
            split_exp: false,
            ..Default::default()
        },
        &handle.resources,
    )
    .map_err(|e| setup_failure(format!("{e}")))?;
    // SAFETY: this function's `# Safety` contract requires `out_id` to be
    // a valid, aligned, writable slot for one `u32`.
    unsafe {
        *out_id = id;
    }
    handle.programs.insert(id, ProgramSlot { compiled });
    Ok(())
}

/// Compiles `program` (an explicit `program_len` bytes, never silently
/// truncated at an embedded NUL) into a program id on this handle.
/// On success, writes the `u32` id into `*out_id` and returns 0; the id is a
/// handle-local table index valid until [`jqf_free`] or [`jqf_program_free`],
/// never reused, and usable only with THIS handle. On failure returns -1 and
/// retains exactly one `MACHINE_SETUP` diagnostic explaining why (the compile
/// clears the handle's diagnostic stream first, so the retained stream after
/// a failed compile is exactly the setup record — the same law that keeps
/// `jqf_run`'s `-1` off an empty stream unambiguous).
///
/// `$ARGS` resolves to `{"positional": [], "named": {}}`.
/// [`jqf_compile_args`] supplies real bindings.
///
/// # Safety
///
/// `handle` must be a live pointer from [`jqf_new`]; `program` must be
/// readable for `program_len` bytes and unmutated for the call; `out_id`
/// must be a valid `*mut u32` slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jqf_compile(
    handle: *mut c_void,
    program: *const u8,
    program_len: usize,
    out_id: *mut u32,
) -> c_int {
    if out_id.is_null() {
        return -1;
    }
    let program = SendConstPtr(program);
    let out_id = SendPtr(out_id);
    hop_or(handle, -1, move |handle| {
        // The stream reflects exactly the compile: cleared first, and a
        // failure retains exactly the setup record below.
        handle.diagnostics.clear();
        let mut value_bindings: Vec<(String, Value)> = Vec::new();
        let outcome = compile_with_args(handle, program.get(), program_len, &mut value_bindings, out_id.get());
        match outcome {
            Ok(()) => 0,
            Err(_) => -1,
        }
    })
}

/// Compiles `program` with host data bound as compile-time `$name` constants
/// (the "no string concatenation" law): each name/value pair is a
/// JSON document parsed by the engine's own reader, so host data reaches the
/// program as a VALUE, never spliced into the source. `$ARGS` resolves to
/// `{"positional": [], "named": {name: value, …}}`, exactly the shape
/// the reference builds for the same bindings.
///
/// `names` and `json_values`/`json_value_lengths` are parallel arrays of
/// `binding_count` entries: each name is a NUL-terminated C string; each
/// value is `json_value_lengths[i]` bytes (JSON text) at `json_values[i]` —
/// length-carrying, so a NUL inside a value string survives. The first
/// occurrence of a duplicated name wins (the reference's law). Everything else follows
/// [`jqf_compile`]'s contract (the id, the setup-record law, the NUL
/// rejection of the program itself).
///
/// # Safety
///
/// `handle` must be a live pointer from [`jqf_new`]; `program` must be
/// readable for `program_len` bytes; `names` must point at `binding_count`
/// live NUL-terminated C strings; `json_values`/`json_value_lengths` must
/// point at `binding_count` readable (ptr, len) pairs; `out_id` must be a
/// valid `*mut u32` slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jqf_compile_args(
    handle: *mut c_void,
    program: *const u8,
    program_len: usize,
    binding_count: c_uint,
    names: *const *const c_char,
    json_values: *const *const u8,
    json_value_lengths: *const usize,
    out_id: *mut u32,
) -> c_int {
    if out_id.is_null() {
        return -1;
    }
    if binding_count > 0 && (names.is_null() || json_values.is_null() || json_value_lengths.is_null()) {
        return hop_or(handle, -1, |handle| {
            handle.diagnostics.clear();
            handle.diagnostics.record_setup_failure("binding arrays are NULL");
            -1
        });
    }
    let program = SendConstPtr(program);
    let names = SendConstPtr(names);
    let json_values = SendConstPtr(json_values);
    let json_value_lengths = SendConstPtr(json_value_lengths);
    let out_id = SendPtr(out_id);
    hop_or(handle, -1, move |handle| {
        handle.diagnostics.clear();
        let diagnostics = handle.diagnostics;
        let setup_failure = |message: String| -> String {
            diagnostics.record_setup_failure(&message);
            message
        };
        let mut value_bindings: Vec<(String, Value)> = Vec::new();
        let mut bound_names: Vec<String> = Vec::new();
        for i in 0..binding_count {
            // SAFETY: this function's `# Safety` contract requires `names` to
            // be an array of `binding_count` NUL-terminated strings and
            // `json_values`/`json_value_lengths` arrays of `binding_count`
            // readable (ptr, len) pairs; `i` stays within the declared count.
            let name_ptr = unsafe { *names.get().add(i as usize) };
            if name_ptr.is_null() {
                setup_failure("binding name is NULL".to_owned());
                return -1;
            }
            let Ok(name) = unsafe { CStr::from_ptr(name_ptr) }.to_str() else {
                setup_failure("binding name is not UTF-8".to_owned());
                return -1;
            };
            let key = format!("${name}");
            // the reference's duplicate-name law: the FIRST binding wins.
            if bound_names.contains(&key) {
                continue;
            }
            bound_names.push(key.clone());
            let value_bytes = read_input(unsafe { *json_values.get().add(i as usize) }, unsafe {
                *json_value_lengths.get().add(i as usize)
            });
            let Ok(value_text) = std::str::from_utf8(value_bytes) else {
                setup_failure(format!("binding {name} is not UTF-8 JSON"));
                return -1;
            };
            let Ok(value) = jqf_engine::decode_json(value_text, &handle.resources) else {
                setup_failure(format!("binding {name} is not a JSON value"));
                return -1;
            };
            value_bindings.push((key, value));
        }
        let outcome = compile_with_args(handle, program.get(), program_len, &mut value_bindings, out_id.get());
        match outcome {
            Ok(()) => 0,
            Err(_) => -1,
        }
    })
}

/// Releases one compiled program. Returns 0 when the id named a live program
/// (its compiled arena and ledger residency are released on the spot), -1
/// when it did not (never a reuse, a different handle's id, or an id already
/// freed — the table never reuses slots, so no stale id can ever alias a
/// live program). Never touches the diagnostic stream.
///
/// # Safety
///
/// `handle` must be a live pointer from [`jqf_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jqf_program_free(handle: *mut c_void, program_id: u32) -> c_int {
    hop_or(handle, -1, move |handle| {
        match handle.programs.remove(&program_id) {
            Some(slot) => {
                // The slot's `CompiledProgram` drops here, releasing the
                // compiled arena and its ledger residency.
                drop(slot);
                0
            }
            None => -1,
        }
    })
}

/// The strict framing profile, selected by passing `profile = 0` to
/// [`jqf_feed_open`]: the first framing or payload fault is terminal.
pub const JQF_FEED_PROFILE_STRICT: c_int = 0;
/// The recovering framing profile, selected by passing `profile = 1` to
/// [`jqf_feed_open`]: framing and payload faults become ordered issues and
/// the stream continues after the next physical line feed.
pub const JQF_FEED_PROFILE_RECOVERING: c_int = 1;

/// Opens a resident streaming feed on this handle : one compiled
/// program over one framed record stream whose input arrives in pieces.
/// `jqf_feed_push` appends input and frames complete records; `jqf_feed_poll`
/// runs the program's compiled requirement over ONE batch of the completed
/// range through the record route's own drive and publishes the batch into a
/// caller buffer under the `snprintf` convention; `jqf_feed_close` releases
/// the feed's retained-input residency.
///
/// `profile` selects the framing profile: [`JQF_FEED_PROFILE_STRICT`] (0) or
/// [`JQF_FEED_PROFILE_RECOVERING`] (1). The feed publishes JSON items with a
/// trailing newline, byte-identical to the whole-input record route over the
/// same records — one feed is one consumer, one thread, exactly like the
/// handle.
///
/// On success writes the `u32` feed id into `*out_id` and returns 0. The id
/// is a handle-local table index valid until [`jqf_free`] or
/// [`jqf_feed_close`], never reused, and usable only with THIS handle. On
/// failure returns -1 and retains exactly one `MACHINE_SETUP` diagnostic
/// explaining why: `program_id` not live on this handle, an unknown profile,
/// or a resource failure (the feed clears the stream first, so the retained
/// stream after a failed open is exactly the setup record).
///
/// # Safety
///
/// `handle` must be a live pointer from [`jqf_new`]; `out_id` must be a
/// valid `*mut u32` slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jqf_feed_open(
    handle: *mut c_void,
    program_id: u32,
    profile: c_int,
    out_id: *mut u32,
) -> c_int {
    if out_id.is_null() {
        return -1;
    }
    let out_id = SendPtr(out_id);
    hop_or(handle, -1, move |handle| {
        // The stream reflects exactly the open: cleared first, and a failure
        // retains exactly the setup record below (the compile law).
        handle.diagnostics.clear();
        let diagnostics = handle.diagnostics;
        let setup_failure = |message: String| -> String {
            diagnostics.record_setup_failure(&message);
            message
        };
        // A feed over a dead program is a DEFINED failure at open — a setup
        // record plus -1 — never a feed that will dangle later.
        if !handle.programs.contains_key(&program_id) {
            setup_failure(format!("program id {program_id} is not a live program on this handle"));
            return -1;
        }
        let profile = match profile {
            JQF_FEED_PROFILE_STRICT => NdjsonProfile::Strict,
            JQF_FEED_PROFILE_RECOVERING => NdjsonProfile::Recovering,
            other => {
                setup_failure(format!("unknown feed profile {other} (0 = strict, 1 = recovering)"));
                return -1;
            }
        };
        let feed = ResidentFeed::new(profile, handle.record_catalog, jqf_codec_json::ndjson::issue_text)
            .with_limits(handle.resources.limits());
        let id = handle.next_feed_id;
        let Some(next) = handle.next_feed_id.checked_add(1) else {
            setup_failure("feed table overflow".to_owned());
            return -1;
        };
        handle.next_feed_id = next;
        // SAFETY: this function's `# Safety` contract requires `out_id` to be
        // a valid, aligned, writable slot for one `u32`.
        unsafe {
            *out_id.get() = id;
        }
        handle.feeds.insert(id, FeedSlot { program_id, feed });
        0
    })
}

/// Appends one piece of input to a feed and recomputes its completed record
/// range (the NDJSON framer's exact cut: a record is complete only after its
/// physical terminator; everything after the last line feed is HELD until
/// more bytes arrive). Returns the number of bytes the feed now retains (the
/// published prefix is drained by `poll`, so the count is the feed's current
/// buffered input), or -1 on failure.
///
/// A framing or payload fault is NOT reported here — it is found by `poll`
/// through the record drive, exactly where the record route finds it (a bare
/// carriage return only becomes a fault once the byte after it is known).
/// `push` itself fails only on a resource failure or a dead feed id; both are
/// recorded on the handle's diagnostic stream (appended — `push` never
/// clears, because the stream is per-POLL: a push between polls must not
/// wipe the last poll's records). A `(NULL, 0)` input is the documented
/// empty piece .
///
/// # Safety
///
/// `handle` must be a live pointer from [`jqf_new`]; `input` must be
/// readable for `input_len` bytes and unmutated for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jqf_feed_push(handle: *mut c_void, feed_id: u32, input: *const u8, input_len: usize) -> i64 {
    let input = SendConstPtr(input);
    hop_or(handle, -1, move |handle| {
        let Some(feed_slot) = handle.feeds.get_mut(&feed_id) else {
            // A dead feed id is a DEFINED failure, and the stream says so —
            // but `push` never clears, so the record is appended to whatever
            // the last poll retained.
            let message = format!("feed id {feed_id} is not a live feed on this handle");
            handle.diagnostics.record_setup_failure(&message);
            return -1;
        };
        // SAFETY: this function's `# Safety` contract requires `input` to be
        // readable for `input_len` bytes and unmutated for the call;
        // `(NULL, 0)` is the documented empty piece.
        let input = read_input(input.get(), input_len);
        match feed_slot.feed.try_push(input) {
            Ok(retained) => i64::try_from(retained).unwrap_or(-1),
            Err(error) => {
                // A refused push is REPORTED, never silent: the piece named is
                // not retained (the feed keeps exactly what it had), and the
                // host sees -1 instead of a length that reads as success. The
                // record is appended — push never clears the stream.
                handle
                    .diagnostics
                    .record_setup_failure(&format!("feed push refused: {error}"));
                -1
            }
        }
    })
}

/// Polls a feed for ONE bounded batch of output. Copies up to `out_cap`
/// published bytes into `out` and returns the REQUIRED byte count of the
/// batch as an `int64_t` (the `snprintf` convention: when the returned count
/// is `<= out_cap`, `out` holds the complete batch; when it is GREATER, only
/// the first `out_cap` bytes were written and the caller must re-call with a
/// buffer of at least the returned size — re-calling re-delivers the SAME
/// batch, never the next one). Returns 0 when no record is completed and
/// unpublished and the feed is healthy. Returns -1 when the feed is
/// terminally failed (a strict framing or payload fault, a program halt, or
/// a setup failure), with the failure's diagnostic record retained on the
/// handle's stream. The sizing probe `(NULL, 0)` returns the required length
/// without writing .
///
/// Each poll computes at most ONE batch — bounded by the record route's own
/// `RECORD_BATCH_ENTRIES`/`RECORD_BATCH_TARGET_BYTES` pair — so a host
/// looping `push`/`poll` pays one ABI call per batch instead of one per
/// record, and a stream's output is byte-identical to the whole-input record
/// route over the same records.
///
/// The diagnostic stream is cleared per poll exactly as a run clears per
/// run — EXCEPT that a poll which only re-delivers a pending batch, or which
/// reports a death, does not clear: the death's retained record must stay
/// readable until the host reads it.
///
/// # Safety
///
/// `handle` must be a live pointer from [`jqf_new`]; `out` must be valid for
/// `out_cap` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jqf_feed_poll(handle: *mut c_void, feed_id: u32, out: *mut u8, out_cap: usize) -> i64 {
    let out = SendPtr(out);
    hop_or(handle, -1, move |handle| {
        let Some(feed_slot) = handle.feeds.get_mut(&feed_id) else {
            // A dead feed id is a DEFINED failure — a setup record plus -1 —
            // never undefined behavior.
            let message = format!("feed id {feed_id} is not a live feed on this handle");
            handle.diagnostics.clear();
            handle.diagnostics.record_setup_failure(&message);
            return -1;
        };
        // The stream reflects the last ENGINE operation: a mere re-delivery
        // of a pending batch, or a death report, does not clear it (a death's
        // record must stay readable); every poll that will run the framer or
        // the drive clears first.
        if !feed_slot.feed.has_pending_batch() && !feed_slot.feed.is_terminal() {
            handle.diagnostics.clear();
        }
        let program_id = feed_slot.program_id;
        // The program id is re-validated every poll: a program freed under a
        // live feed is a DEFINED poll failure, never a dangling reference.
        let Some(slot) = handle.programs.get(&program_id) else {
            let message = format!("program id {program_id} is not a live program on this handle");
            handle.diagnostics.record_setup_failure(&message);
            return -1;
        };
        // SAFETY: this function's `# Safety` contract requires `out` to be
        // valid for `out_cap` bytes; `(NULL, 0)` is the documented sizing
        // probe and yields the empty writable view.
        let out = write_output(out.get(), out_cap);
        let outcome = feed_slot
            .feed
            .poll(&slot.compiled, &mut handle.resources, handle.diagnostics, out);
        match outcome {
            FeedPoll::Batch(required) => byte_count(required),
            FeedPoll::Empty => 0,
            FeedPoll::Failed => -1,
        }
    })
}

/// Marks the end of a feed's input stream: the held partial record (if any)
/// becomes the stream's FINAL record, delivered by subsequent
/// [`jqf_feed_poll`] calls under the feed profile's own tail law — the same
/// answer the whole-input record route gives over the same bytes (a complete
/// final value without its terminator is accepted under both profiles; a
/// truncated one is a strict fault or a recovering issue). This is a CLEAN
/// end of stream: polls drain to 0, never to -1, and later pushes are
/// accepted-and-ignored. Returns 0 when the id named a live feed, -1 when it
/// did not.
///
/// # Safety
///
/// `handle` must be a live pointer from [`jqf_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jqf_feed_finish(handle: *mut c_void, feed_id: u32) -> c_int {
    hop_or(handle, -1, move |handle| {
        let Some(feed_slot) = handle.feeds.get_mut(&feed_id) else {
            let message = format!("feed id {feed_id} is not a live feed on this handle");
            handle.diagnostics.record_setup_failure(&message);
            return -1;
        };
        feed_slot.feed.finish();
        0
    })
}

/// Releases one resident feed: its retained-input residency is released on
/// the spot and the feed id is dead (never reused). Returns 0 when the id
/// named a live feed, -1 when it did not. Never touches the diagnostic
/// stream (the [`jqf_program_free`] law).
///
/// # Safety
///
/// `handle` must be a live pointer from [`jqf_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jqf_feed_close(handle: *mut c_void, feed_id: u32) -> c_int {
    hop_or(handle, -1, move |handle| match handle.feeds.remove(&feed_id) {
        Some(slot) => {
            drop(slot);
            0
        }
        None => -1,
    })
}

/// The published byte count as the ABI's signed 64-bit answer, UNCLAMPED to
/// any caller buffer — this is the `snprintf`-style required length, not the
/// number of bytes actually written. A caller buffer (and any Rust
/// allocation) is at most `isize::MAX` bytes, so the count always fits; the
/// failure sentinel is the answer if a target ever made it not.
fn byte_count(len: usize) -> i64 {
    i64::try_from(len).unwrap_or(-1)
}

/// The shared run body for every buffer-publishing run entry point: lowers
/// the EAGER whole-document requirement (the input-sequence drive's law —
/// see below), drives the SDK's input-sequence route over `input` with the
/// handle's codec context and encode options, publishing into the CALLER's
/// `out` buffer whatever fits and counting the rest (the [`CountingSink`]
/// law — no staging allocation exists on this path) and the per-value errors
/// into `errors`; records the route/cost on success and the terminal failure
/// (plus the halt status) on failure. Returns the REQUIRED byte count.
/// Takes the run's pieces rather than the whole handle so the caller can
/// keep a borrow of the program table alive across the run.
///
/// The requirement is the whole-document one, NOT the program's own
/// pushdown requirement (`try_requirement`, which `jqf_compile` retains for
/// early validation): the input-sequence drive materializes every value and
/// runs the WHOLE program (`try_run_whole_value`) — the `-n`/`-s` law in
/// `compile/mod.rs`. Lowering the program's own requirement there would have
/// the codec resolve the pushed-down prefix, and the drive would run the
/// residual over the pushed-down RESULT (`.a` over `{"a":1}` would run
/// `.a` over the number `1`). The record route (the feed) lowers its own
/// requirement inside `drive_record_range`, so it is untouched.
#[allow(clippy::too_many_arguments)]
fn run_into_handle(
    resources: &mut ResourceContext<'static>,
    diagnostics: &'static Diagnostics,
    catalog: CodecCatalog<'static, 'static>,
    format: &FormatId,
    dialect: &DialectId,
    encode_options: &JsonEncodeOptions,
    errors: &mut Vec<String>,
    halt_status: &mut Option<u32>,
    compiled: &CompiledProgram,
    input: &[u8],
    out: *mut u8,
    out_cap: usize,
) -> Result<usize, String> {
    let requirement = prepare_requirement(resources, diagnostics, compiled)?;
    // SAFETY: the entry point documents `out` as valid for `out_cap` bytes;
    // `(NULL, 0)` is the documented sizing probe and yields an empty view.
    let out = write_output(out, out_cap);
    let mut sink = CountingSink {
        out,
        copied: 0,
        total: 0,
        errors,
    };
    let outcome = run_input_sequence_inner(
        resources,
        catalog,
        format,
        dialect,
        encode_options,
        compiled,
        &requirement,
        input,
        &mut sink,
    );
    match finalize_outcome(diagnostics, resources, halt_status, outcome) {
        Ok(()) => Ok(usize::try_from(sink.total).unwrap_or(usize::MAX)),
        Err(error) => Err(error),
    }
}
/// The streaming entry point's run body: the same requirement, drive, and
/// outcome laws as [`run_into_handle`], with publication handed to the
/// host's chunk callback through the [`StreamingSink`] instead of a caller
/// buffer. Returns the number of bytes delivered.
#[allow(clippy::too_many_arguments)]
fn run_streaming_into_handle(
    resources: &mut ResourceContext<'static>,
    diagnostics: &'static Diagnostics,
    catalog: CodecCatalog<'static, 'static>,
    format: &FormatId,
    dialect: &DialectId,
    encode_options: &JsonEncodeOptions,
    errors: &mut Vec<String>,
    halt_status: &mut Option<u32>,
    compiled: &CompiledProgram,
    input: &[u8],
    chunk: JqfStreamChunkFn,
    context: *mut c_void,
) -> Result<u64, String> {
    let requirement = prepare_requirement(resources, diagnostics, compiled)?;
    let mut sink = StreamingSink {
        errors,
        chunk,
        context,
        buf: Vec::with_capacity(JQF_STREAM_CHUNK_BYTES),
        chunk_cap: JQF_STREAM_CHUNK_BYTES,
        delivered: 0,
        cancelled: false,
    };
    let outcome = run_input_sequence_inner(
        resources,
        catalog,
        format,
        dialect,
        encode_options,
        compiled,
        &requirement,
        input,
        &mut sink,
    );
    // The open partial buffer is delivered whether the run succeeded or
    // failed: chunks already handed over are the host's, so the stream it
    // holds stays consistent up to the cut (a cancellation inside the
    // flush leaves the tail undelivered and marks the sink cancelled —
    // the branch below reports it the same way).
    let flush_result = sink.flush();
    let delivered = sink.delivered;
    let cancelled = sink.cancelled;
    // A host cancellation is its OWN terminal class wherever it lands —
    // mid-run or inside the FINAL flush on an otherwise-successful run —
    // recorded after the generic failure mapping runs so both explanations
    // survive on the stream: the sink's stop surfaces as a sink-error
    // pipeline failure (which alone would read as an engine defect), plus
    // this record naming the cancellation and the byte count delivered
    // before it. A cancelled flush on a successful run takes the same
    // class: bytes crossed, publication stopped early, and returning a
    // positive count would tell the host the stream completed.
    if cancelled {
        diagnostics.record_setup_failure(&format!("streaming run cancelled by the host after {delivered} bytes"));
        return Err("stream cancelled by host".to_owned());
    }
    match finalize_outcome(diagnostics, resources, halt_status, outcome) {
        Ok(()) => {
            debug_assert!(flush_result.is_ok());
            Ok(delivered)
        }
        Err(error) => Err(error),
    }
}

/// Lowers the eager whole-document requirement every input-sequence run
/// starts from (the law documented on [`run_into_handle`]), recording a
/// setup failure when lowering refuses.
fn prepare_requirement(
    resources: &mut ResourceContext<'static>,
    diagnostics: &'static Diagnostics,
    compiled: &CompiledProgram,
) -> Result<AccessRequirement, String> {
    let requirement = compiled.try_whole_document_requirement(resources).map_err(|e| {
        diagnostics.record_setup_failure(&format!("{e}"));
        format!("{e}")
    })?;
    // Fact preservation rides the program's own fact access alone: this
    // surface pins ONE input/output format (strict JSON), so no format term
    // exists here.
    Ok(requirement.with_fact_intent(if compiled.accesses_facts() {
        jqf_codec_core::FactIntent::Preserve
    } else {
        jqf_codec_core::FactIntent::None
    }))
}

/// The shared run epilogue: records the route/cost on success or the
/// terminal failure (plus the halt status) on error.
fn finalize_outcome(
    diagnostics: &'static Diagnostics,
    resources: &ResourceContext<'static>,
    halt_status: &mut Option<u32>,
    outcome: Result<(), jqf_sdk::Failure>,
) -> Result<(), String> {
    match outcome {
        Ok(()) => {
            *halt_status = None;
            diagnostics.record_route_named("input-sequence");
            diagnostics.record_cost(&resources.snapshot());
            Ok(())
        }
        Err(error) => {
            // The halt exit status is a record-carried fact : the
            // `RAISE_HALT` terminal record has no integer field of its own,
            // so the status lives on the handle and `jqf_diag_get` reads it
            // for that record's row.
            if let Some(PipelineFailure::Halt { status, .. }) = error.pipeline_failure() {
                *halt_status = Some(*status);
            }
            if let Some(pipeline) = error.pipeline_failure() {
                diagnostics.record_failure(pipeline);
            }
            Err("run failed".to_owned())
        }
    }
}

/// The input-sequence drive itself: the retained source, the decode/encode
/// policy (with the handle's JSON encode options), and the execute call. This
/// is the ONE route every run entry point takes: RFC 8259 adjacent values
/// with the shared input cursor installed,
/// so `[inputs]`/`input`/`$ENV` answer as they do under the CLI.
#[allow(clippy::too_many_arguments)]
#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "the SDK's execute_input_sequence itself takes &FormatId/&DialectId/&JsonEncodeOptions, so the wrapper forwards the same borrows instead of copying"
)]
fn run_input_sequence_inner<S: ItemSink<Error = &'static str>>(
    resources: &mut ResourceContext<'static>,
    catalog: CodecCatalog<'static, 'static>,
    format: &FormatId,
    dialect: &DialectId,
    encode_options: &JsonEncodeOptions,
    compiled: &CompiledProgram,
    requirement: &AccessRequirement,
    input: &[u8],
    sink: &mut S,
) -> Result<(), jqf_sdk::Failure> {
    let source = ResolvedSource::new(SourceRef::new(SourceId::new(1), SourceKind::Input), "<ffi>", input, 0);
    let policy = PipelinePolicy {
        decode: DecodeRequest {
            validation: ValidationMode::Strict,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            // The caller's dialect, not a hardcoded JSON one: the provider
            // factories dispatch on `policy.decode.dialect`, so it must name
            // the same input dialect `with_format` passes.
            dialect,
            options: None,
            allow_adjacent_values: true,
            value_separator: jqf_codec_json::VALUE_SEPARATORS,
        },
        encode_diagnostics: DiagnosticPolicy::ErrorsOnly,
        preservation: PreservationRequest::Report,
        encode_options: Some(encode_options as &(dyn core::any::Any + Send + Sync)),
        cooperative_credits: 64,
        split: None,
        max_iterations: None,
    };
    // `--raw-output0` replaces the item terminator with a NUL (the CLI's
    // framing law for that flag); everything else keeps the newline.
    let framing = FacadeFraming::item_suffix(if encode_options.raw_output_nul { b"\0" } else { b"\n" });
    let request = jqf_sdk::Request::new(compiled, jqf_sdk::Input::Whole(input))
        .with_catalog(catalog)
        .with_source(source)
        .with_format(
            jqf_data::FormatId::try_new(format.as_str()).expect("built-in format identity"),
            jqf_data::DialectId::try_new(dialect.as_str()).expect("built-in dialect identity"),
        )
        .with_output_format(
            jqf_data::FormatId::try_new(format.as_str()).expect("built-in format identity"),
            jqf_data::DialectId::try_new(dialect.as_str()).expect("built-in dialect identity"),
        )
        .with_policy(policy)
        .with_framing(framing)
        .with_resources(resources)
        .with_requirement(requirement)
        .with_input_family();
    match jqf_sdk::execute(request, sink) {
        Ok(_) => Ok(()),
        Err(error) => Err(error),
    }
}

/// the reference's `$ARGS` object, the ONE spelling every compile path binds: the
/// positional side is always empty (the FFI binding API is named-only), and
/// `named` is the caller's already-built bindings object. [`compile_with_args`]
/// and the zero-binding fresh-compile path both build through this, so the
/// shape cannot drift between them.
fn build_args_value(named: jqf_data::Object) -> Result<Value, String> {
    let positional =
        jqf_data::Array::try_from_vec(Vec::new()).map_err(|_| "cannot allocate $ARGS.positional".to_owned())?;
    let mut builder = jqf_data::ObjectBuilder::try_with_capacity(2).map_err(|_| "cannot allocate $ARGS".to_owned())?;
    for (name, value) in [
        ("positional", Value::Array(positional)),
        ("named", Value::Object(named)),
    ] {
        let key = jqf_data::ObjectKey::try_from_str(name).map_err(|_| "cannot allocate $ARGS key".to_owned())?;
        builder
            .try_insert_last(key, value)
            .map_err(|_| "cannot allocate $ARGS entry".to_owned())?;
    }
    builder
        .try_finish()
        .map(jqf_data::Value::Object)
        .map_err(|_| "cannot allocate $ARGS".to_owned())
}

/// the reference's `$ARGS` for a request with NO bindings: `{"positional": [], "named":
/// {}}`. Every compile path binds `$ARGS`, so a fresh `jqf_run` compile
/// carries it too, exactly as `jqf_compile` does via [`compile_with_args`].
fn empty_args_value(_resources: &ResourceContext<'static>) -> Result<Value, String> {
    let named = jqf_data::ObjectBuilder::try_with_capacity(0)
        .map_err(|_| "cannot allocate $ARGS.named".to_owned())?
        .try_finish()
        .map_err(|_| "cannot allocate $ARGS.named".to_owned())?;
    build_args_value(named)
}

/// The fresh-compile run body shared by [`jqf_run`] and [`jqf_run_sequence`]:
/// validates the program bytes (never silently truncated at an embedded
/// NUL), compiles, lowers the requirement, and runs.
fn run_fresh(
    handle: &mut JqfHandle,
    program: *const u8,
    program_len: usize,
    input: *const u8,
    input_len: usize,
    out: *mut u8,
    out_cap: usize,
) -> Result<usize, String> {
    let diagnostics = handle.diagnostics;
    let setup_failure = |message: String| -> String {
        diagnostics.record_setup_failure(&message);
        message
    };
    // SAFETY: this function's `# Safety` contract requires `program` to be
    // readable for `program_len` bytes; `(NULL, 0)` is the documented empty
    // program.
    let program_bytes = read_input(program, program_len);
    if program_bytes.contains(&0) {
        return Err(setup_failure(
            "program contains a NUL byte; pass the exact program length".to_owned(),
        ));
    }
    let program = std::str::from_utf8(program_bytes).map_err(|_| setup_failure("program is not UTF-8".to_owned()))?;
    // `$ARGS` is bound on EVERY request (the reference's law): a fresh compile carries
    // the empty form, exactly as `jqf_compile` does.
    let args_bindings = [(
        String::from("$ARGS"),
        empty_args_value(&handle.resources).map_err(setup_failure)?,
    )];
    let compiled = try_compile_program(
        program,
        POLICY,
        CompileOptions {
            cli_vars: &args_bindings,
            split_exp: false,
            ..Default::default()
        },
        &handle.resources,
    )
    .map_err(|e| setup_failure(format!("{e}")))?;
    // SAFETY: this function's `# Safety` contract requires `input` to be
    // readable for `input_len` bytes; `(NULL, 0)` is the documented empty
    // input .
    let input = read_input(input, input_len);
    run_into_handle(
        &mut handle.resources,
        handle.diagnostics,
        handle.catalog,
        &handle.format,
        &handle.dialect,
        &handle.encode_options,
        &mut handle.errors,
        &mut handle.halt_status,
        &compiled,
        input,
        out,
        out_cap,
    )
}

/// The compiled-slot run body shared by [`jqf_run_compiled`] and
/// [`jqf_run_sequence_compiled`]: validates the program id (a DEFINED
/// setup failure, never a dangling dereference) and runs the retained
/// program.
fn run_slot(
    handle: &mut JqfHandle,
    program_id: u32,
    input: *const u8,
    input_len: usize,
    out: *mut u8,
    out_cap: usize,
) -> Result<usize, String> {
    let diagnostics = handle.diagnostics;
    let setup_failure = |message: String| -> String {
        diagnostics.record_setup_failure(&message);
        message
    };
    // An id that names no live program is a DEFINED failure — a setup
    // record plus `-1` — never a dangling dereference: the table never
    // reuses slots, and the id is validated against this handle's own
    // table before anything runs.
    let slot = handle
        .programs
        .get(&program_id)
        .ok_or_else(|| setup_failure(format!("program id {program_id} is not a live program on this handle")))?;
    // SAFETY: this function's `# Safety` contract requires `input` to be
    // readable for `input_len` bytes; `(NULL, 0)` is the documented empty
    // input .
    let input = read_input(input, input_len);
    run_into_handle(
        &mut handle.resources,
        handle.diagnostics,
        handle.catalog,
        &handle.format,
        &handle.dialect,
        &handle.encode_options,
        &mut handle.errors,
        &mut handle.halt_status,
        &slot.compiled,
        input,
        out,
        out_cap,
    )
}

/// The per-run epilogue every buffer-publishing run entry point shares.
///
/// Returns the run's REQUIRED byte count under the `snprintf` convention
/// (`-1` on failure). The bytes themselves were already written into the
/// caller's buffer by the [`CountingSink`] DURING the run — there is no
/// staging buffer to copy out of. `result` is the raw `catch_unwind`
/// outcome of the run body after `hop` has already caught a panic.
fn finish_run(result: Result<usize, String>) -> i64 {
    result.ok().map_or(-1, byte_count)
}

/// Runs one COMPILED program over `input` bytes. Identical contract to
/// [`jqf_run`] (the `snprintf` convention, the `-1` failure sentinel, the
/// per-run diagnostic stream) minus the compile: `program_id` must name a
/// live program compiled on THIS handle by [`jqf_compile`]. An id that names
/// no live program is a DEFINED failure — `-1` with a `MACHINE_SETUP`
/// diagnostic retained — never undefined behavior. `jqf_run_sequence_compiled`
/// shares this exact run body; like [`jqf_run`]/[`jqf_run_sequence`] the two
/// names cover one behavior until an ABI version bump folds them.
///
/// # Safety
///
/// `handle`, `input`, and `out` must be valid for their lengths; `program_id`
/// is any `u32` and is validated, not trusted.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jqf_run_compiled(
    handle: *mut c_void,
    program_id: u32,
    input: *const u8,
    input_len: usize,
    out: *mut u8,
    out_cap: usize,
) -> i64 {
    let input = SendConstPtr(input);
    let out = SendPtr(out);
    hop_or(handle, -1, move |handle| {
        // Every operation starts with an empty diagnostic stream (see
        // `run_fresh` for the law), an empty per-run error list , and
        // no halt status .
        handle.diagnostics.clear();
        handle.errors.clear();
        handle.halt_status = None;
        let result = run_slot(handle, program_id, input.get(), input_len, out.get(), out_cap);
        finish_run(result)
    })
}

/// Runs one program over `input` bytes. Copies up to `out_cap` published
/// bytes into `out` and returns the REQUIRED byte count as an `int64_t`
/// (`-1` on failure) — the `snprintf` convention: when the returned count is
/// `<= out_cap`, `out` holds the complete output; when it is GREATER than
/// `out_cap`, only the first `out_cap` bytes were written and the caller must
/// re-call with a buffer of at least the returned size to get the rest. Also
/// clears and retains this run's diagnostic records for `jqf_diag_*` (never a
/// prior run's, even on a reused handle).
///
/// `program` is `program_len` bytes and is REJECTED (a `MACHINE_SETUP`
/// diagnostic, `-1`) when it contains an embedded NUL — a program can never
/// silently truncate at its first NUL . A `(NULL, 0)` input is the
/// empty input: the reference's own answer for no values.
///
/// The input is RFC 8259 ADJACENT values (the reference's default stdin model), and the
/// shared input cursor is installed, so `[inputs]`/`input`/`$ENV`/`env`
/// answer exactly as they do under the CLI . Per-value errors are
/// retained on the handle and read back through [`jqf_run_errors_count`] /
/// [`jqf_run_error_get`].
///
/// `jqf_run_sequence` shares this exact run body: `jqf_run` and
/// `jqf_run_sequence` are two names for one adjacent-value behavior.
///
/// # Safety
///
/// `handle`, `program`, `input`, and `out` must be valid for their lengths.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jqf_run(
    handle: *mut c_void,
    program: *const u8,
    program_len: usize,
    input: *const u8,
    input_len: usize,
    out: *mut u8,
    out_cap: usize,
) -> i64 {
    let program = SendConstPtr(program);
    let input = SendConstPtr(input);
    let out = SendPtr(out);
    hop_or(handle, -1, move |handle| {
        handle.diagnostics.clear();
        handle.errors.clear();
        handle.halt_status = None;
        let result = run_fresh(
            handle,
            program.get(),
            program_len,
            input.get(),
            input_len,
            out.get(),
            out_cap,
        );
        finish_run(result)
    })
}

/// Runs one COMPILED program over a MULTI-VALUE input stream (adjacent
/// values, like the reference's stdin) through the SDK's input-sequence drive. Same
/// contract as [`jqf_run_compiled`] (the `snprintf` convention, the `-1`
/// sentinel, the per-run diagnostic stream), plus the per-value errors:
/// they are retained on the handle for THIS run and read back through
/// [`jqf_run_errors_count`] / [`jqf_run_error_get`] — a length-carrying
/// channel, so an error whose text contains a NUL byte survives intact (the
/// previous NUL-joined `out_errors` out-param is gone with the ABI
/// version bump).
///
/// # Safety
///
/// Same pointer rules as [`jqf_run_compiled`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jqf_run_sequence_compiled(
    handle: *mut c_void,
    program_id: u32,
    input: *const u8,
    input_len: usize,
    out: *mut u8,
    out_cap: usize,
) -> i64 {
    let input = SendConstPtr(input);
    let out = SendPtr(out);
    hop_or(handle, -1, move |handle| {
        handle.diagnostics.clear();
        handle.errors.clear();
        handle.halt_status = None;
        let result = run_slot(handle, program_id, input.get(), input_len, out.get(), out_cap);
        finish_run(result)
    })
}

/// Runs one program over a MULTI-VALUE input stream (adjacent values, like
/// the reference's stdin) through the SDK's input-sequence drive. Same contract as
/// [`jqf_run`], plus the per-value errors: they are retained on the handle
/// for THIS run and read back through [`jqf_run_errors_count`] /
/// [`jqf_run_error_get`] — a length-carrying channel, so an error whose text
/// contains a NUL byte survives intact (the previous NUL-joined
/// `out_errors` out-param is gone with the ABI version bump).
///
/// # Safety
///
/// Same pointer rules as [`jqf_run`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jqf_run_sequence(
    handle: *mut c_void,
    program: *const u8,
    program_len: usize,
    input: *const u8,
    input_len: usize,
    out: *mut u8,
    out_cap: usize,
) -> i64 {
    let program = SendConstPtr(program);
    let input = SendConstPtr(input);
    let out = SendPtr(out);
    hop_or(handle, -1, move |handle| {
        handle.diagnostics.clear();
        handle.errors.clear();
        handle.halt_status = None;
        let result = run_fresh(
            handle,
            program.get(),
            program_len,
            input.get(),
            input_len,
            out.get(),
            out_cap,
        );
        finish_run(result)
    })
}

/// Runs one program over a MULTI-VALUE input stream (adjacent values, like
/// the reference's stdin) and hands the output to the host in BOUNDED
/// chunks through `chunk(context, bytes, len)` instead of staging one
/// contiguous buffer. This is [`jqf_run_sequence`] with the staging `Vec`
/// and its growth transient removed: a consumer that forwards each chunk as
/// it arrives never holds more than one chunk plus one encoded value, where
/// the buffer entry points publish through [`CountingSink`] with no staging copy.
///
/// The drive, the input model, the diagnostic stream, the per-value error
/// channel ([`jqf_run_errors_count`] / [`jqf_run_error_get`]), and the halt
/// status are EXACTLY the buffer entry points' laws; only publication
/// differs. On success the return value is the total number of bytes
/// delivered — the same count `jqf_run_sequence` would have required — and
/// concatenating the delivered chunks in order reproduces the buffer path's
/// output byte for byte.
///
/// Chunks are FLOW units, not frame boundaries: a delivery is at most one
/// internal chunk (256 KiB) except when a single encoder write exceeds it,
/// and one huge VALUE may straddle many callbacks. The consumer's memory
/// stays bounded by its own per-chunk handling no matter how large one
/// value grows; the output byte stream itself carries the value framing.
///
/// The callback runs on this handle's request thread, before the entry
/// returns; `bytes` is valid ONLY for the duration of the call and must be
/// copied or forwarded before returning. The verdicts are
/// `JQF_STREAM_CONTINUE` (0) and `JQF_STREAM_CANCEL` (1); an unknown
/// verdict continues, the same law [`JqfLimits::control_callback`] keeps.
/// A cancellation stops publication cleanly and returns `-1` with a
/// `MACHINE_SETUP` record naming the cancellation and the byte count
/// delivered so far — chunks already handed over stay with the host.
///
/// A NULL `chunk` is rejected with a setup record and `-1`, like every
/// other defined misuse on this surface.
///
/// # Safety
///
/// `handle`, `program`, and `input` must be valid for their lengths;
/// `context` must be valid for every callback invocation, which ends when
/// the entry point returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jqf_run_sequence_streaming(
    handle: *mut c_void,
    program: *const u8,
    program_len: usize,
    input: *const u8,
    input_len: usize,
    chunk: Option<JqfStreamChunkFn>,
    context: *mut c_void,
) -> i64 {
    let Some(chunk) = chunk else {
        // SAFETY: this function's `# Safety` contract requires `handle` to
        // be a live handle or NULL (`hop_or` answers the sentinel).
        return hop_or(handle, -1, |handle| {
            handle.diagnostics.clear();
            let message = "chunk callback is NULL; pass jqf_run_sequence_streaming a consumer";
            handle.diagnostics.record_setup_failure(message);
            -1
        });
    };
    let program = SendConstPtr(program);
    let input = SendConstPtr(input);
    let context = SendPtr(context);
    hop_or(handle, -1, move |handle| {
        handle.diagnostics.clear();
        handle.errors.clear();
        handle.halt_status = None;
        let result = run_fresh_streaming(
            handle,
            program.get(),
            program_len,
            input.get(),
            input_len,
            chunk,
            context.get(),
        );
        match result {
            Ok(delivered) => byte_count(usize::try_from(delivered).unwrap_or(usize::MAX)),
            Err(_) => -1,
        }
    })
}

/// The number of per-value errors the LAST run retained . Cleared
/// per run exactly like the diagnostic records.
///
/// # Safety
///
/// `handle` must be a valid handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jqf_run_errors_count(handle: *const c_void) -> c_uint {
    hop_or(handle.cast_mut(), 0, |handle| {
        c_uint::try_from(handle.errors.len()).unwrap_or(c_uint::MAX)
    })
}

/// The streaming entry point's fresh-compile body: the same NUL/UTF-8
/// program laws and `$ARGS` binding as [`run_fresh`], publishing through
/// [`run_streaming_into_handle`].
#[allow(clippy::too_many_arguments)]
fn run_fresh_streaming(
    handle: &mut JqfHandle,
    program: *const u8,
    program_len: usize,
    input: *const u8,
    input_len: usize,
    chunk: JqfStreamChunkFn,
    context: *mut c_void,
) -> Result<u64, String> {
    let diagnostics = handle.diagnostics;
    let setup_failure = |message: String| -> String {
        diagnostics.record_setup_failure(&message);
        message
    };
    // SAFETY: this function's `# Safety` contract requires `program` to be
    // readable for `program_len` bytes; `(NULL, 0)` is the documented empty
    // program.
    let program_bytes = read_input(program, program_len);
    if program_bytes.contains(&0) {
        return Err(setup_failure(
            "program contains a NUL byte; pass the exact program length".to_owned(),
        ));
    }
    let program = std::str::from_utf8(program_bytes).map_err(|_| setup_failure("program is not UTF-8".to_owned()))?;
    let args_bindings = [(
        String::from("$ARGS"),
        empty_args_value(&handle.resources).map_err(setup_failure)?,
    )];
    let compiled = try_compile_program(
        program,
        POLICY,
        CompileOptions {
            cli_vars: &args_bindings,
            split_exp: false,
            ..Default::default()
        },
        &handle.resources,
    )
    .map_err(|e| setup_failure(format!("{e}")))?;
    // SAFETY: this function's `# Safety` contract requires `input` to be
    // readable for `input_len` bytes; `(NULL, 0)` is the documented empty
    // input .
    let input = read_input(input, input_len);
    run_streaming_into_handle(
        &mut handle.resources,
        handle.diagnostics,
        handle.catalog,
        &handle.format,
        &handle.dialect,
        &handle.encode_options,
        &mut handle.errors,
        &mut handle.halt_status,
        &compiled,
        input,
        chunk,
        context,
    )
}

/// Copies one retained per-value error's TEXT into the caller buffer under
/// the `snprintf` convention (returns the REQUIRED length; `-1` when `index`
/// is out of range). The bytes are the error's own text, which may contain
/// NUL bytes — a NUL-joined out-param would have collapsed those.
///
/// # Safety
///
/// `handle` must be valid; `out` must be valid for `out_cap` bytes (the
/// `(NULL, 0)` sizing probe is defined).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jqf_run_error_get(handle: *const c_void, index: c_uint, out: *mut u8, out_cap: usize) -> i64 {
    let out = SendPtr(out);
    hop_or(handle.cast_mut(), -1, move |handle| {
        let Some(error) = handle.errors.get(index as usize) else {
            return -1;
        };
        publish(error.as_bytes(), out.get(), out_cap)
    })
}

/// The number of retained diagnostic records from the last operation.
///
/// # Safety
///
/// `handle` must be a valid handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jqf_diag_count(handle: *const c_void) -> c_uint {
    hop_or(handle.cast_mut(), 0, |handle| {
        c_uint::try_from(handle.diagnostics.len()).unwrap_or(c_uint::MAX)
    })
}

/// How many diagnostic records overflowed the cap and were dropped.
///
/// `0` on a NULL handle. The buffer evicts oldest first; this is the
/// count the header promised and the existing `dropped()` method already
/// computed.
///
/// # Safety
///
/// `handle` must be a live pointer from [`jqf_new`], or NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jqf_diag_dropped(handle: *const c_void) -> c_uint {
    hop_or(handle.cast_mut(), 0, |handle| {
        c_uint::try_from(handle.diagnostics.dropped()).unwrap_or(c_uint::MAX)
    })
}

/// Copies one retained record's fields into the out-parameters. Returns 0 on
/// success, -1 when `index` is out of range. Every text field is a fresh
/// allocation the caller releases with `jqf_diag_free_text`.
///
/// The three locator out-params surface the record's own
/// `step_index`/`input_ordinal`/`byte_offset` , with the sentinels
/// `u32::MAX`/`u64::MAX` when the record carries none. `halt_status` is the
/// exit status of the run when this record is the `RAISE_HALT` terminal
/// (`halt_error(msg; n)`'s `n`, or `0` for a bare `halt`), and `-1` for every
/// other record.
///
/// # Safety
///
/// `handle` must be valid; every out-parameter must be a valid `*mut` slot
/// of its declared type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jqf_diag_get(
    handle: *const c_void,
    index: c_uint,
    code: *mut u16,
    revision: *mut u16,
    record_class: *mut c_char,
    severity: *mut c_char,
    catchable: *mut u8,
    caught: *mut u32,
    step_index: *mut u32,
    input_ordinal: *mut u64,
    byte_offset: *mut u64,
    halt_status: *mut i32,
    kind: *mut *mut c_char,
    operand: *mut *mut c_char,
    payload: *mut *mut c_char,
) -> c_int {
    if code.is_null()
        || revision.is_null()
        || record_class.is_null()
        || severity.is_null()
        || catchable.is_null()
        || caught.is_null()
        || step_index.is_null()
        || input_ordinal.is_null()
        || byte_offset.is_null()
        || halt_status.is_null()
        || kind.is_null()
        || operand.is_null()
        || payload.is_null()
    {
        return -1;
    }
    let code = SendPtr(code);
    let revision = SendPtr(revision);
    let record_class = SendPtr(record_class);
    let severity = SendPtr(severity);
    let catchable = SendPtr(catchable);
    let caught = SendPtr(caught);
    let step_index = SendPtr(step_index);
    let input_ordinal = SendPtr(input_ordinal);
    let byte_offset = SendPtr(byte_offset);
    let halt_status = SendPtr(halt_status);
    let kind = SendPtr(kind);
    let operand = SendPtr(operand);
    let payload = SendPtr(payload);
    hop_or(handle.cast_mut(), -1, move |handle| {
        let fallback_halt_status = handle.halt_status;
        handle
            .diagnostics
            .with_record(index as usize, |record| {
                // SAFETY: this function's `# Safety` contract requires every
                // out-parameter to be a valid, aligned, writable slot of its
                // declared type. Each text slot receives a fresh `CString`
                // allocation the caller releases with `jqf_diag_free_text`,
                // so nothing borrows from `handle` past this write.
                unsafe {
                    *code.get() = record.code;
                    *revision.get() = record.revision;
                    *record_class.get() = class_byte(record.class);
                    *severity.get() = severity_byte(record.severity);
                    *catchable.get() = u8::from(record.catchable);
                    *caught.get() = record.caught.unwrap_or(u32::MAX);
                    *step_index.get() = record.step_index.unwrap_or(u32::MAX);
                    *input_ordinal.get() = record.input_ordinal.unwrap_or(u64::MAX);
                    *byte_offset.get() = record.byte_offset.unwrap_or(u64::MAX);
                    // The halt exit status rides the RAISE_HALT record's own
                    // field. The handle's legacy channel remains the fallback
                    // when the retained record predates the field.
                    *halt_status.get() = if record.code == codes::RAISE_HALT {
                        record
                            .halt_status
                            .or(fallback_halt_status)
                            .and_then(|status| i32::try_from(status).ok())
                            .unwrap_or(-1)
                    } else {
                        -1
                    };
                    *kind.get() = text_ptr(record.kind());
                    *operand.get() = text_ptr(record.operand());
                    *payload.get() = text_ptr(record.payload());
                }
                0
            })
            .unwrap_or(-1)
    })
}

/// Releases a text allocation from [`jqf_diag_get`].
///
/// # Safety
///
/// `text` must be a pointer from [`jqf_diag_get`], not already freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jqf_diag_free_text(text: *mut c_char) {
    if !text.is_null() {
        // SAFETY: this function's `# Safety` contract requires `text` to be a
        // pointer [`jqf_diag_get`] produced with `CString::into_raw` and not
        // yet freed, so the re-box reclaims that exact allocation once.
        let _ = catch_unwind(AssertUnwindSafe(|| {
            drop(unsafe { CString::from_raw(text) });
        }));
        // A panic must never unwind across the C boundary; a caught panic
        // here is discarded (the free is a void cleanup, and unwinding into
        // a C caller is UB).
    }
}

/// Copies one retained record's text field (kind / operand / payload) into
/// the caller buffer under the `snprintf` convention. Unlike
/// [`jqf_diag_get`], this channel is length-carrying: a NUL inside the
/// text survives. `field` is [`JQF_DIAG_TEXT_KIND`],
/// [`JQF_DIAG_TEXT_OPERAND`], or [`JQF_DIAG_TEXT_PAYLOAD`]. Returns the
/// required length, or `-1` when the record or field is absent.
///
/// # Safety
///
/// `handle` must be a live pointer from [`jqf_new`]; `out` must be valid
/// for `out_cap` bytes (the `(NULL, 0)` sizing probe is defined).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jqf_diag_get_text(
    handle: *const c_void,
    index: c_uint,
    field: c_int,
    out: *mut u8,
    out_cap: usize,
) -> i64 {
    let out = SendPtr(out);
    hop_or(handle.cast_mut(), -1, move |handle| {
        handle
            .diagnostics
            .with_record(index as usize, |record| {
                let text = match field {
                    JQF_DIAG_TEXT_KIND => record.kind(),
                    JQF_DIAG_TEXT_OPERAND => record.operand(),
                    JQF_DIAG_TEXT_PAYLOAD => record.payload(),
                    _ => return -1,
                };
                let Some(text) = text else {
                    return -1;
                };
                publish(text.as_bytes(), out.get(), out_cap)
            })
            .unwrap_or(-1)
    })
}

fn text_ptr(text: Option<&str>) -> *mut c_char {
    match text {
        Some(text) => CString::new(text).map_or_else(|_| core::ptr::null_mut(), CString::into_raw),
        None => core::ptr::null_mut(),
    }
}

// `c_char` is signed on some targets and unsigned on others, so neither `as`
// nor `cast_signed()` is portable on its own. Every tag here is ASCII (< 0x80),
// so the byte value is identical under either signedness and `as` is exact.
// The wrap lint can only fire where `c_char` is SIGNED (i8): a u8->i8 cast
// clippy flags, and no ASCII byte ever wraps. Where `c_char` is unsigned —
// aarch64-linux, the one unsigned-c_char target in the support envelope — the
// cast cannot wrap and the expectation would be UNFULFILLED, so it is
// conditional on the signedness. A future unsigned-c_char target must extend
// the cfg; clippy says which platform forgot, by refusing to compile.
#[cfg_attr(
    not(all(target_os = "linux", target_arch = "aarch64")),
    expect(
        clippy::cast_possible_wrap,
        reason = "ASCII tag bytes are all < 0x80, so the c_char conversion cannot wrap on \
                  signed targets; aarch64-linux c_char is unsigned and the lint cannot fire"
    )
)]
fn class_byte(class: jqf_resource::diag::RecordClass) -> c_char {
    match class {
        jqf_resource::diag::RecordClass::Semantic => b'S' as c_char,
        jqf_resource::diag::RecordClass::Machine => b'M' as c_char,
        jqf_resource::diag::RecordClass::ProgramRaised => b'R' as c_char,
        jqf_resource::diag::RecordClass::Informational => b'I' as c_char,
    }
}

#[cfg_attr(
    not(all(target_os = "linux", target_arch = "aarch64")),
    expect(
        clippy::cast_possible_wrap,
        reason = "ASCII tag bytes are all < 0x80, so the c_char conversion cannot wrap on \
                  signed targets; aarch64-linux c_char is unsigned and the lint cannot fire"
    )
)]
fn severity_byte(severity: jqf_resource::diag::Severity) -> c_char {
    match severity {
        jqf_resource::diag::Severity::Error => b'E' as c_char,
        jqf_resource::diag::Severity::Warning => b'W' as c_char,
        jqf_resource::diag::Severity::Info => b'I' as c_char,
        jqf_resource::diag::Severity::Trace => b'T' as c_char,
    }
}

/// The generated code registry re-exported for bindings that want the
/// constants without linking the resource crate (the Python mirror is
/// generated alongside this).
pub use jqf_resource::diag::codes;

/// The record route's own batch bound, re-exported because it IS the feed's
/// batch bound (one `poll` publishes at most one such batch — the
/// feed introduces no new number).
pub use jqf_sdk::{RECORD_BATCH_ENTRIES, RECORD_BATCH_TARGET_BYTES};

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr::{self, from_mut};

    #[test]
    fn freed_program_slot_is_dropped() {
        let mut handle = ptr::null_mut();
        assert_eq!(unsafe { jqf_new(from_mut(&mut handle)) }, 0);
        for i in 0..64 {
            let mut id = u32::MAX;
            let program = b".";
            assert_eq!(
                unsafe { jqf_compile(handle, program.as_ptr(), program.len(), from_mut(&mut id)) },
                0
            );
            assert_eq!(id, i);
            assert_eq!(unsafe { jqf_program_free(handle, id) }, 0);
            // A freed id is dead; re-freeing it is a defined -1, never a reuse.
            assert_eq!(unsafe { jqf_program_free(handle, id) }, -1);
        }
        let mut id = u32::MAX;
        assert_eq!(unsafe { jqf_compile(handle, b".".as_ptr(), 1, from_mut(&mut id)) }, 0);
        assert_eq!(id, 64, "ids stay monotone");
        unsafe { jqf_free(handle) };
    }

    #[test]
    fn null_handle_and_out_params_return_minus_one() {
        let mut id = 0u32;
        assert_eq!(
            unsafe { jqf_compile(ptr::null_mut(), b".".as_ptr(), 1, from_mut(&mut id)) },
            -1
        );
        let mut handle = ptr::null_mut();
        assert_eq!(unsafe { jqf_new(from_mut(&mut handle)) }, 0);
        assert_eq!(unsafe { jqf_compile(handle, b".".as_ptr(), 1, ptr::null_mut()) }, -1);
        assert_eq!(unsafe { jqf_program_free(ptr::null_mut(), 0) }, -1);
        assert_eq!(
            unsafe { jqf_new_limited(ptr::null(), ptr::null(), ptr::null_mut()) },
            -1
        );
        let mut leftover = 0xdead_beef as *mut c_void;
        let tiny = JqfLimits {
            max_output_bytes: u64::MAX,
            max_memory_bytes: 1,
            max_spill_bytes: u64::MAX,
            max_nesting_depth: u32::MAX,
            deadline_ms: 0,
            control_callback: None,
            control_context: ptr::null_mut(),
        };
        assert_eq!(
            unsafe { jqf_new_limited(&raw const tiny, ptr::null(), from_mut(&mut leftover)) },
            -1
        );
        assert!(leftover.is_null(), "failure must write NULL into *out");
        unsafe { jqf_free(handle) };
    }
}

/// The streaming sink's sealing law, driven at ARBITRARY chunk capacities
/// and write shapes: the integration tests can only observe the 256 KiB
/// default through the ABI, so the boundary arithmetic itself is pinned
/// here — including the 1-byte capacity that makes every seal visible.
#[cfg(test)]
mod streaming_sink_tests {
    use super::*;

    type Chunks = Vec<Vec<u8>>;

    unsafe extern "C" fn gather(context: *mut c_void, bytes: *const u8, len: usize) -> c_int {
        // SAFETY: each test passes a live `&mut Chunks` context.
        let sink = unsafe { &mut *context.cast::<Chunks>() };
        // SAFETY: `bytes` is valid for `len` bytes for this call.
        let slice = unsafe { core::slice::from_raw_parts(bytes, len) };
        sink.push(slice.to_vec());
        JQF_STREAM_CONTINUE
    }

    unsafe extern "C" fn cancel_second(context: *mut c_void, bytes: *const u8, len: usize) -> c_int {
        // SAFETY: same context law as `gather`.
        let sink = unsafe { &mut *context.cast::<Chunks>() };
        // SAFETY: `bytes` is valid for `len` bytes for this call.
        let slice = unsafe { core::slice::from_raw_parts(bytes, len) };
        sink.push(slice.to_vec());
        if sink.len() >= 2 {
            JQF_STREAM_CANCEL
        } else {
            JQF_STREAM_CONTINUE
        }
    }

    fn make(errors: &mut Vec<String>, cap: usize, chunks: *mut Chunks) -> StreamingSink<'_> {
        StreamingSink {
            errors,
            chunk: gather,
            context: chunks.cast::<c_void>(),
            buf: Vec::with_capacity(cap.max(1)),
            chunk_cap: cap,
            delivered: 0,
            cancelled: false,
        }
    }

    #[test]
    fn a_one_byte_cap_delivers_every_write_as_its_own_callback() {
        let mut errors = Vec::new();
        let mut chunks: Chunks = Vec::new();
        let ctx = &raw mut chunks;
        let mut sink = make(&mut errors, 1, ctx);
        let _ = sink.write(b"a");
        // A write at or above the cap on an empty buffer goes direct: the
        // two bytes arrive together at write time, never split.
        let _ = sink.write(b"bc");
        let _ = sink.write(b"d");
        assert_eq!(
            chunks,
            vec![b"a".to_vec(), b"bc".to_vec(), b"d".to_vec()],
            "each delivery is one whole write"
        );
        sink.flush().unwrap();
        assert_eq!(
            chunks.concat(),
            b"abcd".to_vec(),
            "order and content survive 1-byte sealing"
        );
    }

    #[test]
    fn arbitrary_write_shapes_seal_exactly_at_the_bound() {
        // Caps ABOVE the largest piece: every piece buffers, so the exact
        // sealing arithmetic is visible. (A cap at or below a piece's size
        // sends that piece direct — the dedicated 1-byte test covers it.)
        for cap in [4usize, 5, 7, 64] {
            let mut errors = Vec::new();
            let mut chunks: Chunks = Vec::new();
            let ctx = &raw mut chunks;
            let mut sink = make(&mut errors, cap, ctx);
            let payload: Vec<u8> = (0..50u8).collect();
            // Writes of awkward sizes: none a divisor of 50 except by luck.
            let mut written_total = 0usize;
            for piece in payload.chunks(3) {
                written_total += sink.write(piece).unwrap();
                // Between writes, only ONE open buffer exists and it is
                // never past the bound.
                assert!(sink.buf.len() < cap || sink.buf.is_empty());
            }
            sink.flush().unwrap();
            assert_eq!(written_total, 50);
            assert_eq!(chunks.concat(), payload, "cap {cap} preserves bytes");
            // Every sealed chunk but the last is EXACTLY the bound.
            for chunk in &chunks[..chunks.len() - 1] {
                assert_eq!(chunk.len(), cap, "cap {cap} seals exactly");
            }
            assert!(!chunks.last().unwrap().is_empty());
        }
    }

    #[test]
    fn an_oversized_write_on_an_open_buffer_splits_into_sealed_chunks() {
        let mut errors = Vec::new();
        let mut chunks: Chunks = Vec::new();
        let ctx = &raw mut chunks;
        let mut sink = make(&mut errors, 8, ctx);
        let _ = sink.write(b"12345"); // open buffer: 5/8
        let big: Vec<u8> = (0..20u8).collect();
        let _ = sink.write(&big);
        sink.flush().unwrap();
        assert_eq!(chunks[0].len(), 8);
        assert_eq!(chunks[1].len(), 8);
        assert_eq!(chunks[2].len(), 8);
        assert_eq!(chunks[3].len(), 5 + 20 - 24, "the remainder lands once");
        assert_eq!(chunks.concat()[..5], *b"12345", "the first write's bytes stay first");
    }

    #[test]
    fn a_cancel_verdict_stops_publication_and_marks_the_sink() {
        let mut errors = Vec::new();
        let mut chunks: Chunks = Vec::new();
        let ctx = &raw mut chunks;
        let mut sink = StreamingSink {
            errors: &mut errors,
            chunk: cancel_second,
            context: ctx.cast::<c_void>(),
            buf: Vec::with_capacity(4),
            chunk_cap: 4,
            delivered: 0,
            cancelled: false,
        };
        let _ = sink.write(b"aaaa"); // delivery #1: continue
        let second = sink.write(b"bbbb"); // delivery #2: the verdict is CANCEL
        assert_eq!(second, Err("stream cancelled by host"));
        assert!(sink.cancelled);
        // A cancelled sink refuses further writes without calling out.
        assert_eq!(sink.write(b"c"), Err("stream cancelled by host"));
        // The cancelling callback still RECEIVED its bytes before returning
        // the verdict (a consumer keeps what crossed); delivered does not
        // count them.
        assert_eq!(
            chunks,
            vec![b"aaaa".to_vec(), b"bbbb".to_vec()],
            "two deliveries, then the stop"
        );
        assert_eq!(sink.delivered, 4, "the cancelled delivery is not counted");
    }

    #[test]
    fn an_empty_flush_delivers_nothing() {
        let mut errors = Vec::new();
        let mut chunks: Chunks = Vec::new();
        let ctx = &raw mut chunks;
        let mut sink = make(&mut errors, 16, ctx);
        sink.flush().unwrap();
        assert!(chunks.is_empty(), "no phantom empty callback");
    }

    #[test]
    fn recycle_beats_fresh_capacity_on_many_seals() {
        const ROUNDS: usize = 15;
        const SEALS: usize = 800;
        const CAP: usize = 256 * 1024;
        fn median(mut samples: Vec<u128>) -> u128 {
            samples.sort_unstable();
            samples[samples.len() / 2]
        }
        unsafe extern "C" fn drop_chunk(_c: *mut c_void, _b: *const u8, _n: usize) -> c_int {
            JQF_STREAM_CONTINUE
        }
        let payload = vec![0u8; CAP];
        let mut old_samples = Vec::with_capacity(ROUNDS);
        let mut new_samples = Vec::with_capacity(ROUNDS);
        for _ in 0..ROUNDS {
            let mut errors = Vec::new();
            let mut sink = StreamingSink {
                errors: &mut errors,
                chunk: drop_chunk,
                context: core::ptr::null_mut(),
                buf: Vec::with_capacity(CAP),
                chunk_cap: CAP,
                delivered: 0,
                cancelled: false,
            };
            let start = std::time::Instant::now();
            for _ in 0..SEALS {
                let _ = sink.write(&payload);
            }
            new_samples.push(start.elapsed().as_nanos());

            let mut buf = Vec::with_capacity(CAP);
            let start = std::time::Instant::now();
            for _ in 0..SEALS {
                buf.extend_from_slice(&payload);
                let sealed = std::mem::take(&mut buf);
                let _ = unsafe { drop_chunk(core::ptr::null_mut(), sealed.as_ptr(), sealed.len()) };
                buf = Vec::with_capacity(CAP);
            }
            old_samples.push(start.elapsed().as_nanos());
        }
        let old_med = median(old_samples);
        let new_med = median(new_samples);
        eprintln!("lane=stream_buf seals={SEALS} rounds={ROUNDS} fresh_ns={old_med} recycle_ns={new_med}");
        assert!(
            new_med <= old_med,
            "recycle must not be slower than fresh with_capacity"
        );
    }
}
