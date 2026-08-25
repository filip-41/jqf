//! Standalone release-mode performance harness for `jqf-resource`.

mod cases;

#[cfg(feature = "allocation-stats")]
#[global_allocator]
static ALLOCATOR: jqf_bench_core::allocation::MeasuringAllocator = jqf_bench_core::allocation::MeasuringAllocator;

use std::process::ExitCode;

use jqf_bench_core::{
    BenchmarkCase, BenchmarkConfig, CliOutcome, render_timing_human, render_timing_json, run_suite, usage,
};
#[cfg(feature = "allocation-stats")]
use jqf_bench_core::{render_allocation_human, render_allocation_json, run_allocation_suite};

fn main() -> ExitCode {
    match main_result() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn main_result() -> Result<(), String> {
    let CliOutcome::Config(config) = BenchmarkConfig::from_env().map_err(|error| error.to_string())? else {
        println!("{}", usage());
        return Ok(());
    };
    let mut cases = cases::cases();

    if config.allocations {
        return allocation_main(&mut cases, &config);
    }

    let measurements = run_suite(&mut cases, &config).map_err(|error| error.to_string())?;
    print!(
        "{}",
        if config.json {
            render_timing_json(&measurements)
        } else {
            render_timing_human(&measurements, &config)
        }
    );
    Ok(())
}

#[cfg(feature = "allocation-stats")]
fn allocation_main(cases: &mut [Box<dyn BenchmarkCase>], config: &BenchmarkConfig) -> Result<(), String> {
    let measurements = run_allocation_suite(cases, config).map_err(|error| error.to_string())?;
    print!(
        "{}",
        if config.json {
            render_allocation_json(&measurements)
        } else {
            render_allocation_human(&measurements)
        }
    );
    Ok(())
}

#[cfg(not(feature = "allocation-stats"))]
fn allocation_main(_cases: &mut [Box<dyn BenchmarkCase>], _config: &BenchmarkConfig) -> Result<(), String> {
    Err(
        "allocation measurement requires `cargo run --release -p jqf-resource-bench \
         --features allocation-stats -- --allocations`"
            .to_owned(),
    )
}
