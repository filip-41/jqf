use jqf_bench_core::{BenchmarkConfig, CliOutcome, render_timing_human, render_timing_json, run_suite, usage};

#[cfg(feature = "allocation-stats")]
#[global_allocator]
static ALLOCATOR: jqf_bench_core::allocation::MeasuringAllocator = jqf_bench_core::allocation::MeasuringAllocator;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let CliOutcome::Config(config) = BenchmarkConfig::from_env()? else {
        println!("{}", usage());
        return Ok(());
    };
    let mut cases = jqf_codec_core_bench::cases();
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
