//! The jq builtin vocabulary and the value/registry core of jqf.
//!
//! This crate owns the `{semantics, registry}` unit, plus the error vocabulary, the result contracts, and the
//! constant-evaluation helpers.
//! `jqf-engine` keeps the compile/analysis/exec machinery and depends on this crate one-way. The program arena and the
//! arena-fact analysis live in `jqf-engine`: they are one module with the compile pipeline that builds them, and
//! nothing on this side of the boundary reads them.
//!
//! This crate's pub surface is INTERNAL: the wide visibility exists so the engine's private re-export shims
//! (`crate::semantics`, `crate::registry`, …) keep their spellings. The public API of the product is `jqf_engine`'s
//! curated `lib.rs`.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::new_without_default,
    clippy::return_self_not_must_use,
    clippy::should_implement_trait,
    reason = "the pub surface is this crate's internal boundary; the               curated public API lives in jqf-engine, which keeps these               obligations"
)]

extern crate alloc;

pub mod codec_result;
pub mod constant;
pub mod error;
pub mod host;
pub mod registry;
pub mod selector;
pub mod semantics;
