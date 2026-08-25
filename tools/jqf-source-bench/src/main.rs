//! Standalone release-mode performance harness for `jqf-source`.

use std::process::ExitCode;

use jqf_bench_core::{BenchmarkConfig, CliOutcome, usage};

#[cfg(feature = "allocation-stats")]
#[global_allocator]
static ALLOCATOR: jqf_bench_core::allocation::MeasuringAllocator = jqf_bench_core::allocation::MeasuringAllocator;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let CliOutcome::Config(config) = BenchmarkConfig::from_env()? else {
        println!("{}", usage());
        return Ok(());
    };
    let mut cases = jqf_source_bench::cases();
    render(&mut cases, &config)
}

#[cfg(not(feature = "allocation-stats"))]
fn render(
    cases: &mut [Box<dyn jqf_bench_core::BenchmarkCase>],
    config: &BenchmarkConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    if config.allocations {
        return Err("allocation measurement requires --features allocation-stats".into());
    }
    let measurements = jqf_bench_core::run_suite(cases, config)?;
    let output = if config.json {
        jqf_bench_core::render_timing_json(&measurements)
    } else {
        jqf_bench_core::render_timing_human(&measurements, config)
    };
    print!("{output}");
    Ok(())
}

#[cfg(feature = "allocation-stats")]
fn render(
    cases: &mut [Box<dyn jqf_bench_core::BenchmarkCase>],
    config: &BenchmarkConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    if !config.allocations {
        return Err("timing requires a build without --features allocation-stats".into());
    }
    let measurements = jqf_bench_core::run_allocation_suite(cases, config)?;
    let output = if config.json {
        jqf_bench_core::render_allocation_json(&measurements)
    } else {
        jqf_bench_core::render_allocation_human(&measurements)
    };
    print!("{output}");
    Ok(())
}
