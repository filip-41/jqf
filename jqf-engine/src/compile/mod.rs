//! Gate-to-finish program compilation over the shared syntax frontend.
//!
//! This is the CLI-to-engine seam. [`try_compile_program`] in [`pipeline`] runs
//! the compile pipeline — prelude gate, parse, bind, lower, transform, analyze,
//! finish — and produces an opaque [`CompiledProgram`]. Finish packs one
//! [`AccessPlan`]. [`CompiledProgram::try_requirement`] charges that plan.
//! [`CompiledProgram::execute`] tries the committed shortcut, then the residual
//! graph. [`CompiledProgram::host_io`] is what the SDK matches for retained-byte
//! echo and span cut.
//!
//! This module owns [`EngineCompileError`] and the pipeline orchestration;
//! [`ParseRejection`]/[`UnsupportedConstruct`] live in
//! `jqf_builtins::constant` and are re-exported here so the
//! public `jqf_engine::` surface is unchanged.
//! It does not derive analysis facts (that is [`crate::analysis`]),
//! store the arena (that is [`crate::program`]), or build codec requirements
//! (that is [`crate::codec_requirement`]). The residual graph and document
//! oracle answers live in [`crate::exec`].
//!
//! Sibling layout: [`lower`] lowers AST to an arena, [`program`] is the opaque
//! artifact and finish, [`access`] packs and charges codec access, [`shortcut`]
//! is the closed job sum finish commits, [`execute`] is the thin job match,
//! [`transform`] marks and rewrites the fused arena, [`pipeline`] orchestrates
//! gate through finish, [`error`] is the rejection surface, [`stdlib`] is
//! prelude source.

pub(crate) use jqf_builtins::constant::try_copy_str;
pub use jqf_builtins::constant::{ParseRejection, UnsupportedConstruct};

mod prelude;
#[allow(clippy::wildcard_imports)]
pub(crate) use prelude::*;

mod access;
mod error;
mod execute;
mod lower;
mod parse;
mod pipeline;
mod prelude_gate;
mod program;
mod shortcut;
mod stdlib;
mod transform;

pub(crate) use access::AccessPlan;
pub(crate) use shortcut::{Access, Shortcut, commit as commit_shortcut, commit_access};

pub use program::{CompiledProgram, HostIo};

#[cfg(test)]
pub(crate) use stdlib::{EXTENSION_NAMES, EXTENSION_PRELUDE, STDLIB_NAMES, STDLIB_PRELUDE};
#[cfg(test)]
pub(crate) use transform::keyed_collect_keys;

pub use error::EngineCompileError;
pub use pipeline::{CompileOptions, try_compile_program};

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
