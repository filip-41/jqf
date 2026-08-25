//! Which allocator the `jqf` binary links, and why the choice lives here.
//!
//! A global allocator is a WHOLE-PROGRAM property: exactly one crate in a link may declare it, and declaring it forces
//! `std`. The library crates stay allocator-agnostic — they allocate through `alloc`/`std` and never name a provider —
//! but `jqf-resource` supplies the counting wrapper ([`CountingAlloc`](jqf_resource:CountingAlloc)) that the ambient
//! ledger charges through, and this binary supplies the provider it wraps (mimalloc or `System`). Nothing below is
//! visible to any other library crate.
//!
//! The default is mimalloc, MEASURED (design doc `.docs-intenal/ perf-allocator-and-width-policy.md` §3): macOS
//! `libmalloc`'s multithreaded path taxes the record drive's per-record document construction roughly 2x per thread,
//! which is what made `--workers 2` slower than serial. mimalloc removes that tax outright.
//!
//! It is a feature, and `--no-default-features` restores the platform allocator, because the A/B behind the default has
//! to stay buildable from the shipping tree. jemalloc and snmalloc were measured against mimalloc in the same bake-off
//! and both lost; §3 records their numbers and the two manifest lines that reproduce them, so neither is carried as a
//! dependency nobody links.

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: jqf_resource::CountingAlloc<mimalloc::MiMalloc> = jqf_resource::CountingAlloc(mimalloc::MiMalloc);

#[cfg(not(feature = "mimalloc"))]
#[global_allocator]
static GLOBAL: jqf_resource::CountingAlloc<std::alloc::System> = jqf_resource::CountingAlloc(std::alloc::System);

/// The allocator this binary links, for diagnostics and benchmark provenance.
pub const NAME: &str = if cfg!(feature = "mimalloc") {
    "mimalloc"
} else {
    "system"
};
