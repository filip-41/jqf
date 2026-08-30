//! Format-neutral codec contracts: registration, access, encode, record streams, failures.
//!
//! `no_std` + `alloc`. No parser. Format crates depend on this crate; the engine and selector name it without importing
//! a format.
//!
//! Demand clauses refuse at bind; result authority may fall through to the whole-document adapter. Impossible states
//! are [`CodecFailureKind::InternalContractViolation`].

#![no_std]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
#![allow(
    clippy::missing_errors_doc,
    reason = "fallible APIs use the crate's closed structured error vocabulary"
)]

extern crate alloc;

mod access;
mod binder;
pub mod byte_scan;
mod capability;
pub mod comment;
mod deferral;
mod demand;
mod descriptor;
mod encode;
mod erased;
mod error;
mod execution;
mod fallback;
pub mod markup;
mod number;
mod pattern;
mod preservation;
mod product;
mod project;
mod provider;
mod prune;
mod record;
pub mod record_options;
mod registration;
mod report;
mod request;
mod schedule;
mod tag;

#[cfg(test)]
pub(crate) mod test_support;

pub use access::{
    AccessAdapter, AccessBindError, AccessHandle, AccessInput, AccessOutcome, AccessRequirement, AccessResult,
    AccessSession, CapabilityMismatch, CoveragePolicy, ErasedAccessSession, FactIntent, PhysicalRouteId,
    PhysicalRouteReceipt, RecycledSessionState, RouteDescription, RouteSlot, markup_measure_demand,
    required_builder_coverage, required_builder_coverage_with, requirement_wants_intrinsic_tag,
    whole_document_open_plan,
};
pub use binder::ReusableAccessSession;
pub use capability::{AccessFootprintKind, AccessGuarantees, AccessResultKind, CapabilityBundle, RouteCapability};
pub use deferral::{committed_container_spans, declined_deferrals, record_declined_deferral, record_published_spans};
pub use demand::{ATTACHED_FACT_ROLES, CodecDemand, DemandClause, SourceCapabilityDemand, TopologyDemand};
pub use descriptor::{CodecDescriptor, CodecOperations, ItemByteOwner};
pub use encode::{
    ByteSink, EditAppendMembers, EditInsertion, EditRemoval, EditRemoveMembers, EditRenameMembers, EditReplacement,
    EncoderFactoryImpl, EncoderSession, ErasedEncoderSession, FactEditPatch, ReusableEncoderSession, VecByteSink,
    line_statement_cut,
};
pub use erased::{ErasedEncoderFactory, ErasedProvider, ErasedTagValidator};

/// Fact role: this node's authored span must not be patched in place. The payload is the refusal message. The edit lane
/// reads the role by identity.
pub const EDIT_REFUSAL_ROLE: &str = "edit-refusal";

/// Fact role: this member was inherited through a merge, not authored on the host. The payload is the host mapping's
/// node id. The edit lane splices into the host.
pub const MERGE_OVERRIDE_ROLE: &str = "merge-override";

pub use error::{CodecError, CodecFailureKind, data_contract, diagnosed, map_data, map_span_materialization_error};
pub use execution::CodecRunContext;
pub use number::{decimal_render, decimal_render_into, widen_f32};
pub use pattern::{AccessFootprint, ExactPath, OwnedStep, PortableStep, own_steps};
pub use preservation::{PreservationOutcome, PreservationReport};
pub use product::{DocumentProduct, EncodeItem, ExactSelectionRecord, LocatedOutcome, LocatedProduct};
pub use project::{
    NativeSpellings, ProjectableScalar, ProjectionSink, TagLayer, TrackedProjectionSink, classify_scalar, project_tag,
    tag_layer, value_tag_layer, view_tag_layer,
};
pub use provider::{InputProvider, ProviderInput};
pub use prune::{PRUNE_ALL, PruneLookup, PruneRef, PruneTree, PruneTreeError};
pub use record::{
    ErasedRecordStreamProvider, ErasedRecordStreamSession, RecordBatch, RecordBatchLimit, RecordCompletion,
    RecordEntry, RecordIssue, RecordIssueCode, RecordIssueSeverity, RecordItem, RecordLease, RecordOrdinal, RecordPoll,
    RecordProviderOpen, RecordStreamAbort, RecordStreamProvider, RecordStreamSession, RecordTerminator,
};
pub use record_options::{
    CSV_FORMAT_ID, CSV_JQF_RFC4180_DIALECT_ID, CSV_JQF_RFC4180_HEADER_DIALECT_ID, CSV_JQF_UTF8_DIALECT_ID,
    CSV_JQF_UTF8_HEADER_DIALECT_ID, CSV_RFC4180_DIALECT_ID, CSV_RFC4180_HEADER_DIALECT_ID, CSV_UTF8_DIALECT_ID,
    CSV_UTF8_HEADER_DIALECT_ID, JSON_FORMAT_ID, JSON_SEQ_FORMAT_ID, JSON_SEQ_JQF_DIALECT_ID,
    JSON_SEQ_STRICT_DIALECT_ID, NDJSON_FORMAT_ID, NDJSON_RECOVERING_DIALECT_ID, NDJSON_STRICT_DIALECT_ID,
    RECORD_ROUTE_SLOT, RFC8259_DIALECT_ID, TSV_FORMAT_ID, TSV_JQF_LF_DIALECT_ID, TSV_JQF_LF_HEADER_DIALECT_ID,
    TSV_UTF8_DIALECT_ID, TSV_UTF8_HEADER_DIALECT_ID,
};
pub use registration::{
    CodecRegistration, DecoderFactory, DecoderFactoryRecord, EncoderFactoryRecord, RecordProviderFactory,
    RecordProviderFactoryRecord, RegistrationError, TagValidatorFactoryRecord,
};
pub use report::AccessReport;
pub use request::{DecodeRequest, DiagnosticPolicy, EncodeRequest, PreservationRequest, ValidationMode};
pub use schedule::{SelectionOrigin, SelectionSchedule};
pub use tag::{NoTagsValidator, TagValidator};

/// Compiles the README examples as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;
