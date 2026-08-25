//! The RSS governor: measure, then enforce, then default-on.
//!
//! This module owns the whole pillar: the mimalloc stats shim, the physical memory detection, the ceiling resolution,
//! the `Control` implementation the ledger consults at its decision points, and the `--diagnostics` report.
//!
//! # Why it lives here and not in a library crate
//!
//! The governor's enforcement number is the process's CURRENT resident footprint — the allocator's own on macOS, the OS
//! working set on Linux (see the enforcement law below) — and the allocator is a whole-program property declared in
//! `src/allocator.rs`. The library crates are `no_std` and allocator-agnostic by law, so the only crate that may name
//! mimalloc is this one. `jqf-resource` carries the observation seam — [`ControlOutcome:MemoryExceeded`] — and nothing
//! else: it never names a provider, a counter, or a unit. The DECISION (what "exceeded" means, how it is measured, and
//! when to refuse) is entirely this module's.
//!
//! # The three waves
//!
//! W1 (measure): `--diagnostics` prints `jqf: rss: …` with the current footprint (`current_rss` — the OS RSS via
//! `/proc/self/statm` on Linux, mimalloc's precise number elsewhere), the allocator's peak/commit/ page-fault counters
//! where mimalloc provides them, the OS peak (`maxrss` from `getrusage`), the retained input split (`retained_input` —
//! the "N of your RSS is the input" fact), and the active ceiling; the report names its `current_rss` source. W2
//! (enforce): the ceiling is sampled at the ledger's decision points, a release step is forced before any refusal, and
//! the refusal is its own diag code (`MACHINE_MEMORY`, distinct from the accounted rejection's `MACHINE_RESOURCE`). W3
//! (default-on): `min(80% of physical RAM, the cgroup/job limit when detectable)` is the DEFAULT ceiling — see
//! [`configure`] — with `--max-rss N|N%|0` as the dial.
//!
//! # The enforcement number, per platform
//!
//! mimalloc's own FFI documentation states: "`current_rss` is precise on Windows and `MacOSX`; other systems estimate
//! this using `current_commit`." Linux — the primary deployment target — would therefore report an ESTIMATE: mimalloc's
//! read/write-accessible reserved bytes, a lower bound of the process's true resident set (the retained input, code
//! pages, and libc are on top). The policy, decided here and enforced by construction:
//!
//! - On LINUX the enforcement number is the OS current RSS, read directly
//!   from `/proc/self/statm`. That is what a memory ceiling should bound, and it works with ANY allocator: a
//!   system-allocator build has no in-process counters, and the ceiling must survive without them. A refusal there is a
//!   statement about the process's working set, never a guess about the allocator's reserved footprint.
//! - On macOS (and every other non-Linux platform) the enforcement number
//!   stays the ALLOCATOR's footprint — mimalloc's precise `current_rss` — and a system-allocator build, having no
//!   counters, degrades to measure-only (the degradation section below). macOS behavior is the constant; the statm path
//!   is the Linux refinement the estimate policy always earmarked.
//! - The OS peak (`getrusage`'s `ru_maxrss`) is reported beside the current
//!   footprint on every `--diagnostics` run, and the RSS gate's physical-report lanes cross-check the two stay within a
//!   committed factor — the allocator footprint drifts from the OS truth for a REASON (page quantization, mmap'd input
//!   the allocator never sees), and the gate makes that reason visible instead of letting the policy be an accident of
//!   which platform was tested on. The gate measures on macOS, where the numbers are mimalloc's.
//!
//! # Sampling: the ledger's decision points, time-gated
//!
//! No watchdog thread, no per-allocation sampling. The governor is the request's [`Control`] implementation, so it is
//! consulted exactly where the ledger already yields to the host: at cooperative generator resumptions, at work-slice
//! exhaustion (every 4096 work transitions), and — since the plan's "frequent exactly when growth is fast" claim was
//! FALSE for collecting drives, which ran an unbounded collect inside one poll — at every graph-machine frame task (see
//! the engine's `GraphMachine:advance` metering change, which makes each collect push / member insert / loop step a
//! metered transition). The CLI additionally samples at its own decision points: after the input read, after compile,
//! and before each route attempt.
//!
//! The actual footprint read (statm on Linux, `mi_process_info` elsewhere) is time-gated to one sample per
//! [`SAMPLE_INTERVAL`] so the hot path pays one atomic load and one clock read between samples. The overshoot bound
//! this yields — one sample interval of growth plus one work slice of elements — is pinned by the RSS gate's overshoot
//! lane at ceiling × 1.25 (plan A4).
//!
//! # The grace step and the refusal
//!
//! On an over-ceiling observation the governor first forces the allocator to return freed pages to the OS (mimalloc's
//! `mi_collect` where mimalloc is linked, glibc's `malloc_trim` on a glibc Linux system-allocator build — the grace
//! step is allocator-neutral, [`collect_and_release`]), re-reads, and only then declines if the footprint is STILL
//! over. This is what makes a default-on ceiling rare: a run whose accounted memory was released but not yet returned
//! to the OS completes instead of refusing. The refusal is a machine-class failure ([`ControlError:MemoryExceeded`],
//! diag code `MACHINE_MEMORY`) — never catch-eligible, never per-value — and its message names the measured RSS, the
//! ceiling, the ceiling's provenance, the retained-input split, and the override flag.
//!
//! # The ceiling sizing formula
//!
//! `ceiling = 80% × effective_memory`, where `effective_memory = min(physical RAM, cgroup/job limit when detectable)`.
//! The plan's confirmed wording was "min(80% of detected physical RAM, cgroup/job limit when detectable)"; the 80%
//! factor is applied to the cgroup term too, and this is a deliberate, written deviation: in a container the literal
//! formula puts the refusal AT the kernel's hard limit, where the sampling overshoot between ceiling and refusal is
//! exactly the OOM-kill — the brick the pillar exists to prevent. Applying the factor to the effective memory keeps the
//! refusal BELOW the kill point, which is what makes the 1.25× overshoot bound and "cannot OOM-kill" consistent with
//! each other. On a host without a detectable cgroup the two formulas are identical. Detection never guesses: any
//! failure degrades to measure-only with a warning (or, under the `JQF_MEMORY_DETECT_DIR` test instrument, fails loud).
//!
//! # Degradation
//!
//! With the system allocator (`--no-default-features`) there are no in-process counters: on Linux the statm read keeps
//! the ceiling ENFORCED (the whole point of the Linux branch of the enforcement law), while everywhere else
//! `current_rss` is unknown and the ceiling degrades to measure-only (a warning names it). The report prints `na` for
//! the mimalloc fields where they have no source — never a wrong number — and names where `current_rss` came from.
//! `maxrss` and `retained_input` are always reported.

use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

use jqf_codec_core::CodecFailureKind;
use jqf_resource::{Control, ControlError, ControlOutcome};

use crate::errors::CliFailure;

/// One sample per this many milliseconds. The `mi_process_info` read is a few atomic loads, but a tight loop calling it
/// per work slice would still cost a measurable fraction; the gate bounds the real reads while the ledger's decision
/// points bound the OBSERVATION LATENCY. The overshoot budget this constant consumes is pinned by the gate's overshoot
/// lane (plan A4).
const SAMPLE_INTERVAL_MS: u64 = 4;

/// One clock read per this many non-forced checks. The engine consults the governor on every cooperative generator
/// resumption — millions of times a second in a tight loop — and the `Instant:now` + millisecond-division pair was a
/// measured ~14% of a pure-counter lane. At that cadence a 64-check stride still reads the clock every few
/// microseconds, three orders of magnitude inside [`SAMPLE_INTERVAL_MS`], so the A4 overshoot bound (one sample
/// interval plus one work slice) is unchanged. A power of two so the gate compiles to a mask.
const CHECK_STRIDE: u32 = 64;

/// The default ceiling, as a percent of the effective memory (see the module doc for the formula and its written
/// deviation).
pub(crate) const DEFAULT_CEILING_PERCENT: u64 = 80;

/// The `--diagnostics` line's prefix; the gate parses it.
pub(crate) const REPORT_PREFIX: &str = "jqf: rss:";

/// The dial the CLI accepts: bytes, percent-of-effective-memory, or disabled.
///
/// Parsing lives in `args.rs`; the resolution to a concrete byte ceiling lives here, beside the detection it needs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MaxRss {
    /// `--max-rss 0`: no ceiling; the report still measures.
    Disabled,
    /// `--max-rss N`: an absolute byte ceiling, no detection involved.
    Bytes(u64),
    /// `--max-rss N%`: N percent of the detected effective memory (physical RAM, or the cgroup limit when it binds).
    Percent(u64),
}

/// The governor's mutable state, all atomics so the request thread's `check` and the main thread's renderer can share
/// it without a lock.
struct GovernorState {
    /// The active ceiling in bytes; 0 when disabled. `u64:MAX` is never a ceiling (a percent ceiling is at most 1000×
    /// the effective memory, and the effective memory is far below that).
    ceiling: AtomicU64,
    /// Whether an over-ceiling observation must refuse. False when the dial is off, detection failed, or the allocator
    /// has no counters.
    enforcing: AtomicBool,
    /// Monotonic milliseconds of the last real sample (time gate).
    last_sample_ms: AtomicU64,
    /// The retained input bytes (the `max_input_bytes`-bound term the plan's 001 table blames for the 122×–1004×
    /// accounted-vs-RSS ratios).
    retained_input: AtomicU64,
    /// The refusal record the renderer reads: RSS at the refusal and the ceiling that fired. Zero until a refusal
    /// happened (a refusal is terminal, so the record cannot go stale).
    refusal_rss: AtomicU64,
    refusal_ceiling: AtomicU64,
    /// Whether the run-end `--diagnostics` report is enabled for this process.
    report_enabled: AtomicBool,
    /// The ambient (counting-allocator) ledger's peak and live-at-scope-end committed bytes, captured on the REQUEST
    /// thread just before the ambient scope guard restores its thread-local. Stored here because the report prints on
    /// the main thread after the request thread joins, where that thread-local no longer exists.
    ambient_peak: AtomicU64,
    ambient_current: AtomicU64,
    /// Whether `check` may refuse at all — the time-gate gate's own switch, so `--max-rss 0` costs one load per
    /// decision point and nothing else.
    sampling_enabled: AtomicBool,
    /// Non-forced check counter for the [`CHECK_STRIDE`] gate, so the hot path's common case is one relaxed RMW rather
    /// than a clock read.
    checks: AtomicU32,
}

impl GovernorState {
    const fn new() -> Self {
        Self {
            ceiling: AtomicU64::new(0),
            enforcing: AtomicBool::new(false),
            last_sample_ms: AtomicU64::new(0),
            retained_input: AtomicU64::new(0),
            refusal_rss: AtomicU64::new(0),
            refusal_ceiling: AtomicU64::new(0),
            report_enabled: AtomicBool::new(false),
            ambient_peak: AtomicU64::new(0),
            ambient_current: AtomicU64::new(0),
            sampling_enabled: AtomicBool::new(false),
            checks: AtomicU32::new(0),
        }
    }
}

static STATE: GovernorState = GovernorState::new();
/// The provenance of the active ceiling, rendered in refusals and warnings. Set once at configure; `OnceLock` so the
/// renderer cannot read a half-written string.
static PROVENANCE: OnceLock<String> = OnceLock::new();
/// A process-start clock for the time gate's millisecond deltas.
static ZERO: OnceLock<Instant> = OnceLock::new();

/// The request's `Control` implementation: delegates cancellation/deadline to the neutral default and adds the
/// physical-memory observation.
pub(crate) struct Governor;

impl Control for Governor {
    fn check(&self) -> ControlOutcome {
        sample_and_maybe_refuse(false)
    }
}

pub(crate) static GOVERNOR: Governor = Governor;

/// One full physical footprint sample.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PhysicalSample {
    /// The current resident footprint — the enforcement number. On Linux the OS current RSS from `/proc/self/statm`;
    /// elsewhere mimalloc's `current_rss` (precise on macOS/Windows). `None` where neither source exists (a
    /// system-allocator build on a non-Linux platform).
    pub current_rss: Option<u64>,
    /// mimalloc's peak working set; only reported where mimalloc's numbers are the enforcement source, i.e. never on
    /// Linux.
    pub peak_rss: Option<u64>,
    pub current_commit: Option<u64>,
    pub peak_commit: Option<u64>,
    pub page_faults: Option<u64>,
    /// `getrusage`'s `ru_maxrss` converted to BYTES. Linux reports the field in KiB (this path multiplies by 1024);
    /// macOS reports bytes already and is stored as-is.
    pub maxrss: Option<u64>,
}

/// Reads one full sample for the run-end report: the enforcement number's current value, the allocator's own counters
/// where they are the enforcement source, and the OS peak. The `getrusage` read lives HERE and not in the enforcement
/// path — a `check` never pays it.
///
/// The per-build cfg arms fill different field sets, so the default struct is mutated rather than written as one
/// struct-update literal (which would have to be duplicated per arm).
#[allow(clippy::field_reassign_with_default)]
pub(crate) fn sample() -> PhysicalSample {
    let mut sample = PhysicalSample::default();
    #[cfg(target_os = "linux")]
    {
        sample.current_rss = statm_rss_bytes();
        // The peak/commit/page-fault fields stay `None` on Linux: the report's rss source is statm's OS RSS (see
        // `current_rss`), and mimalloc's estimate next to it would read as a contradiction.
    }
    #[cfg(all(not(target_os = "linux"), feature = "mimalloc"))]
    {
        let (current_rss, peak_rss, current_commit, peak_commit, page_faults) = mimalloc_footprint();
        sample.current_rss = Some(current_rss);
        sample.peak_rss = Some(peak_rss);
        sample.current_commit = Some(current_commit);
        sample.peak_commit = Some(peak_commit);
        sample.page_faults = Some(page_faults);
    }
    sample.maxrss = os_maxrss_bytes();
    sample
}

/// The enforcement number: the process's CURRENT resident footprint in bytes. Platform-split by construction (the
/// module doc's enforcement law): Linux reads the OS working set from `/proc/self/statm` — the ceiling must survive
/// WITHOUT mimalloc counters, and a system-allocator build has none — while everywhere else it is the allocator's own
/// current footprint (mimalloc's precise `current_rss` on macOS/Windows).
///
/// The `Option` is a cross-build-config contract: the `None` arm is compiled out on this macOS/mimalloc build, but
/// exists on the system-allocator build, where an unmeasurable allocator must degrade to measure-only.
#[allow(clippy::unnecessary_wraps)]
fn current_rss() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        statm_rss_bytes()
    }
    #[cfg(all(not(target_os = "linux"), feature = "mimalloc"))]
    {
        Some(mimalloc_footprint().0)
    }
    #[cfg(all(not(target_os = "linux"), not(feature = "mimalloc")))]
    {
        None
    }
}

/// `mi_process_info`'s five footprint fields; the elapsed/user/system time triple is not read. One call fills the whole
/// report sample on non-Linux mimalloc builds and the enforcement read on macOS. Linux never calls it: the enforcement
/// number there is statm's OS RSS, and the report shows no mimalloc counters next to it.
#[cfg(all(not(target_os = "linux"), feature = "mimalloc"))]
fn mimalloc_footprint() -> (u64, u64, u64, u64, u64) {
    let mut current_rss = 0usize;
    let mut peak_rss = 0usize;
    let mut current_commit = 0usize;
    let mut peak_commit = 0usize;
    let mut page_faults = 0usize;
    // SAFETY: every out-pointer is a valid, aligned `usize` for the whole call; the time triple is null (mimalloc
    // guards every out-pointer).
    unsafe {
        libmimalloc_sys::mi_process_info(
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            &raw mut current_rss,
            &raw mut peak_rss,
            &raw mut current_commit,
            &raw mut peak_commit,
            &raw mut page_faults,
        );
    }
    (
        current_rss as u64,
        peak_rss as u64,
        current_commit as u64,
        peak_commit as u64,
        page_faults as u64,
    )
}

/// The Linux enforcement number: the OS resident set from `/proc/self/statm` (field 2, resident pages) scaled by the
/// page size. One tiny file read per sample, inside the time gate; the measure-only path never pays it.
#[cfg(target_os = "linux")]
fn statm_rss_bytes() -> Option<u64> {
    let statm = fs::read_to_string("/proc/self/statm").ok()?;
    let pages = statm.split_whitespace().nth(1)?.parse::<u64>().ok()?;
    // SAFETY: `sysconf` takes an int and returns -1 on error; the page size is positive on every Linux we deploy to,
    // and 4096 is the harmless fallback when it is not.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let page_size = u64::try_from(page_size).ok().filter(|&size| size > 0).unwrap_or(4096);
    Some(pages.saturating_mul(page_size))
}

/// Forces the allocator to return freed pages to the OS (the grace step). Allocator-neutral: mimalloc's `mi_collect`
/// where mimalloc is linked, and glibc's `malloc_trim` on a glibc Linux system-allocator build (the allocator the
/// `--no-default-features` Linux binary links); otherwise there is nothing this module may release.
fn collect_and_release() {
    #[cfg(feature = "mimalloc")]
    {
        // SAFETY: `mi_collect` is thread-safe and takes no pointers.
        unsafe { libmimalloc_sys::mi_collect(true) };
    }
    #[cfg(all(not(feature = "mimalloc"), target_os = "linux", target_env = "gnu"))]
    {
        // SAFETY: `malloc_trim` takes a plain `size_t` and has no preconditions.
        unsafe { libc::malloc_trim(0) };
    }
}

/// `getrusage`'s `ru_maxrss` in bytes. The unit is platform-divergent: the BSDs (macOS included) report BYTES, Linux
/// reports KIBIBYTES — the very platform split this module's doc says must be explicit rather than an accident of where
/// the code was tested.
fn os_maxrss_bytes() -> Option<u64> {
    #[cfg(unix)]
    {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
        // SAFETY: `getrusage` writes the whole struct through the pointer.
        let result = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
        if result == 0 {
            // SAFETY: the syscall succeeded, so the struct is initialized.
            let usage = unsafe { usage.assume_init() };
            let value = u64::try_from(usage.ru_maxrss).ok()?;
            return Some(if cfg!(target_os = "linux") {
                value.saturating_mul(1024)
            } else {
                value
            });
        }
    }
    None
}

fn now_millis() -> u64 {
    let zero = ZERO.get_or_init(Instant::now);
    u64::try_from(zero.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// The ledger's decision-point entry: sample (time-gated) and refuse when the grace step cannot bring the footprint
/// under the ceiling.
///
/// `force` skips the time gate — the CLI's own decision points (after the input read, after compile, before a route)
/// must not wait for the gate.
fn sample_and_maybe_refuse(force: bool) -> ControlOutcome {
    // Refusal is the whole job of a `check`: with no enforced ceiling (dial off, detection failed, or an unmeasurable
    // allocator) the hot path returns before paying a footprint read at all. The enforcing check comes FIRST, before
    // the stride/time gates, so a measure-only request costs one load per decision point and nothing else.
    if !STATE.enforcing.load(Ordering::Relaxed) {
        return ControlOutcome::Continue;
    }
    if !STATE.sampling_enabled.load(Ordering::Relaxed) {
        return ControlOutcome::Continue;
    }
    if !force {
        // The stride gate first: reading the clock itself is the hot path's dominant cost at engine-check cadence, so
        // only every [`CHECK_STRIDE`]th check pays it. Wrapping is harmless — one early clock read every 2^32 checks.
        if !STATE
            .checks
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(CHECK_STRIDE)
        {
            return ControlOutcome::Continue;
        }
        let now = now_millis();
        let last = STATE.last_sample_ms.load(Ordering::Relaxed);
        if now.wrapping_sub(last) < SAMPLE_INTERVAL_MS {
            return ControlOutcome::Continue;
        }
        // A racy store is fine: the worst case is one extra sample.
        STATE.last_sample_ms.store(now, Ordering::Relaxed);
    }
    let Some(footprint) = current_rss() else {
        // No measurable footprint: measure-only, never a wrong number.
        return ControlOutcome::Continue;
    };
    let ceiling = STATE.ceiling.load(Ordering::Relaxed);
    if footprint <= ceiling {
        return ControlOutcome::Continue;
    }
    // The grace step: freed pages may simply not have been returned yet.
    collect_and_release();
    let after = current_rss().unwrap_or(footprint);
    if after <= ceiling {
        return ControlOutcome::Continue;
    }
    STATE.refusal_rss.store(after, Ordering::Relaxed);
    STATE.refusal_ceiling.store(ceiling, Ordering::Relaxed);
    ControlOutcome::MemoryExceeded
}

/// Returns freed pages to the OS at a session/cycle boundary (the D3 law's release half): a resident drive's
/// reservations drop when the drive ends, and this step makes that drop OBSERVABLE — without it the allocator keeps the
/// pages a large drive freed, so the daemon's RSS stays elevated and a later session that never needed the memory is
/// refused by a footprint it did not allocate. Called by the serve accept loop at every session end and by the follow
/// cycle drive on an over-ceiling cycle.
pub(crate) fn release_freed_pages() {
    collect_and_release();
}

/// Whether a CLI failure is the RSS governor's refusal — the D3 law's classification: a serve session (or one
/// `--follow` cycle) that crosses the physical ceiling fails with exactly this failure, and the resident routes catch
/// exactly it to end the session/cycle while the daemon (or the tail) survives. Every other failure stays terminal on
/// those routes.
pub(crate) fn is_memory_refusal(failure: &CliFailure) -> bool {
    matches!(
        failure,
        CliFailure::Codec {
            kind: jqf_codec_core::CodecFailureKind::Control(jqf_resource::ControlError::MemoryExceeded),
            ..
        }
    )
}

/// The hard byte cap on one unterminated held record: the armed RSS ceiling when the governor is enforcing, otherwise
/// none. A terminator-less client is refused at this cap instead of growing the retained buffer past the dial. Complete
/// records are not charged here — they drain at the cycle boundary; only the held tail is.
pub(crate) fn held_record_cap() -> Option<u64> {
    if !STATE.enforcing.load(Ordering::Relaxed) {
        return None;
    }
    let ceiling = STATE.ceiling.load(Ordering::Relaxed);
    (ceiling > 0).then_some(ceiling)
}

/// One CLI decision point: forces a sample and fails the request on a confirmed over-ceiling footprint.
pub(crate) fn check_point() -> Result<(), CliFailure> {
    match sample_and_maybe_refuse(true) {
        ControlOutcome::MemoryExceeded => Err(refusal_failure()),
        // This governor never reports cancellation or deadline; Continue is the only other outcome it can produce.
        ControlOutcome::Continue | ControlOutcome::Cancelled | ControlOutcome::DeadlineExceeded => Ok(()),
    }
}

/// The typed failure the renderer recognizes: `MACHINE_MEMORY`, distinct from the accounted rejection's
/// `MACHINE_RESOURCE`.
pub(crate) fn refusal_failure() -> CliFailure {
    CliFailure::Codec {
        kind: CodecFailureKind::Control(ControlError::MemoryExceeded),
        diagnostic: None,
    }
}

/// The actionable refusal text: measured RSS, the ceiling, its provenance, the retained-input split, and the exact
/// override flag. Reads the refusal record the check wrote; the fallback text is the `ControlError`'s own.
pub(crate) fn refusal_message() -> String {
    let rss = STATE.refusal_rss.load(Ordering::Relaxed);
    let ceiling = STATE.refusal_ceiling.load(Ordering::Relaxed);
    let mut message = String::new();
    if rss == 0 {
        message.push_str("physical memory ceiling exceeded");
        return message;
    }
    let provenance = PROVENANCE.get().map_or("unknown", String::as_str);
    let retained = STATE.retained_input.load(Ordering::Relaxed);
    let _ = write!(
        message,
        "physical memory ceiling exceeded: the process resident set is {rss} bytes, \
         over the {ceiling}-byte ceiling ({provenance}); freed pages were released and \
         the footprint re-measured before declining"
    );
    if retained > 0 {
        let _ = write!(message, "; the retained input is {retained} bytes of that");
    }
    message.push_str("; raise the ceiling with --max-rss N|N% or disable it with --max-rss 0");
    message
}

/// Records how much of the footprint is the retained input (the plan's §4 attribution split). Set after the eager input
/// read and the binding files.
pub(crate) fn set_retained_input(bytes: u64) {
    STATE.retained_input.store(bytes, Ordering::Relaxed);
}

pub(crate) fn set_report_enabled(enabled: bool) {
    STATE.report_enabled.store(enabled, Ordering::Relaxed);
}

/// Records the ambient ledger's peak and live committed bytes, captured on the request thread as the ambient scope
/// guard drops. The main thread's report reads these after the request thread joins.
pub(crate) fn set_ambient(peak: u64, current: u64) {
    STATE.ambient_peak.store(peak, Ordering::Relaxed);
    STATE.ambient_current.store(current, Ordering::Relaxed);
}

/// The `--diagnostics` report line, printed once per request after the run. `na` = the field has no source on this
/// build; `off` = no ceiling is enforced. `rss_source` names where `current_rss` came from (statm on Linux, mimalloc
/// elsewhere) so the line never lets a reader assume a mimalloc number where statm was used.
pub(crate) fn report_line() -> String {
    let sample = sample();
    let retained = STATE.retained_input.load(Ordering::Relaxed);
    let ceiling = STATE.ceiling.load(Ordering::Relaxed);
    let mut line = String::from(REPORT_PREFIX);
    let _ = write!(
        line,
        " current_rss={} peak_rss={} current_commit={} peak_commit={} page_faults={} maxrss={} rss_source={}",
        sample.current_rss.map_or_else(|| "na".to_owned(), |v| v.to_string()),
        sample.peak_rss.map_or_else(|| "na".to_owned(), |v| v.to_string()),
        sample.current_commit.map_or_else(|| "na".to_owned(), |v| v.to_string()),
        sample.peak_commit.map_or_else(|| "na".to_owned(), |v| v.to_string()),
        sample.page_faults.map_or_else(|| "na".to_owned(), |v| v.to_string()),
        sample.maxrss.map_or_else(|| "na".to_owned(), |v| v.to_string()),
        rss_source_name(),
    );
    let ceiling_text = if ceiling == 0 {
        "off".to_owned()
    } else {
        ceiling.to_string()
    };
    let _ = write!(line, " retained_input={retained} ceiling={ceiling_text}");
    line
}

/// Where the report's `current_rss` came from — the provenance the `--diagnostics` text must name (the enforcement law
/// is a per-platform fact, never an accident of which machine the code ran on).
const fn rss_source_name() -> &'static str {
    if cfg!(target_os = "linux") {
        "statm"
    } else if cfg!(feature = "mimalloc") {
        "mimalloc"
    } else {
        "na"
    }
}

/// Prints the report line when this request enabled `--diagnostics` (and only then; `--explain` alone stays JSON-free
/// and counter-free).
pub(crate) fn print_report_if_enabled() {
    if STATE.report_enabled.load(Ordering::Relaxed) {
        crate::eprint_line_buffered(&report_line());
        crate::eprint_line_buffered(&ledger_report_line());
    }
}

/// The ambient ledger line: what the counting allocator charged, and whether a memory ceiling is enforceable at all
/// (true only when a [`CountingAlloc`](jqf_resource:CountingAlloc) is the process allocator). `ambient` is the PEAK
/// committed bytes the allocator observed during the request; `current` is the live-at-scope-end count (near zero — the
/// request frees what it built). Both are captured on the request thread before the scope drops and carried here across
/// the thread join.
pub(crate) fn ledger_report_line() -> String {
    let peak = STATE.ambient_peak.load(Ordering::Relaxed);
    let current = STATE.ambient_current.load(Ordering::Relaxed);
    let enforced = jqf_resource::ceiling_enforced();
    format!("jqf: ledger: ambient={peak} current={current} enforced={enforced}")
}

/// Whether the ceiling can be enforced on this build. Linux always: the statm read measures the OS working set with ANY
/// allocator. Everywhere else only with mimalloc's in-process counters.
fn enforcement_available() -> bool {
    cfg!(target_os = "linux") || cfg!(feature = "mimalloc")
}

/// Resolves the dial to a concrete ceiling and arms the governor.
///
/// `Bytes` never needs detection. `Percent` (including the default) resolves against the detected effective memory; a
/// detection failure degrades to measure-only with a warning — except under the `JQF_MEMORY_DETECT_DIR` test
/// instrument, where it fails loud so a test cannot silently measure nothing. With the system allocator the ceiling can
/// be enforced only on Linux (the statm read needs no counters); elsewhere that is a documented degradation to
/// measure-only, not a wrong number.
pub(crate) fn configure(max_rss: MaxRss, explicit: bool) -> Result<(), CliFailure> {
    let (ceiling, enforcing, provenance) = match max_rss {
        MaxRss::Disabled => (0, false, "disabled by --max-rss 0".to_owned()),
        MaxRss::Bytes(bytes) => (bytes, true, format!("user: --max-rss {bytes}")),
        MaxRss::Percent(percent) => match detect_effective_memory() {
            Ok(memory) => {
                let Some(effective) = memory.effective_bytes() else {
                    degrade("no physical memory is detectable; the memory ceiling is disabled (measure-only)");
                    return Ok(());
                };
                let ceiling = effective.saturating_mul(percent) / 100;
                let source = match (memory.host_bytes, memory.cgroup_bytes) {
                    (_, Some(cgroup)) if cgroup < memory.host_bytes.unwrap_or(u64::MAX) => "cgroup memory limit",
                    _ => "physical RAM",
                };
                let provenance = if explicit {
                    format!("user: --max-rss {percent}% of {effective} bytes ({source})")
                } else {
                    format!("default: {percent}% of {effective} bytes ({source})")
                };
                (ceiling, true, provenance)
            }
            Err(message) if test_instrument_set() => {
                return Err(CliFailure::from(format!("JQF_MEMORY_DETECT_DIR: {message}")));
            }
            Err(message) => degrade(&message),
        },
    };
    #[cfg(not(any(target_os = "linux", feature = "mimalloc")))]
    if enforcing {
        crate::eprint_line_buffered(
            "jqf: warning: the memory ceiling cannot be enforced with the system allocator \
             (no in-process counters); running measure-only",
        );
    }
    STATE.ceiling.store(ceiling, Ordering::Relaxed);
    STATE
        .enforcing
        .store(enforcing && enforcement_available(), Ordering::Relaxed);
    STATE.sampling_enabled.store(true, Ordering::Relaxed);
    let _ = PROVENANCE.set(provenance);
    Ok(())
}

fn degrade(warning: &str) -> (u64, bool, String) {
    crate::eprint_line_buffered(&format!("jqf: warning: {warning}"));
    (0, false, "measure-only".to_owned())
}

fn test_instrument_set() -> bool {
    std::env::var_os("JQF_MEMORY_DETECT_DIR").is_some()
}

/// The detected memory facts.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct DetectedMemory {
    /// Physical RAM in bytes.
    pub host_bytes: Option<u64>,
    /// The effective cgroup/job memory limit in bytes, when detectable.
    pub cgroup_bytes: Option<u64>,
}

impl DetectedMemory {
    /// `min(physical RAM, cgroup limit)` — the memory the process can actually use (a container's "physical RAM" IS its
    /// cgroup limit).
    pub fn effective_bytes(&self) -> Option<u64> {
        match (self.host_bytes, self.cgroup_bytes) {
            (Some(host), Some(cgroup)) => Some(host.min(cgroup)),
            (Some(host), None) => Some(host),
            (None, Some(cgroup)) => Some(cgroup),
            (None, None) => None,
        }
    }
}

/// Detects the effective memory. Production failures degrade (the caller warns); failures under the
/// `JQF_MEMORY_DETECT_DIR` instrument are hard.
fn detect_effective_memory() -> Result<DetectedMemory, String> {
    if let Some(dir) = std::env::var_os("JQF_MEMORY_DETECT_DIR") {
        return detect_from_override(&PathBuf::from(dir));
    }
    let host = detect_host_ram();
    let cgroup = detect_cgroup_limit();
    Ok(DetectedMemory {
        host_bytes: host,
        cgroup_bytes: cgroup,
    })
}

/// The test instrument: a fake filesystem mirror of the production reads, so the cgroup-follows law is provable on any
/// platform (macOS included, which has no cgroups). Fail-loud: a test that points here and gets a degraded governor
/// would otherwise pass vacuously.
fn detect_from_override(dir: &std::path::Path) -> Result<DetectedMemory, String> {
    let meminfo_path = dir.join("meminfo");
    let meminfo = fs::read_to_string(&meminfo_path)
        .map_err(|error| format!("cannot read {}: {error}", meminfo_path.display()))?;
    let host = parse_meminfo(&meminfo).ok_or_else(|| format!("no MemTotal line in {}", meminfo_path.display()))?;
    let cgroup_root = dir.join("cgroup");
    let mut cgroup = None;
    let v2_path = cgroup_root.join("memory.max");
    if let Ok(text) = fs::read_to_string(&v2_path) {
        let trimmed = text.trim();
        if trimmed != "max" {
            cgroup = Some(
                trimmed
                    .parse::<u64>()
                    .map_err(|_| format!("bad memory.max value {trimmed:?} in {}", v2_path.display()))?,
            );
        }
    }
    let v1_path = cgroup_root.join("memory").join("memory.limit_in_bytes");
    if let Ok(text) = fs::read_to_string(&v1_path) {
        let value = text
            .trim()
            .parse::<u64>()
            .map_err(|_| format!("bad memory.limit_in_bytes in {}", v1_path.display()))?;
        if value < (1u64 << 60) {
            cgroup = Some(match cgroup {
                Some(existing) => existing.min(value),
                None => value,
            });
        }
    }
    Ok(DetectedMemory {
        host_bytes: Some(host),
        cgroup_bytes: cgroup,
    })
}

/// Physical RAM: `sysctl hw.memsize` on macOS, `/proc/meminfo` on Linux.
fn detect_host_ram() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        let mut size: u64 = 0;
        let mut len = std::mem::size_of::<u64>();
        // SAFETY: `sysctlbyname` writes at most `len` bytes into `size`.
        let result = unsafe {
            libc::sysctlbyname(
                c"hw.memsize".as_ptr(),
                (&raw mut size).cast::<libc::c_void>(),
                &raw mut len,
                core::ptr::null_mut(),
                0,
            )
        };
        if result == 0 {
            return Some(size);
        }
        None
    }
    #[cfg(target_os = "linux")]
    {
        let meminfo = fs::read_to_string("/proc/meminfo").ok()?;
        parse_meminfo(&meminfo)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

fn parse_meminfo(meminfo: &str) -> Option<u64> {
    let line = meminfo.lines().find(|line| line.starts_with("MemTotal:"))?;
    let kib = line
        .strip_prefix("MemTotal:")?
        .split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()?;
    Some(kib.saturating_mul(1024))
}

/// The effective cgroup memory limit: the MINIMUM finite `memory.max` / `limit_in_bytes` over the process's own cgroup
/// and every ancestor up to the root. v2 reads `memory.max` ("max" = unlimited); v1 reads `memory.limit_in_bytes` (the
/// huge sentinel = unlimited). `None` when no hierarchy is detectable or no level carries a finite limit.
fn detect_cgroup_limit() -> Option<u64> {
    let self_cgroup = fs::read_to_string("/proc/self/cgroup").ok()?;
    // v2: `0:<path>`; v1: `<n>:<controllers>:<path>` with "memory" among them.
    let (root, path) = if let Some(path) = self_cgroup.lines().find_map(|l| l.strip_prefix("0::")) {
        (PathBuf::from("/sys/fs/cgroup"), path.trim().to_owned())
    } else {
        let path = self_cgroup
            .lines()
            .find(|line| {
                line.split(':')
                    .nth(1)
                    .is_some_and(|controllers| controllers.split(',').any(|c| c == "memory"))
            })?
            .split(':')
            .nth(2)?
            .trim()
            .to_owned();
        (PathBuf::from("/sys/fs/cgroup/memory"), path)
    };
    // Walk from the deepest segment up to the root and take the MINIMUM of every finite limit seen: cgroups v2's
    // effective limit is the tightest `memory.max` over the ancestry, so a descendant with a generous limit under a
    // tighter ancestor is bounded by the ancestor.
    let mut segments: Vec<&str> = path
        .trim_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    let mut min_limit: Option<u64> = None;
    loop {
        let mut dir = root.clone();
        for segment in &segments {
            dir.push(segment);
        }
        let v2_file = dir.join("memory.max");
        if let Ok(text) = fs::read_to_string(&v2_file) {
            let trimmed = text.trim();
            if trimmed != "max"
                && let Ok(bytes) = trimmed.parse::<u64>()
            {
                min_limit = Some(min_limit.map_or(bytes, |seen: u64| seen.min(bytes)));
            }
        }
        // The v1 huge sentinel is "unlimited"; any other value is finite.
        let v1_file = dir.join("memory.limit_in_bytes");
        if let Ok(text) = fs::read_to_string(&v1_file)
            && let Ok(bytes) = text.trim().parse::<u64>()
            && bytes < (1u64 << 60)
        {
            min_limit = Some(min_limit.map_or(bytes, |seen: u64| seen.min(bytes)));
        }
        if segments.pop().is_none() {
            break;
        }
    }
    min_limit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meminfo_parses_kibibytes() {
        let meminfo = "MemTotal:       16277716 kB\nMemFree:         8000000 kB\n";
        assert_eq!(parse_meminfo(meminfo), Some(16_277_716 * 1024));
        assert_eq!(parse_meminfo("no MemTotal here\n"), None);
    }

    #[test]
    fn effective_memory_takes_the_minimum() {
        let memory = DetectedMemory {
            host_bytes: Some(16 << 30),
            cgroup_bytes: Some(2 << 30),
        };
        assert_eq!(memory.effective_bytes(), Some(2 << 30));
        let host_only = DetectedMemory {
            host_bytes: Some(16 << 30),
            cgroup_bytes: None,
        };
        assert_eq!(host_only.effective_bytes(), Some(16 << 30));
        let nothing = DetectedMemory::default();
        assert_eq!(nothing.effective_bytes(), None);
    }

    #[test]
    fn the_default_ceiling_is_80_percent_of_the_effective_memory() {
        assert_eq!(DEFAULT_CEILING_PERCENT, 80);
        assert_eq!(SAMPLE_INTERVAL_MS, 4);
    }

    #[test]
    fn the_override_detection_reads_the_fake_hierarchy() {
        let dir = std::env::temp_dir().join(format!("jqf-rss-detect-{}", std::process::id()));
        let cgroup = dir.join("cgroup");
        fs::create_dir_all(&cgroup).expect("fake cgroup dir");
        fs::write(dir.join("meminfo"), "MemTotal: 16777216 kB\n").expect("meminfo");
        fs::write(cgroup.join("memory.max"), "2147483648\n").expect("memory.max");
        let memory = detect_from_override(&dir).expect("override detection");
        assert_eq!(memory.host_bytes, Some(16_777_216 * 1024));
        assert_eq!(memory.cgroup_bytes, Some(2 << 30));
        assert_eq!(memory.effective_bytes(), Some(2 << 30));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_override_detection_fails_loud_on_garbage() {
        let dir = std::env::temp_dir().join(format!("jqf-rss-detect-bad-{}", std::process::id()));
        fs::create_dir_all(dir.join("cgroup")).expect("fake cgroup dir");
        fs::write(dir.join("meminfo"), "not meminfo\n").expect("meminfo");
        assert!(detect_from_override(&dir).is_err(), "garbage must fail loud");
        let _ = fs::remove_dir_all(&dir);
    }
}
