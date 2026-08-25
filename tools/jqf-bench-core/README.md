# jqf-bench-core

Shared std-only harness for repository-owned benchmark workers.

What it has:

- `BenchmarkCase` — one untimed preflight plus a mutable one-shot operation
- `BenchmarkConfig` / `CliOutcome` — shared CLI (`--quick`, `--filter`, `--json`, `--samples`, `--sample-ms`, `--warmup-ms`, `--allocations`)
- `run_suite` — calibrate, warm, retain samples, median/p95/MAD
- `run_allocation_suite` — allocation-stats path (feature-gated)
- `limits` — shared measured-region resource envelopes

## Timing worker

Workers implement mutable cases and call `run_suite`. Debug builds are rejected.

```rust
use jqf_bench_core::{
    BenchmarkCase, BenchmarkConfig, CaseMetadata, CliOutcome, PreflightReceipt,
    render_timing_human, render_timing_json, run_suite, usage,
};

struct Case;

impl BenchmarkCase for Case {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new("example/run", 1, 1_024)
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        Ok(PreflightReceipt::new(0x4a51, "items=1 checksum=0x4a51"))
    }

    fn run(&mut self) -> u64 {
        0x4a51
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let CliOutcome::Config(config) = BenchmarkConfig::from_env()? else {
        println!("{usage()}");
        return Ok(());
    };
    let measurements = run_suite(&mut [Case], &config)?;
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
```

`--allocations` selects a separately compiled measurement path. Timing workers
must be built without `allocation-stats`; `run_suite` rejects allocation-mode
configuration and allocation-instrumented builds. `run_allocation_suite`
requires `--allocations`.

## Allocation worker

Enable `allocation-stats` and install the supplied allocator:

```rust
#[cfg(feature = "allocation-stats")]
#[global_allocator]
static ALLOCATOR: jqf_bench_core::allocation::MeasuringAllocator =
    jqf_bench_core::allocation::MeasuringAllocator;
```

Call `run_allocation_suite` only for `config.allocations`. Reports allocation
calls, reallocation calls, requested bytes, peak live requested bytes, and
retained requested bytes. `run_allocation_suite` first performs an exact
allocation/reallocation self-check so forgetting the global allocator cannot
silently report zeroes.
