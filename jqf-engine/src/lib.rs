//! The compile, analysis, and execution engine.
//!
//! Parser output from `jqf-syntax` is lowered into an executable arena graph,
//! analyzed for demand and fast-lane eligibility, and interpreted by `exec/`.
//! Exact root and static-forward input needs are also lowered into codec-core
//! requirements. Decoded nodes stay with their authoritative documents until
//! an explicit semantic construction barrier produces an independently owned
//! value.

#![no_std]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
#![allow(
    clippy::missing_errors_doc,
    reason = "fallible APIs preserve codec-core's closed structured error vocabulary"
)]

extern crate alloc;

mod analysis;
mod codec_requirement;
mod compile;
mod exec;
mod explain;
mod plan;
mod program;

pub use analysis::{BoundaryConsumer, ProjectionClass};
pub use codec_requirement::{
    CodecRequirementPolicy, StaticForwardStep, try_lower_forward_requirement, try_lower_root_requirement,
};
pub use compile::{
    CompileOptions, CompiledProgram, EngineCompileError, ParseRejection, UnsupportedConstruct, try_compile_program,
};
pub use exec::{EngineRun, EngineRunStream, FactDelta, RunInput, RunPoll};
pub use exec::{
    InputSource, InputSourceError, InputSourceHandle, LoadedModule, ModuleLoader, ModuleLoaderHandle, with_input_source,
};
pub use explain::ExplainPlan;
pub use jqf_builtins::codec_result::{CodecInputOutcome, CodecInputResult, EngineResult};
pub use jqf_builtins::error::message::{dump_trunc_owned, kind_name, raised_body, raised_frame_note};
pub use jqf_builtins::error::{ArithFailure, ArithMismatchOp, EngineRunError};
pub use jqf_builtins::registry::{
    BuiltinExample, BuiltinExecution, BuiltinFamilyId, BuiltinFamilyRecord, BuiltinOverloadId, BuiltinOverloadRecord,
    DemandTransfer, Effects, PRELUDE_ENUMERATED, ParameterKind, SemanticRevision, builtin_families, builtin_overloads,
    resolve_builtin, resolve_family,
};
pub use jqf_builtins::semantics::decode::json as decode_json;
pub use jqf_builtins::semantics::decode::json_sequence as decode_json_sequence;
pub use jqf_builtins::semantics::decode::{JsonStreamStep, PruneHint, json_stream_next, json_stream_next_hinted};
pub use jqf_builtins::semantics::order::values_semantically_equal;
pub use jqf_builtins::semantics::rawtext::is_raw_text;
pub use jqf_builtins::semantics::stream_events::{EventParser, StreamEvent};
pub use jqf_builtins::semantics::truth::{PublicationFacts, is_empty_array, is_truthy, publication_facts};
pub use plan::{PlanError, PlanRecord};

#[doc(hidden)]
pub use compile::{PreludeGate, scan_prelude_gate};

/// Compiles the README examples as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;
