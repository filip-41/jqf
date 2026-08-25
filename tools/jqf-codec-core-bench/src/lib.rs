//! Retained algebra benchmarks for exact codec demand and access binding.

mod cases;
mod fixtures;

use jqf_bench_core::BenchmarkCase;

/// Returns the complete retained Batch 2 benchmark inventory.
#[must_use]
pub fn cases() -> Vec<Box<dyn BenchmarkCase>> {
    cases::cases()
}
