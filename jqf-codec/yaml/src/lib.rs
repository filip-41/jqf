//! YAML 1.2.2 decoder registration and source-backed access provider.
//!
//! This crate implements YAML 1.2.2 as a tagged directed graph — not JSON with indentation. One registration carries
//! every input schema dialect and output profile; the factories dispatch on the request's own dialect.

#![cfg_attr(not(test), no_std)]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
#![allow(
    clippy::missing_errors_doc,
    reason = "fallible APIs use closed structured codec errors"
)]
// The scanner/parser are ports of the reference state machines, whose shapes (long case-driven functions, loops,
// matched arms) are the algorithm's own; the pedantic style lints below are noise over that structure.
#![allow(
    clippy::too_many_lines,
    clippy::result_large_err,
    reason = "state-machine functions mirror the reference algorithm's shape; errors carry accounted diagnostics"
)]

extern crate alloc;

mod anchor;
mod block;
mod decode;
mod document;
mod encode;
mod error;
mod graph;
mod key;
mod locate;
mod options;
mod parse;
mod provider;
mod scan;
mod schema;
mod scoped;
mod scoped_build;
mod tag;

/// Compiles the README examples as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;

use jqf_codec_core::{
    AccessSession, CodecDescriptor, CodecOperations, CodecRegistration, DecoderFactoryRecord, EncoderFactoryRecord,
    ItemByteOwner, RegistrationError, RouteCapability, TagValidatorFactoryRecord,
};
use jqf_data::{DialectIdRef, FormatIdRef};

pub use options::YamlTargetSchema;

/// Stable YAML format identity text.
pub const FORMAT_ID: &str = "yaml";
/// Stable YAML failsafe-schema input dialect identity text.
pub const YAML_FAILSAFE_DIALECT_ID: &str = "yaml.failsafe@1";
/// Stable YAML JSON-schema input dialect identity text.
pub const YAML_JSON_DIALECT_ID: &str = "yaml.json@1";
/// Stable YAML core-schema input dialect identity text.
pub const YAML_CORE_DIALECT_ID: &str = "yaml.core@1";
/// Stable YAML stream-canonical output-profile identity text. The target schema on encode comes from request options
/// (`YamlTargetSchema`), default Core.
pub const YAML_STREAM_CANONICAL_DIALECT_ID: &str = "yaml.stream-canonical@1";
/// Stable YAML single-document output-profile identity text.
pub const YAML_SINGLE_DOCUMENT_DIALECT_ID: &str = "yaml.single-document@1";
/// Stable YAML block output-profile identity text.
///
/// The human-readable dialect: block collections, plain scalars wherever the core schema round-trips them, native tags.
/// A plain scalar is admitted only when the core schema reads it back as the same value.
pub const YAML_BLOCK_DIALECT_ID: &str = "yaml.block@1";
/// Stable YAML edit-render output-profile identity text.
///
/// The dialect the CLI names for the source-preserving edit lane (`--edit`): its encoder renders a new value at a
/// splice site in the format's LOCAL syntax — block collections at the splice site's indentation, flow collections
/// inline, plain scalars where the core schema round-trips them — and carries the ruled splice policy (alias refusal,
/// block-scalar whole-span replacement) in [`crate::encode`]'s docs. Behaviorally the block profile; the separate
/// identity is the edit lane's own output namespace, exactly as TOML's `toml.jqf-1.0@1` is.
pub const YAML_JQF_1_0_DIALECT_ID: &str = "yaml.jqf-1.0@1";

/// The insignificant inter-document trivia of a YAML document stream (§4.8): blank lines and other whitespace between
/// `---`/`...` markers. The adjacent-value drive skips it between complete documents; the set is byte-identical to
/// JSON's whitespace because YAML's separators are line-based.
pub const VALUE_SEPARATORS: &[u8] = b" \t\n\r";

/// Constructs the allocation-free validated YAML codec registration.
///
/// One registration carries EVERY YAML dialect: the input schema dialects (`yaml.failsafe@1`, `yaml.json@1`,
/// `yaml.core@1`) and the output profiles (`yaml.stream-canonical@1`, `yaml.single-document@1`, `yaml.block@1`,
/// `yaml.jqf-1.0@1`). The catalog matches the decoder and the encoder against the SAME list, and the factories dispatch
/// on the request's own dialect — the decoder maps an input dialect to its schema ladder, the encoder maps an output
/// dialect to its profile.
pub fn registration() -> Result<CodecRegistration<'static>, RegistrationError> {
    let format = FormatIdRef::from_static(FORMAT_ID);
    CodecRegistration::try_new(
        CodecDescriptor::new(
            format,
            &ALL_DIALECTS,
            CodecOperations::new(true, true, true),
            &ROUTES,
            &["yaml", "yml"],
            // Input dialects (core/failsafe/json) retain their edit document's trailing byte; output profiles
            // (stream-canonical/single-document/block/jqf-1.0) have the facade supply the item newline.
            &[
                ItemByteOwner::Codec,
                ItemByteOwner::Codec,
                ItemByteOwner::Codec,
                ItemByteOwner::Facade,
                ItemByteOwner::Facade,
                ItemByteOwner::Facade,
                ItemByteOwner::Facade,
            ],
            &[],
            // Inter-document trivia is JSON whitespace (§4.8): blank lines between documents are skipped by the drive's
            // separator scan.
            VALUE_SEPARATORS,
        ),
        Some(DecoderFactoryRecord::new(decode::create_provider)),
        Some(EncoderFactoryRecord::new(encode::create_factory)),
        Some(TagValidatorFactoryRecord::new(tag::create_validator)),
        None,
    )
}

/// Every dialect the YAML registration serves: the three input schema dialects and the four output profiles. The CORE
/// dialect is FIRST because the catalog resolves an extension's default INPUT dialect as `descriptor.dialects()[0]`
/// (`detect_by_extension`'s documented law) and core is YAML's default input schema.
pub const ALL_DIALECTS: [DialectIdRef<'static>; 7] = [
    DialectIdRef::from_static(YAML_CORE_DIALECT_ID),
    DialectIdRef::from_static(YAML_FAILSAFE_DIALECT_ID),
    DialectIdRef::from_static(YAML_JSON_DIALECT_ID),
    DialectIdRef::from_static(YAML_STREAM_CANONICAL_DIALECT_ID),
    DialectIdRef::from_static(YAML_SINGLE_DOCUMENT_DIALECT_ID),
    DialectIdRef::from_static(YAML_BLOCK_DIALECT_ID),
    DialectIdRef::from_static(YAML_JQF_1_0_DIALECT_ID),
];

/// The CLI-facing routes the YAML registration serves: the adjacent-value input model (YAML is NOT single-document — a
/// `---`-separated document stream takes the adjacent-value route), the edit lane (the codec binds retained source
/// spans and supplies the edit-render dialect and splice policy), and the record route's output side (an
/// NDJSON/json-seq/CSV/TSV stream publishing YAML items — `RouteCapability::Record`). The record drive plans such an
/// output SERIAL: the block profile's inter-document `---` is stream state in the encoder factory, which a morsel
/// worker rebuilding its own factory cannot reproduce.
const ROUTES: [RouteCapability; 3] = [
    RouteCapability::AdjacentValues,
    RouteCapability::Edit,
    RouteCapability::Record,
];

/// Stable physical identity of the complete YAML document route.
pub const FULL_PHYSICAL_ROUTE_ID: jqf_codec_core::PhysicalRouteId =
    match jqf_codec_core::PhysicalRouteId::derive(crate::FORMAT_ID, 1, 1) {
        Some(id) => id,
        None => panic!("nonzero route identity"),
    };

/// Stable physical identity of the native YAML scoped (exact-path) route.
pub const SCOPED_PHYSICAL_ROUTE_ID: jqf_codec_core::PhysicalRouteId =
    match jqf_codec_core::PhysicalRouteId::derive(crate::FORMAT_ID, 2, 1) {
        Some(id) => id,
        None => panic!("nonzero route identity"),
    };

/// Stable physical identity of deterministic semantic YAML encoding.
pub const ENCODE_PHYSICAL_ROUTE_ID: jqf_codec_core::PhysicalRouteId =
    match jqf_codec_core::PhysicalRouteId::derive(crate::FORMAT_ID, 3, 1) {
        Some(id) => id,
        None => panic!("nonzero route identity"),
    };

/// Decodes every document in one YAML stream under the core schema, one owned [`jqf_data::Value`] per document in
/// stream order (§4.8: each document is one ordered unit-stream item).
///
/// This is the codec-level unit-stream product, used by the conformance differential and available to SDK callers that
/// need the whole stream at once. It drives the multi-yield whole-document session directly and materializes each
/// document's root. The SDK's ordered sequence drive (reopen-at-offset per document) is the streaming follow-up.
///
/// # Errors
///
/// Returns a structured [`jqf_codec_core::CodecError`] when the stream cannot be decoded to its semantic documents.
pub fn decode_documents(
    source: jqf_source::ResolvedSource<'_>,
    resources: &mut jqf_resource::ResourceContext<'_>,
) -> Result<alloc::vec::Vec<jqf_data::Value>, jqf_codec_core::CodecError> {
    let mut state = parse::YamlParseState::try_new(
        source,
        provider::DialectKind::Core,
        None,
        jqf_data::BuilderCoverage::minimal_semantic(),
        false,
        resources,
    )?;
    let mut values = alloc::vec::Vec::new();
    while !state.stream_done() {
        let mut context = jqf_codec_core::CodecRunContext::new(resources);
        context.set_cooperative_credits(4_096);
        // The trailing-StreamEnd decode (the session's "no more documents" terminal, the poll-era `Complete`) sets
        // `stream_done` and errors with the data contract; the driver treats exactly that as the clean end — a genuine
        // failure leaves `stream_done` false and propagates.
        match state.decode(jqf_codec_core::AccessInput::Source(source), &mut context) {
            Ok(result) => {
                let jqf_codec_core::AccessOutcome::FullDocument(product) = result.outcome() else {
                    return Err(jqf_codec_core::CodecError::new(
                        jqf_codec_core::CodecFailureKind::RequirementMismatch,
                    ));
                };
                let value = product
                    .document()
                    .materialize_root(resources)
                    .map_err(document::map_data)?;
                values.push(value);
            }
            Err(_) if state.stream_done() => break,
            Err(error) => return Err(error),
        }
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::{CodecOperations, RegistrationError};
    use jqf_codec_core::{CodecDescriptor, CodecRegistration};

    #[test]
    fn one_registration_serves_every_dialect() {
        // The YAML codec registers ONCE; the single registration lists every input schema dialect and output profile,
        // and the factories dispatch on the request's own dialect. This test pins that inventory: the one list is
        // exactly ALL_DIALECTS.
        let registration = super::registration().expect("registration");
        let descriptor = registration.descriptor();
        assert_eq!(descriptor.format().as_str(), super::FORMAT_ID);
        assert_eq!(descriptor.operations(), CodecOperations::new(true, true, true));
        assert!(registration.decoder().is_some());
        assert!(registration.encoder().is_some());
        assert!(registration.tag_validator().is_some());
        let ids: alloc::vec::Vec<&str> = descriptor.dialects().iter().map(|d| d.as_str()).collect();
        let expected: alloc::vec::Vec<&str> = super::ALL_DIALECTS.iter().map(|d| d.as_str()).collect();
        assert_eq!(ids, expected, "the one registration lists every dialect");
    }

    #[test]
    fn duplicate_dialects_are_rejected() {
        let dialects = [
            jqf_data::DialectIdRef::from_static(super::YAML_CORE_DIALECT_ID),
            jqf_data::DialectIdRef::from_static(super::YAML_CORE_DIALECT_ID),
        ];
        let result = CodecRegistration::try_new(
            CodecDescriptor::new(
                jqf_data::FormatIdRef::from_static(super::FORMAT_ID),
                &dialects,
                CodecOperations::new(false, false, false),
                &[],
                &[],
                // The duplicate check fires before the framing arity check.
                &[],
                &[],
                &[],
            ),
            None,
            None,
            None,
            None,
        );
        assert!(matches!(result, Err(RegistrationError::DuplicateDialect)));
    }

    #[test]
    fn physical_route_ids_are_stable() {
        assert_eq!(super::FULL_PHYSICAL_ROUTE_ID, super::FULL_PHYSICAL_ROUTE_ID);
        assert_eq!(super::ENCODE_PHYSICAL_ROUTE_ID, super::ENCODE_PHYSICAL_ROUTE_ID);
        // The identities are DERIVED from (format, kind, specialization), so the stability pin is the derivation
        // itself: the constant must equal what the triple derives.
        let derive = |kind, spec| jqf_codec_core::PhysicalRouteId::derive("yaml", kind, spec).expect("derived");
        assert_eq!(super::FULL_PHYSICAL_ROUTE_ID, derive(1, 1));
        assert_eq!(super::SCOPED_PHYSICAL_ROUTE_ID, derive(2, 1));
        assert_eq!(super::ENCODE_PHYSICAL_ROUTE_ID, derive(3, 1));
    }
}
