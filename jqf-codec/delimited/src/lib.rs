//! Record-stream codecs over delimited payloads.
//!
//! This crate owns PHYSICAL framing for record streams whose payloads are RFC 4180 delimited records. Today that is two
//! formats, `csv` (the frozen RFC pair `csv.rfc4180@1` input / `csv.jqf-rfc4180@1` output and the Unicode-capable twin
//! `csv.utf8@1` input / `csv.jqf-utf8@1` output, each with a headered dialect) and `tsv` (the no-quote grammar,
//! `tsv.utf8@1` input / `tsv.jqf-lf@1` output), each with a headered dialect pair. It owns record boundaries, ordinals,
//! terminators, and the quote-aware framing law (the TSV grammar skips quote state entirely — `"` is field data); it
//! owns no field grammar whatsoever — field splitting, quoting, and the header row are the payload codec's, reached
//! later by narrowing the same retained source to the record's byte range.
//!
//! ## The two CSV input families
//!
//! `csv.rfc4180@1` implements the RFC its id names: an unquoted field admits only `TEXTDATA = %x20-21 / %x23-2B /
//! %x2D-7E`, a quoted field only TEXTDATA, comma, CR, LF, and the `""` escape — TAB, NUL, other C0/DEL, and every
//! non-ASCII scalar are `InvalidInput` even when the bytes are valid UTF-8. `csv.utf8@1` is the Unicode-capable
//! sibling: the same quoting grammar, admitting every valid-UTF-8 scalar. The short `--input-format csv` selects the
//! utf8 family; the RFC-named dialects are explicit `--input-dialect` opt-ins.
//!
//! ## Delimited input is explicit, never inferred
//!
//! Selecting `csv` or `tsv` is always an explicit request. Delimited input is never auto-detected, and this crate must
//! never be reachable by inference.

#![no_std]
#![deny(missing_docs)]
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]
#![allow(
    clippy::missing_errors_doc,
    reason = "fallible APIs use the codec crate's closed structured error vocabulary"
)]

extern crate alloc;

mod boundary;
mod byte_scan;
mod column;
mod decode;
mod encode;
mod error;
mod fields;
mod header;
mod options;
mod provider;
mod scan;
mod stream;
mod tag;

pub use jqf_codec_core::record_options::{
    CSV_FORMAT_ID as FORMAT_ID, CSV_JQF_RFC4180_DIALECT_ID as JQF_RFC4180_DIALECT_ID,
    CSV_JQF_RFC4180_HEADER_DIALECT_ID as JQF_RFC4180_HEADER_DIALECT_ID, CSV_JQF_UTF8_DIALECT_ID as JQF_UTF8_DIALECT_ID,
    CSV_JQF_UTF8_HEADER_DIALECT_ID as JQF_UTF8_HEADER_DIALECT_ID, CSV_RFC4180_DIALECT_ID as RFC4180_DIALECT_ID,
    CSV_RFC4180_HEADER_DIALECT_ID as RFC4180_HEADER_DIALECT_ID, CSV_UTF8_DIALECT_ID as UTF8_DIALECT_ID,
    CSV_UTF8_HEADER_DIALECT_ID as UTF8_HEADER_DIALECT_ID, TSV_FORMAT_ID, TSV_JQF_LF_DIALECT_ID,
    TSV_JQF_LF_HEADER_DIALECT_ID, TSV_UTF8_DIALECT_ID, TSV_UTF8_HEADER_DIALECT_ID,
};
pub use options::{CsvDecodeOptions, CsvEncodeOptions, is_headered_delimited_dialect, is_valid_delimiter};

use jqf_codec_core::{
    CodecDescriptor, CodecError, CodecOperations, CodecRegistration, DecoderFactoryRecord, DiagnosticPolicy,
    EncoderFactoryRecord, ItemByteOwner, RecordProviderFactoryRecord, RegistrationError, RouteCapability,
    TagValidatorFactoryRecord, ValidationMode,
};
use jqf_data::{DialectIdRef, FormatIdRef};
use jqf_resource::ResourceContext;
use jqf_source::ResolvedSource;

const FORMAT: FormatIdRef<'static> = FormatIdRef::from_static(FORMAT_ID);
const DIALECTS: [DialectIdRef<'static>; 8] = [
    DialectIdRef::from_static(RFC4180_DIALECT_ID),
    DialectIdRef::from_static(RFC4180_HEADER_DIALECT_ID),
    DialectIdRef::from_static(UTF8_DIALECT_ID),
    DialectIdRef::from_static(UTF8_HEADER_DIALECT_ID),
    DialectIdRef::from_static(JQF_RFC4180_DIALECT_ID),
    DialectIdRef::from_static(JQF_RFC4180_HEADER_DIALECT_ID),
    DialectIdRef::from_static(JQF_UTF8_DIALECT_ID),
    DialectIdRef::from_static(JQF_UTF8_HEADER_DIALECT_ID),
];

const TSV_FORMAT: FormatIdRef<'static> = FormatIdRef::from_static(TSV_FORMAT_ID);
const TSV_DIALECTS: [DialectIdRef<'static>; 4] = [
    DialectIdRef::from_static(TSV_UTF8_DIALECT_ID),
    DialectIdRef::from_static(TSV_UTF8_HEADER_DIALECT_ID),
    DialectIdRef::from_static(TSV_JQF_LF_DIALECT_ID),
    DialectIdRef::from_static(TSV_JQF_LF_HEADER_DIALECT_ID),
];

/// The CLI-facing routes the CSV registration serves: the record route (the payload decoder serves one document per
/// record range), the adjacent-value input model (the streaming-stdin drive frames CSV records by quote-state walk),
/// and the source-preserving edit lane (the record-route edit splices field spans, so `--edit` over csv/tsv is served
/// by declaration).
const ROUTES: [RouteCapability; 3] = [
    RouteCapability::Record,
    RouteCapability::AdjacentValues,
    RouteCapability::Edit,
];

/// Registers the `csv` format.
///
/// Decoding a CSV STREAM is a record-stream operation reached through [`create_record_provider`] and the record ABI,
/// not a decoder-factory operation. But the RECORD DRIVE (`jqf-sdk::execute_record_sequence`) opens one provider over
/// the whole retained source and decodes each record's byte range through the catalog's decoder for the payload format
/// — which for CSV is CSV itself. So the registration also declares a DECODER whose factory builds the single-record
/// payload provider ([`decode::create_payload_provider`]); that is the "one document per record range" decode the
/// record drive calls.
///
/// `csv` owns only `.csv`: the `.tsv` extension is the separate [`registration_tsv`] format's, and comma-parsing a
/// tab-separated file would be silently wrong — the second registration exists so each format owns exactly its
/// extension.
pub fn registration() -> Result<CodecRegistration<'static>, RegistrationError> {
    CodecRegistration::try_new(
        CodecDescriptor::new(
            FORMAT,
            &DIALECTS,
            CodecOperations::new(true, true, true),
            &ROUTES,
            &["csv"],
            // The record codec owns the CRLF/LF terminator of every record.
            &[ItemByteOwner::Codec; 8],
            &[],
            // No insignificant inter-value bytes: every byte reaches the decoder.
            &[],
        ),
        Some(DecoderFactoryRecord::new(decode::create_payload_provider)),
        Some(EncoderFactoryRecord::new(encode::create_registered_factory)),
        Some(TagValidatorFactoryRecord::new(tag::create_validator)),
        Some(RecordProviderFactoryRecord::new(provider::create_registered_provider)),
    )
}

/// Registers the `tsv` format: tab-delimited, no-quote grammar, the four sealed dialect ids (`tsv.utf8@1` /
/// `tsv.utf8-header@1` input, `tsv.jqf-lf@1` / `tsv.jqf-lf-header@1` output), the same route set and the same factories
/// as `csv` (the physical route ids are SHARED). The grammar lives on the options struct the caller builds with the TSV
/// constructor; this registration is a descriptor row plus the `.tsv` extension.
pub fn registration_tsv() -> Result<CodecRegistration<'static>, RegistrationError> {
    CodecRegistration::try_new(
        CodecDescriptor::new(
            TSV_FORMAT,
            &TSV_DIALECTS,
            CodecOperations::new(true, true, true),
            &ROUTES,
            &["tsv"],
            // The record codec owns the LF terminator of every record.
            &[ItemByteOwner::Codec; 4],
            &[],
            // No insignificant inter-value bytes: every byte reaches the decoder.
            &[],
        ),
        Some(DecoderFactoryRecord::new(decode::create_payload_provider)),
        Some(EncoderFactoryRecord::new(encode::create_registered_factory)),
        Some(TagValidatorFactoryRecord::new(tag::create_validator)),
        Some(RecordProviderFactoryRecord::new(provider::create_registered_provider)),
    )
}

pub use decode::SCOPED_PHYSICAL_ROUTE_ID;
pub use jqf_codec_core::RECORD_ROUTE_SLOT;

/// Compiles the README examples as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;

/// Opens one RFC 4180 record-stream provider over contiguous retained input.
///
/// The provider advertises exactly one route, [`RECORD_ROUTE_SLOT`], whose result kind is
/// [`jqf_codec_core::AccessResultKind::RecordStream`]. Opening it yields a framer that hands out payload RANGES; the
/// caller decodes each range through the ordinary access ladder, which is what lets a record route reuse the recycled
/// session the adjacent-value path already owns.
pub fn create_record_provider<'source>(
    source: ResolvedSource<'source>,
    options: CsvDecodeOptions,
    diagnostics: DiagnosticPolicy,
    validation: ValidationMode,
    resources: &mut ResourceContext<'_>,
) -> Result<jqf_codec_core::ErasedRecordStreamProvider<'source>, CodecError> {
    if validation != ValidationMode::Strict {
        return Err(CodecError::new(jqf_codec_core::CodecFailureKind::RequirementMismatch));
    }
    provider::create_record_provider(source, options, diagnostics, resources)
}
