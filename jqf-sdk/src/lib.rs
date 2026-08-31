//! Portable ordered codec-to-engine-to-codec orchestration.
//!
//! The SDK selects registrations, preserves engine item boundaries, and owns
//! output publication accounting. Format grammar, jq semantics, host I/O, and
//! facade framing policy remain outside this crate.
//!
//! # The one entry point
//!
//! [`execute`] is the SDK's entire routing surface: a caller says what it
//! wants run, over what input, under what options — a [`Request`] — and the
//! SDK picks the drive. The route-named drives are crate-private and
//! reachable only through this function.
//!
//! # Request-thread stack
//!
//! [`try_compile_program`] and [`execute`] recurse over the program tree on the call stack. The
//! documented `10_000` nesting refusal needs a large stack (default 256 MiB).
//! A default OS thread aborts far sooner. [`Request`] and
//! [`ResourceContext`] are `!Send` (`PhantomData<Rc<()>>`), so they cannot
//! hop to another thread after they exist: spawn a sized thread first
//! ([`request_stack_bytes`]), construct the request on that thread, and join
//! only the `Result`. The CLI and the FFI handle do this; an embedder that
//! calls compile or execute on the caller's thread must do the same.

#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
#![allow(
    clippy::missing_errors_doc,
    reason = "pipeline failures preserve closed codec, registry, and sink causes"
)]

mod diagnostics;
mod drive;
mod patch;
mod request;
mod stack;

pub use diagnostics::{Diagnostics, record_json, render_record};
// The embeddability surface: the compile-to-execute path an
// external consumer names, re-exported from the crates that own the types
// (plain `pub use`, never wrappers — the five-crate coupling would just
// move). An embedder needs ONLY `jqf-sdk` plus a codec crate: the compile
// entrypoints and the compiled-program type, the requirement policy they
// take, the resource context they need, the source vocabulary for the input
// slice, and the format/dialect and policy vocabulary for the request.
pub use drive::{
    CatalogIndex,
    CodecCatalog,
    EditRun,
    EncodedItemReport,
    EventStreamReport,
    // `encode_ordered` + `OrderedResultProducer` are the ordered-publication
    // boundary whose only in-tree implementor and caller is the sdk-smoke
    // receipt tool; no production drive publishes through it (see the trait
    // doc in drive/mod.rs).
    FacadeFraming,
    ItemSink,
    OrderedEncodingPolicy,
    OrderedEncodingReport,
    OrderedResultPoll,
    OrderedResultProducer,
    PipelineDisposition,
    PipelineError,
    PipelineFailure,
    PipelinePolicy,
    PipelineReport,
    PublicationStatus,
    RECORD_BATCH_ENTRIES,
    RECORD_BATCH_TARGET_BYTES,
    RaisedError,
    RangeLocateRun,
    RecordIssueReport,
    RecordSequenceReport,
    RegistryFailure,
    RoundtripRun,
    RuntimeMismatchClass,
    SequenceReport,
    SequenceValueError,
    StreamingEventStreamError,
    StreamingSequenceError,
    UNKNOWN_INPUT_LINE,
    decode_record_values,
    decode_source_values,
    encode_ordered,
    is_per_value_codec_kind,
};
pub use jqf_codec_core::{DecodeRequest, DiagnosticPolicy, PreservationRequest, ValidationMode};
pub use jqf_data::{DialectId, FormatId};
pub use jqf_engine::{
    ArithFailure, ArithMismatchOp, CodecRequirementPolicy, CompileOptions, CompiledProgram, PlanRecord,
    try_compile_program,
};
pub use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
pub use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};
pub use patch::{BytePatch, PatchError, PatchSet};
// The one public entry point: the request surface and the single execute.
// Adding a second public route entry point here fails `public_surface.rs`.
pub use request::execute;
pub use request::warm_default_dialect;
pub use request::{DEFAULT_COOPERATIVE_CREDITS, Failure, Input, Outcome, ReadFailure, Report, Request, RequestError};
pub use stack::{DEFAULT_REQUEST_STACK_BYTES, MIN_REQUEST_STACK_BYTES, REQUEST_STACK_BYTES_VAR, request_stack_bytes};

/// Compiles the README examples as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;
