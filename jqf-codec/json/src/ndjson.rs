//! Record-stream codecs layered over strict JSON.
//!
//! This module owns PHYSICAL framing for record streams whose payloads are strict-JSON texts. Today that is one format,
//! `ndjson`, with two sealed profiles. It owns record boundaries, ordinals, terminators, recovery law, and output
//! framing policy; it owns no JSON grammar whatsoever, and every payload operation is delegated to `jqf-codec-json`'s
//! strict ladder.
//!
//! ## NDJSON is explicit, never inferred
//!
//! Default stdin is a stream of adjacent JSON texts separated by arbitrary whitespace, and jqf's default input keeps
//! exactly that behaviour. It is not NDJSON: it permits any RFC 8259 whitespace between values, accepts values with no
//! physical newline boundary at all, has no physical record ordinal, and cannot express blank-record, byte-order-mark,
//! or final-terminator laws. Selecting `ndjson` is therefore always an explicit request. Newline- separated input is
//! NEVER auto-detected as NDJSON, and this module must never be reachable by inference.

pub use crate::options::{NdjsonDecodeOptions, NdjsonEncodeOptions, NdjsonProfile, NdjsonTerminator};
pub use jqf_codec_core::record_options::{
    NDJSON_FORMAT_ID as FORMAT_ID, NDJSON_RECOVERING_DIALECT_ID as RECOVERING_DIALECT_ID,
    NDJSON_STRICT_DIALECT_ID as STRICT_DIALECT_ID,
};

pub(crate) mod boundary;
pub(crate) mod error;
pub(crate) mod provider;
pub(crate) mod render;
pub(crate) mod stream;

use jqf_codec_core::{
    CodecDescriptor, CodecError, CodecOperations, CodecRegistration, DiagnosticPolicy, EncoderFactoryRecord,
    ErasedRecordStreamProvider, ItemByteOwner, RecordProviderFactoryRecord, RegistrationError, RouteCapability,
    ValidationMode,
};
use jqf_data::{DialectIdRef, FormatIdRef};
use jqf_resource::ResourceContext;
use jqf_source::ResolvedSource;

const FORMAT: FormatIdRef<'static> = FormatIdRef::from_static(FORMAT_ID);
const DIALECTS: [DialectIdRef<'static>; 2] = [
    DialectIdRef::from_static(STRICT_DIALECT_ID),
    DialectIdRef::from_static(RECOVERING_DIALECT_ID),
];

/// The routes the NDJSON registration declares. An ndjson INPUT selection always takes the record route (whole input,
/// streamed stdin, and `--follow` all ride it), so [`RouteCapability::AdjacentValues`] is never the input path for
/// ndjson-named bytes; its live consumer is the adjacent-value lane law elsewhere — e.g. the SDK's multi-item
/// publication check (`jqf-sdk/src/drive/encode.rs`) reads the OUTPUT registration's capability set to decide whether a
/// second published item needs the stream treatment.
const ROUTES: [RouteCapability; 2] = [RouteCapability::Record, RouteCapability::AdjacentValues];

/// Registers the `ndjson` format's ENCODE side.
///
/// Only encoding goes through the registry. Decoding an NDJSON stream is not a decoder-factory operation at all: it
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
            &["ndjson", "jsonl"],
            // The record codec owns the terminator inside its staging buffer.
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

/// Opens one NDJSON record-stream provider over contiguous retained input.
///
/// The provider advertises exactly one route, [`RECORD_ROUTE_SLOT`], whose result kind is
/// [`jqf_codec_core::AccessResultKind::RecordStream`]. Opening it yields a framer that hands out payload RANGES; the
/// caller decodes each range through the ordinary strict-JSON access ladder, which is what lets a record route reuse
/// the recycled session and shared schema prototype the adjacent-value path already owns.
///
/// The runtime reaches a record stream through the codec registration's record-provider factory, never by naming this
/// crate. This free function is the direct entry, which the CLI's record input ladder and the workspace's smoke and
/// fuzz drivers use.
pub fn create_record_provider<'source>(
    source: ResolvedSource<'source>,
    profile: NdjsonProfile,
    options: NdjsonDecodeOptions,
    diagnostics: DiagnosticPolicy,
    validation: ValidationMode,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedRecordStreamProvider<'source>, CodecError> {
    provider::create_record_provider(source, profile, options, diagnostics, validation, resources)
}
