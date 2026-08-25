//! Shared release-mode harness for jqf benchmark workers.
//!
//! A worker supplies deterministic cases, each with one untimed correctness
//! preflight and a mutable one-shot operation. This crate calibrates, warms,
//! measures, and renders those operations without adding dependencies to the
//! benchmark binaries.

#[cfg(feature = "allocation-stats")]
pub mod allocation;

#[cfg(all(test, feature = "allocation-stats"))]
#[global_allocator]
static TEST_ALLOCATOR: allocation::MeasuringAllocator = allocation::MeasuringAllocator;

use std::{
    env,
    fmt::{self, Write},
    hint::black_box,
    time::{Duration, Instant},
};

const DEFAULT_SAMPLES: usize = 30;
const DEFAULT_SAMPLE_MILLIS: u64 = 20;
const DEFAULT_WARMUP_MILLIS: u64 = 250;
const QUICK_SAMPLES: usize = 8;
const QUICK_SAMPLE_MILLIS: u64 = 5;
const QUICK_WARMUP_MILLIS: u64 = 25;
const MAX_CALIBRATION_ITERATIONS: u64 = 1 << 24;
const MEBIBYTE: f64 = 1_048_576.0;

/// The shared measured-region resource envelopes for the benchmark workers.
///
/// One place, so the workers cannot drift apart again: each codec bench
/// carried a private copy of these five numbers and they diverged (an
/// unbounded fixture ledger here, a 512 MiB measured ceiling there). The
/// tuples are the arguments of `ResourceLimits::new` in order — input bytes,
/// output bytes, live memory, spill disk, nesting depth.
pub mod limits {
    /// Untimed setup work only (fixture building and materialization): no
    /// ceiling on any dimension.
    pub const FIXTURE_BUILD: (u64, u64, u64, u64, u32) = (u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);

    /// One measured region (the timed decode/encode drive): logical bytes
    /// unbounded, a bounded live-memory ceiling so a runaway case fails
    /// loudly instead of paging, and a fixed nesting depth.
    pub const MEASURED_REGION: (u64, u64, u64, u64, u32) = (u64::MAX, u64::MAX, 128 << 20, u64::MAX, 256);
}

/// Immutable description of one benchmark operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CaseMetadata {
    /// Stable machine-readable case name, conventionally `group/name`.
    pub name: &'static str,
    /// Successful logical operations completed by one [`BenchmarkCase::run`] call.
    pub operations_per_invocation: u64,
    /// Input or output bytes represented by one [`BenchmarkCase::run`] call.
    ///
    /// This may be zero for a workload without a meaningful byte rate.
    pub bytes_per_invocation: u64,
}

impl CaseMetadata {
    /// Create metadata for a benchmark case.
    ///
    /// # Panics
    ///
    /// Panics when `operations_per_invocation` is zero because normalized timing
    /// statistics require a positive operation count.
    #[must_use]
    pub const fn new(name: &'static str, operations_per_invocation: u64, bytes_per_invocation: u64) -> Self {
        assert!(operations_per_invocation > 0);
        Self {
            name,
            operations_per_invocation,
            bytes_per_invocation,
        }
    }
}

/// Receipt produced by an untimed correctness preflight.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreflightReceipt {
    /// Deterministic checksum or other compact correctness witness.
    pub checksum: u64,
    /// Human-readable exact receipt details retained in machine-readable output.
    pub detail: String,
}

impl PreflightReceipt {
    /// Create a preflight receipt.
    #[must_use]
    pub fn new(checksum: u64, detail: impl Into<String>) -> Self {
        Self {
            checksum,
            detail: detail.into(),
        }
    }
}

/// A deterministic benchmark case implemented by a worker binary.
pub trait BenchmarkCase {
    /// Return stable metadata for this case.
    fn metadata(&self) -> CaseMetadata;

    /// Run one untimed correctness check and return its retained receipt.
    ///
    /// The error text is rendered with the case name and aborts the suite before
    /// any timing or allocation measurement for that case.
    ///
    /// # Errors
    ///
    /// Returns worker-defined text when the case is not correct to measure.
    fn preflight(&mut self) -> Result<PreflightReceipt, String>;

    /// Run one complete measured invocation and return a checksum for `black_box`.
    fn run(&mut self) -> u64;
}

impl<T: BenchmarkCase + ?Sized> BenchmarkCase for Box<T> {
    fn metadata(&self) -> CaseMetadata {
        (**self).metadata()
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        (**self).preflight()
    }

    fn run(&mut self) -> u64 {
        (**self).run()
    }
}

impl<T: BenchmarkCase + ?Sized> BenchmarkCase for &mut T {
    fn metadata(&self) -> CaseMetadata {
        (**self).metadata()
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        (**self).preflight()
    }

    fn run(&mut self) -> u64 {
        (**self).run()
    }
}

/// Common command-line and timing settings for all workers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BenchmarkConfig {
    /// Timed samples collected per selected case.
    pub samples: usize,
    /// Target duration used by deterministic calibration for one sample batch.
    pub sample_time: Duration,
    /// Minimum warm-up duration after calibration for each case.
    pub warmup_time: Duration,
    /// Optional substring filter over [`CaseMetadata::name`].
    pub filter: Option<String>,
    /// Render machine-readable JSON instead of the human table.
    pub json: bool,
    /// Run one allocation measurement instead of the timing harness.
    pub allocations: bool,
}

impl Default for BenchmarkConfig {
    fn default() -> Self {
        Self {
            samples: DEFAULT_SAMPLES,
            sample_time: Duration::from_millis(DEFAULT_SAMPLE_MILLIS),
            warmup_time: Duration::from_millis(DEFAULT_WARMUP_MILLIS),
            filter: None,
            json: false,
            allocations: false,
        }
    }
}

impl BenchmarkConfig {
    /// Parse the common benchmark command-line options from the process environment.
    ///
    /// # Errors
    ///
    /// Returns [`BenchmarkError::InvalidArgument`] for malformed options.
    pub fn from_env() -> Result<CliOutcome, BenchmarkError> {
        Self::parse(env::args().skip(1))
    }

    /// Parse the common benchmark command-line options.
    ///
    /// # Errors
    ///
    /// Returns [`BenchmarkError::InvalidArgument`] for malformed options.
    pub fn parse(arguments: impl IntoIterator<Item = String>) -> Result<CliOutcome, BenchmarkError> {
        let mut config = Self::default();
        let mut arguments = arguments.into_iter();
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--help" | "-h" => return Ok(CliOutcome::Help),
                "--json" => config.json = true,
                "--allocations" => config.allocations = true,
                "--quick" => config.apply_quick(),
                "--samples" => {
                    config.samples = parse_positive_usize(&argument, arguments.next())?;
                }
                "--sample-ms" => {
                    config.sample_time = Duration::from_millis(parse_positive(&argument, arguments.next())?);
                }
                "--warmup-ms" => {
                    config.warmup_time = Duration::from_millis(parse_positive(&argument, arguments.next())?);
                }
                "--filter" => {
                    config.filter = Some(
                        arguments
                            .next()
                            .ok_or_else(|| BenchmarkError::InvalidArgument(format!("{argument} requires a value")))?,
                    );
                }
                _ => {
                    return Err(BenchmarkError::InvalidArgument(format!("unknown argument: {argument}")));
                }
            }
        }
        Ok(CliOutcome::Config(config))
    }

    /// Apply the short deterministic development profile.
    pub fn apply_quick(&mut self) {
        self.samples = QUICK_SAMPLES;
        self.sample_time = Duration::from_millis(QUICK_SAMPLE_MILLIS);
        self.warmup_time = Duration::from_millis(QUICK_WARMUP_MILLIS);
    }

    fn validate(&self) -> Result<(), BenchmarkError> {
        if self.samples == 0 {
            return Err(BenchmarkError::InvalidConfig("samples must be positive"));
        }
        if self.sample_time.is_zero() {
            return Err(BenchmarkError::InvalidConfig("sample time must be positive"));
        }
        Ok(())
    }
}

/// Result of parsing common worker options.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CliOutcome {
    /// Print [`usage`] and exit successfully.
    Help,
    /// Run the worker with this configuration.
    Config(BenchmarkConfig),
}

/// Errors returned by the shared harness.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BenchmarkError {
    /// A command-line option was malformed or unknown.
    InvalidArgument(String),
    /// Programmatic timing configuration was invalid.
    InvalidConfig(&'static str),
    /// No case name matched a requested filter.
    NoMatchingCases,
    /// A case's correctness preflight failed.
    Preflight {
        /// Case name.
        case: &'static str,
        /// Worker-provided failure text.
        message: String,
    },
    /// The worker was built with debug assertions enabled.
    ReleaseBuildRequired,
    /// The allocation worker did not install the shared measuring allocator.
    AllocationInstrumentationUnavailable,
    /// The timing entrypoint received an allocation-mode configuration.
    TimingSuiteRequiresTimingMode,
    /// The allocation entrypoint received a timing-mode configuration.
    AllocationSuiteRequiresAllocationMode,
    /// The allocation accounting feature is present in a timing worker build.
    TimingInstrumentationPresent,
}

impl fmt::Display for BenchmarkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidArgument(message) => formatter.write_str(message),
            Self::InvalidConfig(message) => formatter.write_str(message),
            Self::NoMatchingCases => formatter.write_str("no benchmark case matched the requested filter"),
            Self::Preflight { case, message } => {
                write!(formatter, "{case} preflight failed: {message}")
            }
            Self::ReleaseBuildRequired => {
                formatter.write_str("jqf benchmark workers require a release build; run cargo with --release")
            }
            Self::AllocationInstrumentationUnavailable => formatter.write_str(
                "allocation measurement requires jqf_bench_core::allocation::MeasuringAllocator \
                 as the worker's global allocator",
            ),
            Self::TimingSuiteRequiresTimingMode => {
                formatter.write_str("run_suite measures timing; call run_allocation_suite for --allocations")
            }
            Self::AllocationSuiteRequiresAllocationMode => {
                formatter.write_str("run_allocation_suite requires the --allocations configuration")
            }
            Self::TimingInstrumentationPresent => {
                formatter.write_str("timing requires a worker built without the allocation-stats feature")
            }
        }
    }
}

impl std::error::Error for BenchmarkError {}

/// One raw timed sample before aggregate statistics are calculated.
#[derive(Clone, Debug, PartialEq)]
pub struct RawSample {
    /// Number of case invocations in this sample batch.
    pub iterations: u64,
    /// Total wall-clock duration of the batch.
    pub elapsed: Duration,
    /// Total wall-clock nanoseconds per complete case invocation.
    pub nanoseconds_per_invocation: f64,
    /// Normalized nanoseconds per declared logical operation.
    pub nanoseconds_per_operation: f64,
}

/// Robust aggregate statistics calculated from raw samples.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TimingStats {
    /// Nearest-rank median nanoseconds per operation.
    pub median_nanoseconds_per_operation: f64,
    /// Nearest-rank 95th percentile nanoseconds per operation.
    pub p95_nanoseconds_per_operation: f64,
    /// Median absolute deviation from the median, in nanoseconds per operation.
    pub mad_nanoseconds_per_operation: f64,
    /// Throughput from the median timing, when the case declares bytes.
    pub median_mebibytes_per_second: Option<f64>,
}

/// Timing result for one successful benchmark case.
#[derive(Clone, Debug, PartialEq)]
pub struct TimingMeasurement {
    /// Case metadata.
    pub metadata: CaseMetadata,
    /// Correctness evidence collected before timing.
    pub preflight: PreflightReceipt,
    /// Calibrated invocations in each sample batch.
    pub iterations_per_sample: u64,
    /// Sorted raw samples used for the aggregate statistics.
    pub samples: Vec<RawSample>,
    /// Aggregate statistics derived from [`Self::samples`].
    pub statistics: TimingStats,
}

/// One allocation result gathered from a complete untimed case invocation.
#[cfg(feature = "allocation-stats")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AllocationMeasurement {
    /// Case metadata.
    pub metadata: CaseMetadata,
    /// Correctness evidence collected before allocation accounting.
    pub preflight: PreflightReceipt,
    /// Checksum returned by the measured invocation.
    pub operation_checksum: u64,
    /// Heap accounting gathered by the worker-installed measuring allocator.
    pub statistics: allocation::AllocationStats,
}

/// Measure every selected case after requiring a release build.
///
/// Every selected case executes one preflight before calibration. The timing
/// route invokes [`BenchmarkCase::run`] only after that receipt succeeds.
///
/// # Errors
///
/// Returns an error for debug builds, invalid configuration, an empty selection,
/// or a worker preflight failure.
pub fn run_suite<C: BenchmarkCase>(
    cases: &mut [C],
    config: &BenchmarkConfig,
) -> Result<Vec<TimingMeasurement>, BenchmarkError> {
    ensure_timing_mode(config)?;
    require_release_build()?;
    config.validate()?;
    let preflights = preflight_selected_cases(cases, config.filter.as_deref())?;
    let mut measurements = Vec::new();
    for (case, preflight) in cases.iter_mut().zip(preflights) {
        if let Some(preflight) = preflight {
            let metadata = case.metadata();
            measurements.push(measure_case_with_preflight(case, config, metadata, preflight));
        }
    }
    Ok(measurements)
}

fn ensure_timing_mode(config: &BenchmarkConfig) -> Result<(), BenchmarkError> {
    if config.allocations {
        return Err(BenchmarkError::TimingSuiteRequiresTimingMode);
    }
    if cfg!(feature = "allocation-stats") {
        return Err(BenchmarkError::TimingInstrumentationPresent);
    }
    Ok(())
}

/// Measure one complete allocation-accounted invocation of every selected case.
///
/// This route is feature-gated and intentionally does not calibrate, warm, or
/// collect timing samples. The worker must install
/// [`allocation::MeasuringAllocator`] as its global allocator.
///
/// # Errors
///
/// Returns an error for debug builds, invalid configuration, an empty selection,
/// or a worker preflight failure.
#[cfg(feature = "allocation-stats")]
pub fn run_allocation_suite<C: BenchmarkCase>(
    cases: &mut [C],
    config: &BenchmarkConfig,
) -> Result<Vec<AllocationMeasurement>, BenchmarkError> {
    if !config.allocations {
        return Err(BenchmarkError::AllocationSuiteRequiresAllocationMode);
    }
    require_release_build()?;
    config.validate()?;
    allocation::verify_global_allocator().map_err(|_| BenchmarkError::AllocationInstrumentationUnavailable)?;
    let preflights = preflight_selected_cases(cases, config.filter.as_deref())?;
    let mut measurements = Vec::new();
    for (case, preflight) in cases.iter_mut().zip(preflights) {
        if let Some(preflight) = preflight {
            let metadata = case.metadata();
            let (operation_checksum, statistics) = allocation::measure(|| case.run());
            black_box(operation_checksum);
            measurements.push(AllocationMeasurement {
                metadata,
                preflight,
                operation_checksum,
                statistics,
            });
        }
    }
    Ok(measurements)
}

/// Return whether metadata is selected by an optional substring filter.
#[must_use]
pub fn matches_filter(metadata: CaseMetadata, filter: Option<&str>) -> bool {
    filter.is_none_or(|filter| metadata.name.contains(filter))
}

/// Reject debug benchmark workers because their measurements are misleading.
///
/// # Errors
///
/// Returns [`BenchmarkError::ReleaseBuildRequired`] when debug assertions are enabled.
pub fn require_release_build() -> Result<(), BenchmarkError> {
    if cfg!(debug_assertions) {
        Err(BenchmarkError::ReleaseBuildRequired)
    } else {
        Ok(())
    }
}

/// Render the common worker help text.
#[must_use]
pub const fn usage() -> &'static str {
    "Usage: jqf-*-bench [OPTIONS]\n\
     \n\
     Options:\n\
     \x20 --quick             Use a short development run\n\
     \x20 --samples N         Timed samples per case (default: 30)\n\
     \x20 --sample-ms N       Target duration per sample (default: 20)\n\
     \x20 --warmup-ms N       Warm-up duration per case (default: 250)\n\
     \x20 --filter TEXT       Run cases whose names contain TEXT\n\
     \x20 --allocations       Report one allocation measurement per case\n\
     \x20 --json              Emit machine-readable JSON\n\
     \x20 -h, --help          Print this help"
}

/// Render timing results as a compact human-readable table.
#[must_use]
pub fn render_timing_human(measurements: &[TimingMeasurement], config: &BenchmarkConfig) -> String {
    let mut output = String::new();
    writeln!(
        output,
        "jqf benchmark: {} samples, {:.0} ms target/sample, {:.0} ms warm-up",
        config.samples,
        config.sample_time.as_secs_f64() * 1_000.0,
        config.warmup_time.as_secs_f64() * 1_000.0
    )
    .expect("writing to String cannot fail");
    writeln!(
        output,
        "{:<35} {:>16} {:>12} {:>12} {:>12} {:>12}",
        "case", "bytes/invocation", "median", "p95", "MAD", "median MiB/s"
    )
    .expect("writing to String cannot fail");
    for measurement in measurements {
        let throughput = measurement
            .statistics
            .median_mebibytes_per_second
            .map_or_else(|| "-".to_owned(), |value| format!("{value:.2}"));
        writeln!(
            output,
            "{:<35} {:>16} {:>9.1} ns {:>9.1} ns {:>9.1} ns {:>12}",
            measurement.metadata.name,
            measurement.metadata.bytes_per_invocation,
            measurement.statistics.median_nanoseconds_per_operation,
            measurement.statistics.p95_nanoseconds_per_operation,
            measurement.statistics.mad_nanoseconds_per_operation,
            throughput
        )
        .expect("writing to String cannot fail");
    }
    output
}

/// Render timing results as dependency-free machine-readable JSON.
///
/// # Panics
///
/// Panics only if formatting into an owned [`String`] unexpectedly fails.
#[must_use]
pub fn render_timing_json(measurements: &[TimingMeasurement]) -> String {
    let mut output = String::from("{\"schema\":1,\"mode\":\"timing\",\"measurements\":[");
    for (index, measurement) in measurements.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        output.push_str("{\"name\":");
        push_json_string(&mut output, measurement.metadata.name);
        write!(
            output,
            ",\"operations_per_invocation\":{},\"bytes_per_invocation\":{},\"preflight\":{{\"checksum\":{},\"detail\":",
            measurement.metadata.operations_per_invocation,
            measurement.metadata.bytes_per_invocation,
            measurement.preflight.checksum,
        )
        .expect("writing to String cannot fail");
        push_json_string(&mut output, &measurement.preflight.detail);
        write!(
            output,
            "}},\"iterations_per_sample\":{},\"samples\":[",
            measurement.iterations_per_sample,
        )
        .expect("writing to String cannot fail");
        for (sample_index, sample) in measurement.samples.iter().enumerate() {
            if sample_index > 0 {
                output.push(',');
            }
            write!(
                output,
                "{{\"iterations\":{},\"elapsed_ns\":{},\"nanoseconds_per_invocation\":{:.3},\"nanoseconds_per_operation\":{:.3}}}",
                sample.iterations,
                sample.elapsed.as_nanos(),
                sample.nanoseconds_per_invocation,
                sample.nanoseconds_per_operation,
            )
            .expect("writing to String cannot fail");
        }
        write!(
            output,
            "],\"statistics\":{{\"median_nanoseconds_per_operation\":{:.3},\"p95_nanoseconds_per_operation\":{:.3},\"mad_nanoseconds_per_operation\":{:.3}",
            measurement.statistics.median_nanoseconds_per_operation,
            measurement.statistics.p95_nanoseconds_per_operation,
            measurement.statistics.mad_nanoseconds_per_operation,
        )
        .expect("writing to String cannot fail");
        match measurement.statistics.median_mebibytes_per_second {
            Some(value) => write!(output, ",\"median_mebibytes_per_second\":{value:.3}"),
            None => output.write_str(",\"median_mebibytes_per_second\":null"),
        }
        .expect("writing to String cannot fail");
        output.push_str("}}");
    }
    output.push_str("]}");
    output
}

/// Render allocation results as a compact human-readable table.
#[cfg(feature = "allocation-stats")]
#[must_use]
pub fn render_allocation_human(measurements: &[AllocationMeasurement]) -> String {
    let mut output = String::from("jqf allocation measurement: one complete operation per case\n");
    writeln!(
        output,
        "{:<35} {:>9} {:>9} {:>12} {:>12} {:>12}",
        "case", "allocs", "reallocs", "requested", "peak live", "retained"
    )
    .expect("writing to String cannot fail");
    for measurement in measurements {
        writeln!(
            output,
            "{:<35} {:>9} {:>9} {:>12} {:>12} {:>12}",
            measurement.metadata.name,
            measurement.statistics.allocation_calls,
            measurement.statistics.reallocation_calls,
            measurement.statistics.requested_bytes,
            measurement.statistics.peak_live_bytes,
            measurement.statistics.retained_bytes,
        )
        .expect("writing to String cannot fail");
    }
    output
}

/// Render allocation results as dependency-free machine-readable JSON.
///
/// # Panics
///
/// Panics only if formatting into an owned [`String`] unexpectedly fails.
#[cfg(feature = "allocation-stats")]
#[must_use]
pub fn render_allocation_json(measurements: &[AllocationMeasurement]) -> String {
    let mut output = String::from("{\"schema\":1,\"mode\":\"allocations\",\"measurements\":[");
    for (index, measurement) in measurements.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        output.push_str("{\"name\":");
        push_json_string(&mut output, measurement.metadata.name);
        write!(
            output,
            ",\"operations_per_invocation\":{},\"bytes_per_invocation\":{},\"preflight\":{{\"checksum\":{},\"detail\":",
            measurement.metadata.operations_per_invocation,
            measurement.metadata.bytes_per_invocation,
            measurement.preflight.checksum,
        )
        .expect("writing to String cannot fail");
        push_json_string(&mut output, &measurement.preflight.detail);
        write!(
            output,
            "}},\"operation_checksum\":{},\"allocation_calls\":{},\"reallocation_calls\":{},\"requested_bytes\":{},\"peak_live_bytes\":{},\"retained_bytes\":{}}}",
            measurement.operation_checksum,
            measurement.statistics.allocation_calls,
            measurement.statistics.reallocation_calls,
            measurement.statistics.requested_bytes,
            measurement.statistics.peak_live_bytes,
            measurement.statistics.retained_bytes,
        )
        .expect("writing to String cannot fail");
    }
    output.push_str("]}");
    output
}

fn preflight_selected_cases<C: BenchmarkCase>(
    cases: &mut [C],
    filter: Option<&str>,
) -> Result<Vec<Option<PreflightReceipt>>, BenchmarkError> {
    let mut preflights = Vec::with_capacity(cases.len());
    let mut selected = false;
    for case in cases {
        let metadata = case.metadata();
        if matches_filter(metadata, filter) {
            selected = true;
            preflights.push(Some(case.preflight().map_err(|message| BenchmarkError::Preflight {
                case: metadata.name,
                message,
            })?));
        } else {
            preflights.push(None);
        }
    }
    if selected {
        Ok(preflights)
    } else {
        Err(BenchmarkError::NoMatchingCases)
    }
}

fn measure_case_with_preflight<C: BenchmarkCase + ?Sized>(
    case: &mut C,
    config: &BenchmarkConfig,
    metadata: CaseMetadata,
    preflight: PreflightReceipt,
) -> TimingMeasurement {
    let iterations_per_sample = calibrate(case, config.sample_time);
    warm_up(case, iterations_per_sample, config.warmup_time);

    let mut samples = Vec::with_capacity(config.samples);
    for _ in 0..config.samples {
        let started = Instant::now();
        run_batch(case, iterations_per_sample);
        let elapsed = started.elapsed();
        samples.push(RawSample {
            iterations: iterations_per_sample,
            elapsed,
            nanoseconds_per_invocation: normalized_nanoseconds_per_invocation(elapsed, iterations_per_sample),
            nanoseconds_per_operation: normalized_nanoseconds(elapsed, iterations_per_sample, metadata),
        });
    }
    samples.sort_by(|left, right| {
        left.nanoseconds_per_operation
            .total_cmp(&right.nanoseconds_per_operation)
    });
    let statistics = statistics(&samples, metadata);
    TimingMeasurement {
        metadata,
        preflight,
        iterations_per_sample,
        samples,
        statistics,
    }
}

fn calibrate<C: BenchmarkCase + ?Sized>(case: &mut C, target: Duration) -> u64 {
    let mut iterations = 1_u64;
    loop {
        let started = Instant::now();
        run_batch(case, iterations);
        if started.elapsed() >= target || iterations >= MAX_CALIBRATION_ITERATIONS {
            return iterations;
        }
        iterations = iterations.saturating_mul(2).min(MAX_CALIBRATION_ITERATIONS);
    }
}

fn warm_up<C: BenchmarkCase + ?Sized>(case: &mut C, iterations: u64, duration: Duration) {
    let started = Instant::now();
    while started.elapsed() < duration {
        run_batch(case, iterations);
    }
}

fn run_batch<C: BenchmarkCase + ?Sized>(case: &mut C, iterations: u64) {
    for _ in 0..iterations {
        black_box(case.run());
    }
}

fn normalized_nanoseconds(elapsed: Duration, iterations: u64, metadata: CaseMetadata) -> f64 {
    normalized_nanoseconds_per_invocation(elapsed, iterations) / work_units_as_f64(metadata.operations_per_invocation)
}

fn normalized_nanoseconds_per_invocation(elapsed: Duration, iterations: u64) -> f64 {
    elapsed.as_secs_f64() * 1_000_000_000.0 / work_units_as_f64(iterations)
}

fn work_units_as_f64(value: u64) -> f64 {
    const LOWER_MASK: u64 = 0xFFFF_FFFF;
    const U32_RANGE: f64 = 4_294_967_296.0;
    let upper = u32::try_from(value >> 32).expect("the upper u64 half fits u32");
    let lower = u32::try_from(value & LOWER_MASK).expect("the lower u64 half fits u32");
    f64::from(upper) * U32_RANGE + f64::from(lower)
}

fn statistics(samples: &[RawSample], metadata: CaseMetadata) -> TimingStats {
    let values: Vec<_> = samples.iter().map(|sample| sample.nanoseconds_per_operation).collect();
    let median = percentile(&values, 50, 100);
    let p95 = percentile(&values, 95, 100);
    let mut deviations: Vec<_> = values.into_iter().map(|value| (value - median).abs()).collect();
    deviations.sort_by(f64::total_cmp);
    let mad = percentile(&deviations, 50, 100);
    let median_mebibytes_per_second = (metadata.bytes_per_invocation > 0).then(|| {
        let invocation_values: Vec<_> = samples.iter().map(|sample| sample.nanoseconds_per_invocation).collect();
        let median_invocation = percentile(&invocation_values, 50, 100);
        work_units_as_f64(metadata.bytes_per_invocation) / MEBIBYTE / (median_invocation / 1_000_000_000.0)
    });
    TimingStats {
        median_nanoseconds_per_operation: median,
        p95_nanoseconds_per_operation: p95,
        mad_nanoseconds_per_operation: mad,
        median_mebibytes_per_second,
    }
}

fn percentile(sorted_values: &[f64], numerator: usize, denominator: usize) -> f64 {
    assert!(!sorted_values.is_empty());
    assert!(numerator > 0 && numerator <= denominator);
    let rank = sorted_values
        .len()
        .checked_mul(numerator)
        .expect("sample rank cannot overflow")
        .div_ceil(denominator);
    sorted_values[rank.saturating_sub(1).min(sorted_values.len() - 1)]
}

fn parse_positive(flag: &str, value: Option<String>) -> Result<u64, BenchmarkError> {
    let value = value.ok_or_else(|| BenchmarkError::InvalidArgument(format!("{flag} requires a value")))?;
    let parsed = value
        .parse::<u64>()
        .map_err(|_| BenchmarkError::InvalidArgument(format!("{flag} requires a positive integer, got {value:?}")))?;
    if parsed == 0 {
        Err(BenchmarkError::InvalidArgument(format!(
            "{flag} requires a positive integer"
        )))
    } else {
        Ok(parsed)
    }
}

fn parse_positive_usize(flag: &str, value: Option<String>) -> Result<usize, BenchmarkError> {
    let parsed = parse_positive(flag, value)?;
    usize::try_from(parsed)
        .map_err(|_| BenchmarkError::InvalidArgument(format!("{flag} requires an integer that fits this platform")))
}

fn push_json_string(output: &mut String, value: &str) {
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{08}' => output.push_str("\\b"),
            '\u{0C}' => output.push_str("\\f"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character.is_control() => {
                write!(output, "\\u{:04x}", character as u32).expect("writing to String cannot fail");
            }
            character => output.push(character),
        }
    }
    output.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    struct CountingCase {
        metadata: CaseMetadata,
        preflights: usize,
        runs: usize,
    }

    impl CountingCase {
        fn new(name: &'static str) -> Self {
            Self {
                metadata: CaseMetadata::new(name, 2, 128),
                preflights: 0,
                runs: 0,
            }
        }
    }

    impl BenchmarkCase for CountingCase {
        fn metadata(&self) -> CaseMetadata {
            self.metadata
        }

        fn preflight(&mut self) -> Result<PreflightReceipt, String> {
            self.preflights += 1;
            Ok(PreflightReceipt::new(42, "items=2"))
        }

        fn run(&mut self) -> u64 {
            self.runs += 1;
            self.runs as u64
        }
    }

    #[test]
    fn parser_accepts_quick_json_and_filter() {
        let outcome = BenchmarkConfig::parse([
            "--quick".to_owned(),
            "--json".to_owned(),
            "--filter".to_owned(),
            "lookup".to_owned(),
        ])
        .unwrap();
        let CliOutcome::Config(config) = outcome else {
            panic!("arguments must produce a configuration");
        };
        assert_eq!(config.samples, QUICK_SAMPLES);
        assert_eq!(config.sample_time, Duration::from_millis(QUICK_SAMPLE_MILLIS));
        assert_eq!(config.warmup_time, Duration::from_millis(QUICK_WARMUP_MILLIS));
        assert!(config.json);
        assert_eq!(config.filter.as_deref(), Some("lookup"));
    }

    #[test]
    fn statistics_include_nearest_rank_p95_and_mad() {
        let samples = [(1, 1.0), (2, 2.0), (3, 3.0), (4, 4.0), (5, 5.0)]
            .into_iter()
            .map(|(elapsed_nanoseconds, nanoseconds_per_operation)| RawSample {
                iterations: 1,
                elapsed: Duration::from_nanos(elapsed_nanoseconds),
                nanoseconds_per_invocation: nanoseconds_per_operation,
                nanoseconds_per_operation,
            })
            .collect::<Vec<_>>();
        let result = statistics(&samples, CaseMetadata::new("test/stats", 1, 1_048_576));
        assert!(result.median_nanoseconds_per_operation.total_cmp(&3.0).is_eq());
        assert!(result.p95_nanoseconds_per_operation.total_cmp(&5.0).is_eq());
        assert!(result.mad_nanoseconds_per_operation.total_cmp(&1.0).is_eq());
        assert_eq!(result.median_mebibytes_per_second, Some(333_333_333.333_333_3));
    }

    #[test]
    fn measurement_uses_a_preflight_collected_before_timing() {
        let mut case = CountingCase::new("test/counting");
        let config = BenchmarkConfig {
            samples: 2,
            sample_time: Duration::from_nanos(1),
            warmup_time: Duration::ZERO,
            ..BenchmarkConfig::default()
        };
        let preflight = preflight_selected_cases(std::slice::from_mut(&mut case), None)
            .unwrap()
            .pop()
            .flatten()
            .unwrap();
        let metadata = case.metadata();
        let measurement = measure_case_with_preflight(&mut case, &config, metadata, preflight);
        assert_eq!(case.preflights, 1);
        assert!(case.runs >= 3);
        assert_eq!(measurement.preflight, PreflightReceipt::new(42, "items=2"));
        assert_eq!(measurement.samples.len(), 2);
        assert!(measurement.samples.iter().all(|sample| sample.iterations > 0));
    }

    #[test]
    fn json_escapes_receipt_details_and_preserves_raw_samples() {
        let measurement = TimingMeasurement {
            metadata: CaseMetadata::new("test/json", 1, 0),
            preflight: PreflightReceipt::new(7, "line=1\nquote=\""),
            iterations_per_sample: 1,
            samples: vec![RawSample {
                iterations: 1,
                elapsed: Duration::from_nanos(12),
                nanoseconds_per_invocation: 12.0,
                nanoseconds_per_operation: 12.0,
            }],
            statistics: TimingStats {
                median_nanoseconds_per_operation: 12.0,
                p95_nanoseconds_per_operation: 12.0,
                mad_nanoseconds_per_operation: 0.0,
                median_mebibytes_per_second: None,
            },
        };
        let json = render_timing_json(&[measurement]);
        assert!(json.contains("\"detail\":\"line=1\\nquote=\\\"\""));
        assert!(json.contains("\"samples\":[{\"iterations\":1"));
        assert!(json.contains("\"nanoseconds_per_invocation\":12.000"));
        assert!(json.contains("\"median_mebibytes_per_second\":null"));
    }

    #[test]
    fn debug_build_guard_matches_the_current_build() {
        if cfg!(debug_assertions) {
            assert_eq!(require_release_build(), Err(BenchmarkError::ReleaseBuildRequired));
        } else {
            assert_eq!(require_release_build(), Ok(()));
        }
    }

    #[test]
    fn throughput_uses_invocation_time_for_multi_operation_cases() {
        let samples = vec![RawSample {
            iterations: 1,
            elapsed: Duration::from_nanos(100),
            nanoseconds_per_invocation: 100.0,
            nanoseconds_per_operation: 50.0,
        }];
        let result = statistics(&samples, CaseMetadata::new("test/batched", 2, 128));
        assert_eq!(result.median_mebibytes_per_second, Some(1_220.703_125));
        let measurement = TimingMeasurement {
            metadata: CaseMetadata::new("test/batched", 2, 128),
            preflight: PreflightReceipt::new(1, "ok"),
            iterations_per_sample: 1,
            samples,
            statistics: result,
        };
        assert!(render_timing_human(&[measurement], &BenchmarkConfig::default()).contains("bytes/invocation"));
    }

    #[test]
    fn normalization_accepts_full_u64_work_units_without_narrowing() {
        let metadata = CaseMetadata::new("test/large", u64::MAX, u64::MAX);
        let normalized = normalized_nanoseconds(Duration::from_nanos(1), u64::MAX, metadata);
        assert!(normalized.is_finite());
        let sample = RawSample {
            iterations: u64::MAX,
            elapsed: Duration::from_nanos(1),
            nanoseconds_per_invocation: normalized_nanoseconds_per_invocation(Duration::from_nanos(1), u64::MAX),
            nanoseconds_per_operation: normalized,
        };
        assert!(
            statistics(&[sample], metadata)
                .median_mebibytes_per_second
                .is_some_and(f64::is_finite)
        );
    }

    #[test]
    fn all_selected_cases_preflight_before_any_timing_work() {
        use std::{cell::Cell, rc::Rc};

        struct FailingCase {
            metadata: CaseMetadata,
            runs: Rc<Cell<usize>>,
        }

        impl BenchmarkCase for FailingCase {
            fn metadata(&self) -> CaseMetadata {
                self.metadata
            }

            fn preflight(&mut self) -> Result<PreflightReceipt, String> {
                Err("not ready".to_owned())
            }

            fn run(&mut self) -> u64 {
                self.runs.set(self.runs.get() + 1);
                0
            }
        }

        let runs = Rc::new(Cell::new(0));
        let mut cases: Vec<Box<dyn BenchmarkCase>> = vec![
            Box::new(CountingCase::new("test/ready")),
            Box::new(FailingCase {
                metadata: CaseMetadata::new("test/failing", 1, 0),
                runs: Rc::clone(&runs),
            }),
        ];
        assert_eq!(
            preflight_selected_cases(&mut cases, None),
            Err(BenchmarkError::Preflight {
                case: "test/failing",
                message: "not ready".to_owned(),
            })
        );
        assert_eq!(runs.get(), 0);
    }

    #[test]
    fn suite_entrypoints_reject_the_opposite_mode_before_build_checks() {
        let mut case = [CountingCase::new("test/mode")];
        let allocation_config = BenchmarkConfig {
            allocations: true,
            ..BenchmarkConfig::default()
        };
        assert_eq!(
            run_suite(&mut case, &allocation_config),
            Err(BenchmarkError::TimingSuiteRequiresTimingMode)
        );
        #[cfg(feature = "allocation-stats")]
        {
            let timing_config = BenchmarkConfig::default();
            assert_eq!(
                run_suite(&mut case, &timing_config),
                Err(BenchmarkError::TimingInstrumentationPresent)
            );
            assert_eq!(
                run_allocation_suite(&mut case, &timing_config),
                Err(BenchmarkError::AllocationSuiteRequiresAllocationMode)
            );
        }
    }
}
