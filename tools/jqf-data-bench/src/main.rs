use jqf_bench_core::{BenchmarkConfig, CliOutcome, render_timing_human, render_timing_json, run_suite, usage};

#[cfg(feature = "allocation-stats")]
#[global_allocator]
static ALLOCATOR: jqf_bench_core::allocation::MeasuringAllocator = jqf_bench_core::allocation::MeasuringAllocator;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    jqf_data_bench::verify_build_source()?;
    let CliOutcome::Config(config) = BenchmarkConfig::parse(arguments)? else {
        println!("{}", usage());
        return Ok(());
    };
    let mut cases = jqf_data_bench::cases();

    #[cfg(feature = "allocation-stats")]
    if config.allocations {
        let measurements = jqf_bench_core::run_allocation_suite(&mut cases, &config)?;
        print!(
            "{}",
            if config.json {
                jqf_data_bench::bind_worker_json(&jqf_bench_core::render_allocation_json(&measurements))?
            } else {
                jqf_bench_core::render_allocation_human(&measurements)
            }
        );
        jqf_data_bench::verify_build_source()?;
        return Ok(());
    }

    let measurements = run_suite(&mut cases, &config)?;
    print!(
        "{}",
        if config.json {
            jqf_data_bench::bind_worker_json(&render_timing_json(&measurements))?
        } else {
            render_timing_human(&measurements, &config)
        }
    );
    jqf_data_bench::verify_build_source()?;
    Ok(())
}
