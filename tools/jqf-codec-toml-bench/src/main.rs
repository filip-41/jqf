//! Light TOML codec benchmark worker.
//!
//! Same-fixture lanes: jqf whole-document decode, jqf deterministic encode,
//! and the two maintained Rust TOML parsers (`toml`, `toml_edit`) as
//! competitors. Every measured case has an untimed correctness preflight so
//! the timing never measures a wrong result, and the jqf decode lane asserts
//! the sealed physical route receipt before timing.

use jqf_bench_core::{BenchmarkConfig, CliOutcome, render_timing_human, render_timing_json, run_suite};

#[cfg(feature = "allocation-stats")]
#[global_allocator]
static ALLOCATOR: jqf_bench_core::allocation::MeasuringAllocator = jqf_bench_core::allocation::MeasuringAllocator;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let CliOutcome::Config(config) = BenchmarkConfig::from_env()? else {
        println!("{}", jqf_bench_core::usage());
        return Ok(());
    };
    let mut cases = jqf_codec_toml_bench::cases();
    if config.allocations {
        #[cfg(feature = "allocation-stats")]
        {
            let measurements = jqf_bench_core::run_allocation_suite(&mut cases, &config)?;
            print!(
                "{}",
                if config.json {
                    jqf_bench_core::render_allocation_json(&measurements)
                } else {
                    jqf_bench_core::render_allocation_human(&measurements)
                }
            );
            return Ok(());
        }
        #[cfg(not(feature = "allocation-stats"))]
        return Err("allocation mode requires --features allocation-stats".into());
    }
    let measurements = run_suite(&mut cases, &config)?;
    // The machine-readable receipt is what makes a before/after A/B diff
    // over a lever (the json bench protocol) possible.
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
