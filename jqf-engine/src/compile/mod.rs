//! Gate-to-finish program compilation over the shared syntax frontend.
//!
//! This is the CLI-to-engine seam. [`try_compile_program`] in [`pipeline`] runs
//! the compile pipeline — prelude gate, parse, bind, lower, transform, analyze,
//! finish — and produces an opaque [`CompiledProgram`].
//! [`CompiledProgram::try_requirement`]
//! lowers the derived split into a codec [`AccessRequirement`], reusing the
//! existing tested requirement lowerings; it invents no new requirement
//! vocabulary. [`CompiledProgram::try_run`] carries the program's optional (`?`)
//! flags and its residual graph into the engine's typed run interpretation.
//!
//! This module owns [`EngineCompileError`] and the pipeline orchestration;
//! [`ParseRejection`]/[`UnsupportedConstruct`] live in
//! `jqf_builtins::constant` and are re-exported here so the
//! public `jqf_engine::` surface is unchanged.
//! It does not derive analysis facts (that is [`crate::analysis`]),
//! store the arena (that is [`crate::program`]), build codec requirements
//! (that is [`crate::codec_requirement`]), or interpret run outcomes (that
//! is [`crate::exec`]).
//!
//! Sibling layout: [`lower`] lowers AST to an arena, [`program`] is the compiled
//! artifact, [`pipeline`] orchestrates gate through finish, [`error`] is the
//! rejection surface, [`stdlib`] is prelude source.

pub(crate) use jqf_builtins::constant::try_copy_str;
pub use jqf_builtins::constant::{ParseRejection, UnsupportedConstruct};

mod prelude;
#[allow(clippy::wildcard_imports)]
pub(crate) use prelude::*;

mod error;
mod lower;
mod parse;
mod pipeline;
mod prelude_gate;
mod program;
mod stdlib;
mod transform;

#[cfg(test)]
pub(crate) use stdlib::{EXTENSION_NAMES, EXTENSION_PRELUDE, STDLIB_NAMES, STDLIB_PRELUDE};
pub(crate) use transform::push_rewrite_node;

pub use error::EngineCompileError;
pub use pipeline::{CompileOptions, try_compile_program};
pub use program::CompiledProgram;

#[cfg(test)]
pub(crate) use program::{COUNT_DEMAND_DERIVATIONS, DERIVATION_LOCK, ELEMENT_DEMAND_DERIVATIONS};

#[cfg(test)]
mod demand_cache;
#[cfg(test)]
mod element_probe;
#[cfg(test)]
mod lower_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod type_probe;

#[doc(hidden)]
pub use prelude_gate::{PreludeGate, scan_prelude_gate};
