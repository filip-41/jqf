//! Integration tests that do not need the counting allocator.
//!
//! One binary, cases split by area. Two extra binaries exist because they install `CountingAlloc` as the process
//! allocator (`counting_alloc.rs`, `worker_ambient.rs`); that cannot be a module of this target.

#[path = "cases/grants.rs"]
mod grants;
