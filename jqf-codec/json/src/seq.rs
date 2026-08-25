//! The json-seq codec: RFC 7464 JSON Text Sequences, a record-stream format layered over strict JSON.
//!
//! This module owns PHYSICAL framing for record streams whose payloads are strict-JSON texts separated by the ASCII
//! record separator (RS, 0x1E). It shares the unit-stream, lease, ordering, and worker machinery with NDJSON
//! ([`crate::ndjson`]); it does NOT share NDJSON's line rules — a raw RS always establishes the next boundary, even
//! mid-string or mid-container, and LF/CR are ordinary payload whitespace inside a possible-JSON.
//!
//! The registered surface is deliberately narrow: one input dialect, `json-seq.strict@1`, and one output dialect,
//! `json-seq.jqf@1`. Recovering behavior is [`JsonSeqProfile::Recovering`], never a registered dialect.
//!
//! ## json-seq is explicit, never inferred
//!
//! Selecting `json-seq` is always an explicit request. RS-framed input is never auto-detected, and this module must
//! never be reachable by inference.

pub use crate::options::{JsonSeqDecodeOptions, JsonSeqEncodeOptions, JsonSeqProfile, JsonSeqSuffix};
pub use jqf_codec_core::record_options::{
    JSON_SEQ_FORMAT_ID as FORMAT_ID, JSON_SEQ_JQF_DIALECT_ID as JQF_DIALECT_ID,
    JSON_SEQ_STRICT_DIALECT_ID as STRICT_DIALECT_ID,
};

mod boundary;
mod error;
mod provider;
mod render;
mod stream;

use jqf_codec_core::{
    CodecDescriptor, CodecError, CodecOperations, CodecRegistration, DiagnosticPolicy, EncoderFactoryRecord,
    ErasedRecordStreamProvider, ItemByteOwner, RecordProviderFactoryRecord, RegistrationError, RouteCapability,
    ValidationMode,
};
use jqf_data::{DialectIdRef, FormatIdRef};
use jqf_resource::ResourceContext;
use jqf_source::ResolvedSource;

const FORMAT: FormatIdRef<'static> = FormatIdRef::from_static(FORMAT_ID);
// The descriptor's dialect set is what the catalog's encode lookup matches: the strict input identity and the jqf
// output identity. `json-seq.recover@1` is RESERVED and never listed.
const DIALECTS: [DialectIdRef<'static>; 2] = [
    DialectIdRef::from_static(STRICT_DIALECT_ID),
    DialectIdRef::from_static(JQF_DIALECT_ID),
];

/// The CLI-facing routes the json-seq registration serves: the record route (input and output — the `--seq` flag and
/// the json-seq input/output formats) and the adjacent-value input model.
const ROUTES: [RouteCapability; 2] = [RouteCapability::Record, RouteCapability::AdjacentValues];

/// Registers the `json-seq` format's ENCODE side.
///
/// Only encoding goes through the registry. Decoding a json-seq stream is not a decoder-factory operation at all: it
/// produces a RECORD STREAM, not one document, so it is reached through [`create_record_provider`] and the record ABI.
/// The registration declares that honestly — `CodecOperations` advertises encode only — rather than pretending a
/// record stream is a document the access binder could bind.
pub fn registration() -> Result<CodecRegistration<'static>, RegistrationError> {
    CodecRegistration::try_new(
        CodecDescriptor::new(
            FORMAT,
            &DIALECTS,
            CodecOperations::new(false, true, false),
            &ROUTES,
            // json-seq has no extension in common use, so its registration declares none: a `.jsonseq` file falls back
            // to json exactly like any other unrecognized extension.
            &[],
            // The record codec owns the RS prefix and the LF suffix.
            &[ItemByteOwner::Codec, ItemByteOwner::Codec],
            &[],
            // RFC 8259 insignificant inter-value whitespace.
            crate::VALUE_SEPARATORS,
        ),
        None,
        Some(EncoderFactoryRecord::new(render::create_registered_factory)),
        None,
        Some(RecordProviderFactoryRecord::new(provider::create_registered_provider)),
    )
}

pub use error::framing_text as issue_text;
pub use jqf_codec_core::RECORD_ROUTE_SLOT;

/// Opens one json-seq record-stream provider over contiguous retained input.
///
/// The provider advertises exactly one route, [`RECORD_ROUTE_SLOT`], whose result kind is
/// [`jqf_codec_core::AccessResultKind::RecordStream`]. Opening it yields a framer that hands out payload RANGES; the
/// caller decodes each range through the ordinary strict-JSON access ladder, which is what lets a record route reuse
/// the recycled session and shared schema prototype the adjacent-value path already owns.
pub fn create_record_provider<'source>(
    source: ResolvedSource<'source>,
    profile: JsonSeqProfile,
    options: JsonSeqDecodeOptions,
    diagnostics: DiagnosticPolicy,
    validation: ValidationMode,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedRecordStreamProvider<'source>, CodecError> {
    provider::create_record_provider(source, profile, options, diagnostics, validation, resources)
}
