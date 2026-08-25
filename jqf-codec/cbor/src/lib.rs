//! CBOR (RFC 8949) decoder registration and source-backed access provider.
//!
//! One semantic input dialect, [`CBOR_GENERIC_DIALECT_ID`], enforces generic-data-model validity (text UTF-8, selected
//! recognized-tag content, map-key uniqueness per RFC 8949 §5.6.1) on top of structural well-formedness. The four
//! output profiles are distinct registries under the same format, and the catalog matches decoder and encoder against
//! the SAME descriptor dialect list, so one registration carries the input dialect and all four output profiles
//! together.
//!
//! The contract this crate implements is §4.14, reproduced in the module docs of [`parse`], [`equality`], [`encode`],
//! and [`tag`].
//!
//! **Map-key projection narrowing (v1).** A map projects to a semantic object only when every key is a unique direct
//! UTF-8 text string. A map with ANY non-text key — integers, floats, byte strings, arrays, or embedded maps, all
//! legal keys under RFC 8949 §3.2.4 — is therefore refused with `UnsupportedRepresentation` by every route: the
//! whole-document route and the scoped walk both fail at the first non-text key anywhere in the input, on or off the
//! resolved path. This is a documented narrowing, not a spec error; a future pass may stringify or otherwise project
//! non-text keys. See the `parse` module doc for the decoder-side detail.
//!
//! **The adjacent-value law.** The plain `cbor` format declares no [`RouteCapability::AdjacentValues`] and keeps the
//! single-document contract: one source is ONE CBOR item, and every byte after the item is rejected as trailing
//! content. The RFC 8742 sequence framing is its OWN registered format, [`crate::seq`] (`cbor-seq`), whose decode
//! requests opt in with the adjacent-value contract: the decoder stops at the item's end and reports the real consumed
//! offset, so the adjacent-value drive decodes the remainder as the next item. A CBOR item's initial byte self-delimits
//! its length class, so the cut is the decoder's own cursor (grammar business, owned here), and the sequence drive's
//! value-separator set is EMPTY for cbor-seq because `0x20` is a complete item (`-1`), never insignificant whitespace.

#![no_std]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
#![allow(
    clippy::missing_errors_doc,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::cast_lossless,
    clippy::cast_precision_loss,
    clippy::useless_conversion,
    clippy::unnecessary_cast,
    reason = "wire-format bit work (RFC 8949 argument decoding, float widening, epoch math) uses explicit checked casts and conversions"
)]

#[cfg(test)]
extern crate std;

extern crate alloc;

mod big;
mod datetime;
mod decode;
mod encode;
mod equality;
mod error;
mod lazy;
mod parse;
mod provider;
mod read;
mod routes;
/// The cbor-seq (RFC 8742) format identity: concatenated CBOR items served through the adjacent-value contract, with an
/// empty value-separator set. See the module doc for the surface and the registration.
pub mod seq;
mod tag;
mod walk;

use jqf_codec_core::{
    CodecDescriptor, CodecOperations, CodecRegistration, DecoderFactoryRecord, EncoderFactoryRecord, ItemByteOwner,
    RegistrationError, RouteCapability, TagValidatorFactoryRecord,
};
use jqf_data::{DialectIdRef, FormatIdRef};

/// Stable CBOR format identity text.
pub const FORMAT_ID: &str = "cbor";
/// Stable generic-data-model input dialect identity text.
pub const CBOR_GENERIC_DIALECT_ID: &str = "cbor.rfc8949-generic@1";
/// Stable source-echo output-profile identity text.
pub const CBOR_SOURCE_DIALECT_ID: &str = "cbor.source@1";
/// Stable preferred output-profile identity text.
pub const CBOR_PREFERRED_DIALECT_ID: &str = "cbor.preferred@1";
/// Stable core-deterministic output-profile identity text.
pub const CBOR_CORE_DETERMINISTIC_DIALECT_ID: &str = "cbor.core-deterministic@1";
/// Stable length-first output-profile identity text.
pub const CBOR_LENGTH_FIRST_DIALECT_ID: &str = "cbor.length-first@1";

/// The registration's dialect set: the generic input dialect and the four output profiles.
const DIALECTS: [DialectIdRef<'static>; 5] = [
    DialectIdRef::from_static(CBOR_GENERIC_DIALECT_ID),
    DialectIdRef::from_static(CBOR_SOURCE_DIALECT_ID),
    DialectIdRef::from_static(CBOR_PREFERRED_DIALECT_ID),
    DialectIdRef::from_static(CBOR_CORE_DETERMINISTIC_DIALECT_ID),
    DialectIdRef::from_static(CBOR_LENGTH_FIRST_DIALECT_ID),
];

/// Stable physical identity of the complete CBOR document route.
pub const FULL_PHYSICAL_ROUTE_ID: jqf_codec_core::PhysicalRouteId =
    match jqf_codec_core::PhysicalRouteId::derive(crate::FORMAT_ID, 1, 1) {
        Some(id) => id,
        None => panic!("nonzero route identity"),
    };

/// Stable physical identity of deterministic semantic CBOR encoding.
pub const ENCODE_PHYSICAL_ROUTE_ID: jqf_codec_core::PhysicalRouteId =
    match jqf_codec_core::PhysicalRouteId::derive(crate::FORMAT_ID, 3, 1) {
        Some(id) => id,
        None => panic!("nonzero route identity"),
    };

/// Stable physical identity of the native CBOR scoped (exact-path) route.
pub const SCOPED_PHYSICAL_ROUTE_ID: jqf_codec_core::PhysicalRouteId =
    match jqf_codec_core::PhysicalRouteId::derive(crate::FORMAT_ID, 2, 1) {
        Some(id) => id,
        None => panic!("nonzero route identity"),
    };

/// The CLI-facing routes the CBOR registration serves: the source-preserving edit lane (the splice binds per-item
/// header-through-payload spans and the three hooks splice the bytes, so `--edit` over CBOR is served by declaration).
/// Not a record route and never adjacent-values: one CBOR item per source, and the access routes the provider
/// advertises are not CLI-facing capabilities.
const ROUTES: [RouteCapability; 1] = [RouteCapability::Edit];

/// Constructs the allocation-free validated CBOR registration.
pub fn registration() -> Result<CodecRegistration<'static>, RegistrationError> {
    let format = FormatIdRef::from_static(FORMAT_ID);
    CodecRegistration::try_new(
        CodecDescriptor::new(
            format,
            &DIALECTS,
            CodecOperations::new(true, true, true),
            &ROUTES,
            &["cbor"],
            // Binary: a facade newline would corrupt the item, and the codec owns the item's bytes.
            &[ItemByteOwner::Codec; 5],
            &[],
            // No insignificant inter-value bytes: every byte reaches the decoder.
            &[],
        ),
        Some(DecoderFactoryRecord::new(decode::create_provider)),
        Some(EncoderFactoryRecord::new(encode::create_factory)),
        Some(TagValidatorFactoryRecord::new(tag::create_validator)),
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::DIALECTS;

    #[test]
    fn the_registration_dialect_set_has_no_duplicates() {
        let mut seen: alloc::vec::Vec<&str> = alloc::vec::Vec::new();
        for dialect in DIALECTS.iter().map(|d| d.as_str()) {
            assert!(
                !seen.contains(&dialect),
                "dialect {dialect} appears twice in the CBOR set"
            );
            seen.push(dialect);
        }
    }

    /// The adjacent-value law pinned: the plain `cbor` format stays a single-document format — its registration
    /// declares NO [`RouteCapability::AdjacentValues`] route, so the SDK's sequence drive can never select it — while
    /// the registered `cbor-seq` format declares exactly the adjacent-value route. The two halves live in the same
    /// commit: the decode side honors the opt-in (served through [`crate::seq`]) and the plain path's trailing-byte
    /// rejection is unchanged (pinned by `parse::tests::trailing_bytes_are_rejected`).
    #[test]
    fn the_adjacent_value_ruling_is_pinned() {
        use jqf_codec_core::RouteCapability;
        let cbor_routes = super::ROUTES;
        assert!(
            !cbor_routes.contains(&RouteCapability::AdjacentValues),
            "plain cbor declares no adjacent-value route"
        );
        let seq_routes = crate::seq::ROUTES;
        assert_eq!(&seq_routes, &[RouteCapability::AdjacentValues]);
    }

    #[test]
    fn physical_route_ids_are_stable_and_distinct() {
        let ids = [
            super::FULL_PHYSICAL_ROUTE_ID,
            super::ENCODE_PHYSICAL_ROUTE_ID,
            super::SCOPED_PHYSICAL_ROUTE_ID,
        ];
        for (index, left) in ids.iter().enumerate() {
            for right in &ids[index + 1..] {
                assert_ne!(left, right, "physical route ids must be distinct");
            }
        }
    }
}

/// Compiles the README examples as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;
