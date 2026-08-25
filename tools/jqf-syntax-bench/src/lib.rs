//! Deterministic successful-path benchmarks for `jqf-syntax`.

mod cases;
mod fixtures;

use jqf_bench_core::BenchmarkCase;

/// Builds the exact stable benchmark inventory.
#[must_use]
pub fn cases() -> Vec<Box<dyn BenchmarkCase>> {
    cases::cases()
}
