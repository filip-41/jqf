//! Native host capabilities for jqf.
//!
//! Threads, the filesystem, and the two morsel drives. Portable crates stay `no_std`; this crate is the only one that
//! may spawn a thread or open a temp directory. The portable grant and work-budget laws live in `jqf-resource`.

#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod feed;
mod output;
pub use output::JsonItemSuffix;
pub mod parallel;
pub mod records;
pub mod spill;
pub mod values;
pub mod workers;

/// Compiles the README examples as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;
