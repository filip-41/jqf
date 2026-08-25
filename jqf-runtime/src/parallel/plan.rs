//! What a morsel-parallel request decides before it dispatches anything: how wide to run, and how large one morsel is.
//!
//! Nothing here spawns a thread or touches the ledger. It is the observable half of `--workers`: every decision this
//! module makes is printable under the CLI's diagnostics flag, which is what makes `auto` auditable rather than
//! magical.
//!
//! The vocabulary is shared by every morsel route. What is NOT shared is the pinned [`WidthPolicy`] each route measures
//! for itself: a break-even derived from one route's cost model is not evidence about another's, so each route owns its
//! own constants and their provenance.

use core::fmt;

/// The width a request asked for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerRequest {
    /// jqf decides both WHETHER this request benefits and HOW WIDE.
    Auto,
    /// An explicit width. One is the degenerate coordinator, byte-identical to serial; anything up to
    /// [`WORKER_HARD_CAP`] is accepted, oversubscription included, because oversubscription is a supported measurement
    /// mode.
    Explicit(usize),
}

/// The documented hard cap on `--workers N`.
///
/// The CLI contract requires a cap of at least 128. Oversubscription below it is supported, not an error: grants divide
/// accordingly and the ledger ceiling binds their sum.
pub const WORKER_HARD_CAP: usize = 256;

/// One route's observed width constants.
///
/// Every field is a receipt, not a preference. A route that copies another route's numbers is claiming evidence it does
/// not have, so the policy travels with the route that observed it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WidthPolicy {
    /// Below this many input bytes, `auto` stays on the serial path.
    pub(crate) break_even_bytes: u64,
    /// The narrowest width `auto` will choose above the break-even point.
    pub(crate) min_workers: usize,
    /// The smallest morsel this route will cut, and so `auto`'s unit of width.
    pub(crate) min_morsel_bytes: u64,
    /// The largest morsel this route will cut; it bounds one worker's envelope.
    pub(crate) max_morsel_bytes: u64,
    /// Morsels per worker the planner aims for, so an uneven tail cannot strand a core.
    pub(crate) morsels_per_worker: u64,
}

/// Why the planner chose the width it chose.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlanDecision {
    /// `--no-parallel` was given: today's serial execution, same code path.
    SwitchedOff,
    /// The program is outside the morsel static-path class.
    ProgramIneligible,
    /// The input format or dialect is outside this parallel lane.
    InputIneligible,
    /// `-e`/`--exit-status` reads the LAST OUTPUT VALUE, and a morsel worker publishes bytes and nothing else: the
    /// parallel relay never hands the facade a value to judge. The request takes the serial drive, which publishes the
    /// same bytes AND the exit-status fact.
    ExitStatus,
    /// `-n`/`-s` run the program exactly ONCE over the whole stream, so there are no per-record morsels to relay: the
    /// request takes the serial drive.
    SingleRunModel,
    /// The OUTPUT dialect carries state ACROSS items — the headered CSV profile derives one header row from the FIRST
    /// item's keys — so a morsel worker's bytes are not serial's bytes (every worker would write its own header). The
    /// request takes the serial drive.
    StatefulOutput,
    /// The mismatch dial is not lenient: warn aggregates its per-cell report in the worker's OWN request context (never
    /// the coordinator's), so a worker's report is lost and a strict raise inside a worker would need the
    /// yield-to-serial rerun to say anything at all. The request takes the serial drive, whose report and raises are
    /// the request's own.
    MismatchPolicy,
    /// The strictness dial is beyond `Error`: a morsel worker can neither surface a warning nor carry promotion, so
    /// `warn`/`strict` plan serial (the mismatch-policy decline's shape).
    Strictness,
    /// Colour is engaged : the colour pass renders per ITEM, and the parallel relay publishes a byte stream without
    /// item boundaries — a worker's bytes could never reach the pass as items, and a worker cannot colour (workers
    /// publish serial's bytes and nothing else). The serial drive publishes per item with its reports, exactly as the
    /// `-e` and mismatch declines do.
    Colour,
    /// The source-preserving EDIT lane: a morsel worker publishes bytes and nothing else, while an edit splices each
    /// record's OWN retained bytes against its authored spans and verifies the patch by re-decode — there is no
    /// per-record morsel to relay. The request takes the serial drive.
    Edit,
    /// A non-JSON-family output format (toml, yaml, cbor, csv, xml, html, jqft, jqfb, jqfjson) cannot travel the value
    /// lane's relay: the relay concatenates JSON-family byte streams, and a worker cannot encode a one-document-per-run
    /// format per value. The request takes the serial drive, whose encoder owns the format.
    OutputFormat,
    /// `--arg`/`--argjson`/`--args`/`-L`: the binding table and module search path are request-local HOST state a
    /// morsel worker is not built with, so the request plans serial even when its program itself is inside the morsel
    /// class. Distinct from [`Self::ProgramIneligible`] so a diagnostics reader can tell "the program cannot cross"
    /// from "the host bindings cannot".
    HostBindings,
    /// The split destination (`--split-exp`) opens one file per published ITEM, and a morsel worker publishes a byte
    /// stream without item boundaries — the worker's bytes could never reach the per-item sink. The serial drive
    /// publishes per item with its names, exactly as the exit-status decline does.
    SplitExp,
    /// `--unbuffered` flushes per ITEM; the relay flushes per morsel. A request that asked for per-item flush cannot
    /// cross the relay, so it takes the serial drive whose sink owns that cadence.
    Unbuffered,
    /// `auto` observed this input below the pinned break-even size.
    BelowBreakEven,
    /// The input holds fewer than two morsels, so a second worker has nothing to do.
    SingleMorsel,
    /// Boundary discovery could not prove two shard starts, so there is nothing safe to cut. Only a route that must
    /// DISCOVER its boundaries can report this.
    NoBoundary,
    /// The lane runs, this wide.
    Parallel,
}

impl PlanDecision {
    /// Returns whether this decision runs the coordinator at all.
    #[must_use]
    pub const fn is_parallel(self) -> bool {
        matches!(self, Self::Parallel)
    }

    const fn label(self) -> &'static str {
        match self {
            Self::SwitchedOff => "switched-off",
            Self::ProgramIneligible => "program-ineligible",
            Self::InputIneligible => "input-ineligible",
            Self::ExitStatus => "exit-status",
            Self::SingleRunModel => "single-run-model",
            Self::StatefulOutput => "stateful-output",
            Self::MismatchPolicy => "mismatch-policy",
            Self::Strictness => "strictness",
            Self::Colour => "colour",
            Self::Edit => "edit",
            Self::OutputFormat => "output-format",
            Self::HostBindings => "host-bindings",
            Self::SplitExp => "split-exp",
            Self::Unbuffered => "unbuffered",
            Self::BelowBreakEven => "below-break-even",
            Self::SingleMorsel => "single-morsel",
            Self::NoBoundary => "no-boundary",
            Self::Parallel => "parallel",
        }
    }
}

/// The complete, printable plan for one request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParallelPlan {
    request: WorkerRequest,
    decision: PlanDecision,
    workers: usize,
    morsel_bytes: u64,
    input_bytes: u64,
    core_ceiling: usize,
}

impl ParallelPlan {
    /// The serial plan, carrying why the lane did not engage.
    #[must_use]
    pub const fn serial(request: WorkerRequest, decision: PlanDecision, input_bytes: u64) -> Self {
        Self {
            request,
            decision,
            workers: 0,
            morsel_bytes: 0,
            input_bytes,
            core_ceiling: 0,
        }
    }

    /// The same request, demoted to serial for `decision`.
    ///
    /// A route whose boundaries are DISCOVERED learns only after partitioning that its input yields too few shards to
    /// be worth a worker; demotion keeps the width the caller asked for on the printable line.
    #[must_use]
    pub const fn demoted(self, decision: PlanDecision) -> Self {
        Self::serial(self.request, decision, self.input_bytes)
    }

    /// Returns the width the coordinator will run, zero for the serial path.
    #[must_use]
    pub const fn workers(self) -> usize {
        self.workers
    }

    /// Returns the target morsel size in bytes.
    #[must_use]
    pub const fn morsel_bytes(self) -> u64 {
        self.morsel_bytes
    }

    /// Returns why the planner chose this shape.
    #[must_use]
    pub const fn decision(self) -> PlanDecision {
        self.decision
    }

    /// Returns whether the coordinator runs.
    #[must_use]
    pub const fn is_parallel(self) -> bool {
        self.decision.is_parallel()
    }
}

impl fmt::Display for ParallelPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let requested = match self.request {
            WorkerRequest::Auto => "auto".to_owned(),
            WorkerRequest::Explicit(width) => width.to_string(),
        };
        write!(
            formatter,
            "plan: requested={requested} decision={} workers={} morsel_bytes={} input_bytes={} core_ceiling={}",
            self.decision.label(),
            self.workers,
            self.morsel_bytes,
            self.input_bytes,
            self.core_ceiling
        )
    }
}

/// Plans one morsel-parallel request under one route's observed `policy`.
///
/// `ineligible` is the caller's own gate — the program class and the input format/dialect — expressed as the decision
/// to record when it fails, so the diagnostics line says WHY a request fell through instead of only that it did.
#[must_use]
pub(crate) fn plan_request(
    request: WorkerRequest,
    input_bytes: u64,
    ineligible: Option<PlanDecision>,
    policy: WidthPolicy,
) -> ParallelPlan {
    if let Some(decision) = ineligible {
        return ParallelPlan::serial(request, decision, input_bytes);
    }
    let core_ceiling = auto_worker_ceiling();
    let workers = match request {
        // REAL auto: below the observed break-even the serial path wins, and above it the width is the number of
        // morsels the input can actually yield — a worker beyond that has nothing to draw.
        WorkerRequest::Auto => {
            if input_bytes < policy.break_even_bytes {
                return ParallelPlan::serial(request, PlanDecision::BelowBreakEven, input_bytes);
            }
            let drawable = usize::try_from(input_bytes.div_ceil(policy.min_morsel_bytes)).unwrap_or(usize::MAX);
            drawable.clamp(policy.min_workers.min(core_ceiling), core_ceiling)
        }
        WorkerRequest::Explicit(width) => width.clamp(1, WORKER_HARD_CAP),
    };
    let morsel_bytes = morsel_bytes_for(input_bytes, workers, policy);
    if input_bytes <= morsel_bytes {
        return ParallelPlan::serial(request, PlanDecision::SingleMorsel, input_bytes);
    }
    ParallelPlan {
        request,
        decision: PlanDecision::Parallel,
        workers,
        morsel_bytes,
        input_bytes,
        core_ceiling,
    }
}

fn morsel_bytes_for(input_bytes: u64, workers: usize, policy: WidthPolicy) -> u64 {
    let divisor = (workers as u64).saturating_mul(policy.morsels_per_worker).max(1);
    (input_bytes / divisor).clamp(policy.min_morsel_bytes, policy.max_morsel_bytes)
}

/// How the physical performance-core count was determined.
///
/// `--diagnostics` reports this beside the count itself, so a non-Apple run can tell a real topology read from a
/// portable stand-in instead of guessing which one happened.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PcoreSource {
    /// A platform implementation read real CPU topology (sysctl, sysfs, or `GetLogicalProcessorInformationEx`).
    Detected,
    /// No platform implementation could name the P-cores, so the portable fallback stands: half of
    /// `available_parallelism`, capped at 8. Keeps SMT hosts out of the collapse band that a logical-thread auto width
    /// would pick.
    Assumed,
}

/// The machine-topology facts one `--diagnostics` line reports.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CoreTopology {
    /// The physical performance-core count, at least 1.
    pub pcores: usize,
    /// The physical efficiency-core count, 0 when the platform reports no perflevel split (every non-hybrid machine,
    /// and every platform whose topology reader does not name E-cores).
    pub ecores: usize,
    /// How `pcores` was obtained.
    pub source: PcoreSource,
}

/// Returns the machine's core topology and how it was found — the facts `--diagnostics` reports together.
///
/// Efficiency cores are counted but HALVED into the auto ceiling ([`auto_worker_ceiling`]). Excluding them entirely
/// cost real wall clock: on a 4 GB NDJSON scan, `auto` at 6 workers took 4.47 s where 12 took 2.59 s and 18 took 2.32
/// s, byte-identical at every width. The counterweight is unchanged — an E-core buys less wall per unit of CPU than a
/// P-core, and on battery it is a power trade the user did not ask for — so `auto` takes half the E-cores rather than
/// all of them: most of the wall, well short of the 1.65x CPU that full oversubscription costs. Both extremes stay
/// reachable through `--workers N` up to [`WORKER_HARD_CAP`], which is where a user makes that choice deliberately.
///
/// The counts are computed ONCE per process and cached: the topology does not change mid-run, and the planning step
/// calls this on every request, so the sysfs reads would otherwise be paid once per request instead of once.
#[must_use]
pub fn core_topology() -> CoreTopology {
    static CACHE: std::sync::OnceLock<CoreTopology> = std::sync::OnceLock::new();
    *CACHE.get_or_init(compute_core_topology)
}

fn compute_core_topology() -> CoreTopology {
    match performance_cores() {
        Some(pcores) => CoreTopology {
            pcores: pcores.max(1),
            ecores: efficiency_cores().unwrap_or(0),
            source: PcoreSource::Detected,
        },
        None => CoreTopology {
            pcores: assumed_worker_ceiling(std::thread::available_parallelism().ok().map_or(1, Into::into)),
            ecores: 0,
            source: PcoreSource::Assumed,
        },
    }
}

/// The ceiling `auto` scales to: every performance core plus HALF the efficiency cores.
///
/// On a machine with no perflevel split `ecores` is 0 and this is exactly the physical performance-core count, so
/// nothing about non-hybrid behavior changes. `--diagnostics` reports both halves through [`core_topology`].
#[must_use]
pub(crate) fn auto_worker_ceiling() -> usize {
    let topology = core_topology();
    (topology.pcores + topology.ecores / 2).max(1)
}

/// Portable auto-width when topology detection fails: half the logical thread count, at least 1, at most 8. A 16-thread
/// SMT host therefore autos to 8, not 16 — out of the staging-collapse band that used to turn a default `jqf .` into a
/// full dispatch plus serial redrive.
pub(crate) const fn assumed_worker_ceiling(logical: usize) -> usize {
    let half = logical / 2;
    if half == 0 {
        1
    } else if half > 8 {
        8
    } else {
        half
    }
}

/// Apple: the P-core count is `hw.perflevel0.physicalcpu` — the physical performance-core count, so the ceiling is
/// per-CORE rather than per-THREAD.
#[cfg(target_vendor = "apple")]
fn performance_cores() -> Option<usize> {
    sysctl_physical_cpus(c"hw.perflevel0.physicalcpu")
}

/// Apple: the E-core count is `hw.perflevel1.physicalcpu`. Absent on a non-hybrid part, where the sysctl does not exist
/// and `auto` keeps the P-core ceiling.
#[cfg(target_vendor = "apple")]
fn efficiency_cores() -> Option<usize> {
    sysctl_physical_cpus(c"hw.perflevel1.physicalcpu")
}

/// Reads one `hw.perflevelN.physicalcpu` sysctl as a positive core count.
#[cfg(target_vendor = "apple")]
fn sysctl_physical_cpus(name: &core::ffi::CStr) -> Option<usize> {
    // `sysctlbyname` with a NUL-terminated name, a `u32` out-parameter, and its size. The three-argument-tail form
    // (newp/newlen) is a pure READ when both are null, which is the only call shape used here.
    unsafe extern "C" {
        fn sysctlbyname(
            name: *const core::ffi::c_char,
            oldp: *mut core::ffi::c_void,
            oldlenp: *mut usize,
            newp: *mut core::ffi::c_void,
            newlen: usize,
        ) -> core::ffi::c_int;
    }

    let mut value: u32 = 0;
    let mut length = core::mem::size_of::<u32>();
    // SAFETY: the name is a NUL-terminated literal, `oldp` points at a live `u32` whose exact size `oldlenp` reports,
    // and the write path is disabled with a null `newp` and a zero `newlen`.
    let status = unsafe {
        sysctlbyname(
            name.as_ptr(),
            (&raw mut value).cast(),
            &raw mut length,
            core::ptr::null_mut(),
            0,
        )
    };
    if status != 0 || length != core::mem::size_of::<u32>() || value == 0 {
        return None;
    }
    usize::try_from(value).ok()
}

/// Linux: P-cores from scheduler capacity when the part is hybrid; otherwise the physical-core count from topology (SMT
/// siblings count once).
///
/// `/sys/devices/system/cpu/cpuN/cpu_capacity` distinguishes P/E clusters. Where the values differ, a physical core is
/// a P-core when any of its threads carries the top capacity, deduplicated through `topology/core_cpus_list`. On a
/// non-hybrid part, or when `cpu_capacity` is absent, the same sibling list (or `thread_siblings_list`) names the
/// physical cores so auto width is not the logical-thread count. Only the CPUs the process may actually use
/// (`Cpus_allowed_list`) are considered.
#[cfg(target_os = "linux")]
fn performance_cores() -> Option<usize> {
    let allowed = allowed_cpus();
    let candidates = candidate_cpus(allowed.as_deref())?;
    if let Some(pcores) = hybrid_from_capacity(&candidates) {
        return Some(pcores);
    }
    physical_cores_from_topology(&candidates)
}

/// Hybrid P-core count from `cpu_capacity`. `None` when the file is missing or every allowed CPU reports the same
/// capacity (non-hybrid).
#[cfg(target_os = "linux")]
fn hybrid_from_capacity(candidates: &[u32]) -> Option<usize> {
    let mut capacities = Vec::with_capacity(candidates.len());
    for index in candidates {
        let capacity = std::fs::read_to_string(format!("/sys/devices/system/cpu/cpu{index}/cpu_capacity")).ok()?;
        capacities.push((*index, capacity.trim().parse::<u64>().ok()?));
    }
    hybrid_pcores(&capacities, core_members)
}

/// Physical cores via unique sibling groups. `None` when no topology file is readable, so the portable fallback stands
/// instead of counting threads.
#[cfg(target_os = "linux")]
fn physical_cores_from_topology(candidates: &[u32]) -> Option<usize> {
    physical_cores_from_groups(candidates, core_members)
}

/// Unique sibling-group count. Extracted so the unit tests can feed groups without touching sysfs.
#[cfg(any(target_os = "linux", test))]
fn physical_cores_from_groups(candidates: &[u32], core_of: impl Fn(u32) -> Option<Vec<u32>>) -> Option<usize> {
    if candidates.is_empty() {
        return None;
    }
    let mut cores = std::collections::HashSet::new();
    let mut any = false;
    for cpu in candidates {
        if let Some(members) = core_of(*cpu) {
            cores.insert(members);
            any = true;
        }
    }
    any.then_some(cores.len().max(1))
}

/// The set of CPU indexes worth reading topology for.
#[cfg(target_os = "linux")]
fn candidate_cpus(allowed: Option<&[u32]>) -> Option<Vec<u32>> {
    if let Some(list) = allowed.filter(|list| !list.is_empty()) {
        return Some(list.to_vec());
    }
    let online = std::fs::read_to_string("/sys/devices/system/cpu/online").ok()?;
    parse_cpu_list(&online)
}

/// The affinity mask's CPU list, read from the process's own status file.
#[cfg(target_os = "linux")]
fn allowed_cpus() -> Option<Vec<u32>> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("Cpus_allowed_list:") {
            return parse_cpu_list(rest);
        }
    }
    None
}

/// The logical CPUs sharing the physical core that contains `cpu`.
#[cfg(target_os = "linux")]
fn core_members(cpu: u32) -> Option<Vec<u32>> {
    let base = format!("/sys/devices/system/cpu/cpu{cpu}/topology");
    let list = std::fs::read_to_string(format!("{base}/core_cpus_list"))
        .or_else(|_| std::fs::read_to_string(format!("{base}/thread_siblings_list")))
        .ok()?;
    parse_cpu_list(&list)
}

/// Decides the P-core count from one CPU's capacity list.
///
/// `core_of` names the physical core a CPU index belongs to. Returns `None` when the capacities are uniform
/// (non-hybrid: the portable fallback stands) or when the list is empty. Unconditional so the unit tests run on every
/// host, though only the Linux arm calls it in production.
#[cfg(any(target_os = "linux", test))]
fn hybrid_pcores(capacities: &[(u32, u64)], core_of: impl Fn(u32) -> Option<Vec<u32>>) -> Option<usize> {
    let max = capacities.iter().map(|(_, capacity)| *capacity).max()?;
    let min = capacities.iter().map(|(_, capacity)| *capacity).min()?;
    if max == min {
        return None;
    }
    let mut cores = std::collections::HashSet::new();
    for (index, capacity) in capacities {
        if *capacity == max {
            cores.insert(core_of(*index).unwrap_or_else(|| vec![*index]));
        }
    }
    Some(cores.len())
}

/// Parses a Linux CPU-list ("0,2-3,5") into its individual indexes.
///
/// Unconditional so the unit tests run on every host, though only the Linux arm uses it in production.
#[cfg(any(target_os = "linux", test))]
fn parse_cpu_list(spec: &str) -> Option<Vec<u32>> {
    let mut cpus = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        let (start, end) = if let Some((start, end)) = part.split_once('-') {
            (start.trim().parse::<u32>().ok()?, end.trim().parse::<u32>().ok()?)
        } else {
            let cpu = part.parse::<u32>().ok()?;
            (cpu, cpu)
        };
        if start > end {
            return None;
        }
        cpus.extend(start..=end);
    }
    if cpus.is_empty() {
        return None;
    }
    cpus.sort_unstable();
    cpus.dedup();
    Some(cpus)
}

/// Every platform without a topology implementation: the portable fallback stands in, and the run reports
/// `PcoreSource::Assumed`.
#[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
const fn performance_cores() -> Option<usize> {
    None
}

/// Every platform whose topology reader does not name efficiency cores. Linux COULD derive them from the same
/// `cpu_capacity` values `hybrid_from_capacity` already reads; it deliberately does not yet, because no hybrid Linux
/// part has been measured here and a wider `auto` default is a measured change or it is nothing. `auto` therefore keeps
/// the P-core ceiling everywhere but Apple.
#[cfg(not(target_vendor = "apple"))]
const fn efficiency_cores() -> Option<usize> {
    None
}

/// One morsel: a half-open byte range of the input covering whole units.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Morsel {
    start: usize,
    end: usize,
}

impl Morsel {
    /// Creates one half-open morsel over `start..end`.
    #[must_use]
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    /// Returns the inclusive start offset in the request's input.
    #[must_use]
    pub const fn start(self) -> usize {
        self.start
    }

    /// Returns the exclusive end offset in the request's input.
    #[must_use]
    pub const fn end(self) -> usize {
        self.end
    }

    /// Returns the morsel's byte length.
    #[must_use]
    pub(crate) const fn len(self) -> usize {
        self.end - self.start
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PlanDecision, assumed_worker_ceiling, auto_worker_ceiling, core_topology, hybrid_pcores, parse_cpu_list,
        physical_cores_from_groups,
    };

    #[test]
    fn serial_plan_display_has_single_spaces() {
        let plan = super::ParallelPlan::serial(super::WorkerRequest::Auto, PlanDecision::BelowBreakEven, 1024);
        assert_eq!(
            plan.to_string(),
            "plan: requested=auto decision=below-break-even workers=0 morsel_bytes=0 input_bytes=1024 core_ceiling=0"
        );
    }

    #[test]
    fn core_topology_is_positive_and_is_the_worker_ceiling() {
        let topology = core_topology();
        assert!(topology.pcores >= 1);
        // `auto` takes every P-core plus half the E-cores, so the ceiling is the P-core count exactly when the machine
        // reports no efficiency cores.
        assert_eq!(auto_worker_ceiling(), topology.pcores + topology.ecores / 2);
        assert!(auto_worker_ceiling() >= topology.pcores);
    }

    #[test]
    fn cpu_list_parses_ranges_singles_and_duplicates() {
        assert_eq!(parse_cpu_list("0,2-3,5"), Some(vec![0, 2, 3, 5]));
        assert_eq!(parse_cpu_list("4"), Some(vec![4]));
        assert_eq!(parse_cpu_list("0-3,2-4"), Some(vec![0, 1, 2, 3, 4]));
        assert_eq!(parse_cpu_list(" 0-3 , 5 "), Some(vec![0, 1, 2, 3, 5]));
    }

    #[test]
    fn cpu_list_rejects_malformed_specs() {
        assert_eq!(parse_cpu_list(""), None);
        assert_eq!(parse_cpu_list("0,,"), None);
        assert_eq!(parse_cpu_list("3-1"), None);
        assert_eq!(parse_cpu_list("x"), None);
        assert_eq!(parse_cpu_list("0-3,x"), None);
    }

    #[test]
    fn uniform_capacities_fall_back_to_the_portable_answer() {
        let capacities = vec![(0, 1024), (1, 1024), (2, 1024)];
        assert_eq!(hybrid_pcores(&capacities, |cpu| Some(vec![cpu])), None);
        assert_eq!(hybrid_pcores(&[], |cpu| Some(vec![cpu])), None);
    }

    #[test]
    fn hybrid_capacities_count_top_capacity_physical_cores() {
        // Two logical threads on one P-core (an SMT pair) plus four E-cores.
        let capacities = vec![(0, 1024), (1, 1024), (2, 574), (3, 574), (4, 574), (5, 574)];
        let core_of = |cpu| {
            if cpu == 0 || cpu == 1 {
                Some(vec![0, 1])
            } else {
                Some(vec![cpu])
            }
        };
        assert_eq!(hybrid_pcores(&capacities, core_of), Some(1));
    }

    #[test]
    fn hybrid_capacities_count_cores_not_threads() {
        let capacities = vec![(0, 1024), (1, 1024), (2, 1024), (3, 1024), (4, 512), (5, 512)];
        let core_of = |cpu| match cpu {
            0 | 1 => Some(vec![0, 1]),
            2 | 3 => Some(vec![2, 3]),
            other => Some(vec![other]),
        };
        assert_eq!(hybrid_pcores(&capacities, core_of), Some(2));
    }

    #[test]
    fn an_unreadable_core_topology_degrades_to_one_thread_per_top_core() {
        let capacities = vec![(0, 1024), (1, 1024), (2, 512)];
        let core_of = |_cpu| None;
        assert_eq!(hybrid_pcores(&capacities, core_of), Some(2));
    }

    #[test]
    fn assumed_auto_width_is_half_logical_capped_at_eight() {
        assert_eq!(assumed_worker_ceiling(1), 1);
        assert_eq!(assumed_worker_ceiling(2), 1);
        assert_eq!(assumed_worker_ceiling(4), 2);
        assert_eq!(assumed_worker_ceiling(16), 8);
        assert_eq!(assumed_worker_ceiling(18), 8);
        assert_eq!(assumed_worker_ceiling(32), 8);
    }

    #[test]
    fn topology_counts_physical_cores_not_smt_threads() {
        let candidates = [0, 1, 2, 3];
        let core_of = |cpu| match cpu {
            0 | 1 => Some(vec![0, 1]),
            2 | 3 => Some(vec![2, 3]),
            _ => None,
        };
        assert_eq!(physical_cores_from_groups(&candidates, core_of), Some(2));
        assert_eq!(physical_cores_from_groups(&[], core_of), None);
        assert_eq!(
            physical_cores_from_groups(&candidates, |_| None),
            None,
            "unreadable topology declines so the portable fallback stands"
        );
    }

    #[test]
    fn relay_decline_names_are_stable() {
        // The printable names of the relay-incompatible decisions are pinned surface (the plan line a differential
        // harness reads).
        assert_eq!(PlanDecision::SplitExp.label(), "split-exp");
        assert_eq!(PlanDecision::Colour.label(), "colour");
        assert_eq!(PlanDecision::Unbuffered.label(), "unbuffered");
    }
}
