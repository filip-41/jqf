//! Retained TOML parse and encode benchmark inventory.

mod cases;
pub(crate) mod fixtures;

use jqf_bench_core::BenchmarkCase;

/// Returns the retained TOML benchmark inventory.
#[must_use]
pub fn cases() -> Vec<Box<dyn BenchmarkCase>> {
    cases::cases()
}
