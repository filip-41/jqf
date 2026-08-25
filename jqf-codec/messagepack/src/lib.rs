//! `MessagePack` (spec.md) decoder registration and source-backed access provider.
//!
//! One semantic input dialect, [`MESSAGEPACK_UTF8_DIALECT_ID`], enforces UTF-8 validity on every `str` payload during
//! validation; the [`MESSAGEPACK_KEY_EQUIVALENCE_DIALECT_ID`] dialect behaves exactly like `utf8@1` and additionally
//! rejects a map key repeated under the native key-equivalence law (a registered identity must resolve to behaviour,
//! not to nothing); the [`MESSAGEPACK_WIRE_DIALECT_ID`] identity is REGISTERED and **unadvertised** — on valid input
//! the two dialects agree, and an invalid-UTF-8 `str` fails the semantic document build with
//! `UnsupportedRepresentation` under `wire@1` where `utf8@1` rejects it during the scan. Two output profiles: the
//! [`MESSAGEPACK_DETERMINISTIC_DIALECT_ID`] default, which emits the shortest exact marker for every value and
//! preserves map occurrence order, and the [`MESSAGEPACK_DETERMINISTIC_FLOAT64_DIALECT_ID`] opt-in, which is the same
//! grammar except that a `Decimal` is encoded as its nearest IEEE-754 binary64 float instead of refused (the precision
//! loss is in the dialect's identity).
//!
//! The contract this crate implements is §4.15 of the codec-portfolio design, in the five-function shape (`Options` /
//! `scan` / `materialize` / `encode` / `registration`), reproduced in the module docs of [`scan`], [`materialize`], and
//! [`encode`].
//!
//! **Arbitrary-key maps.** A map projects to a semantic object only when every key is a `str`. Any other key set makes
//! the whole-document build terminate with `UnsupportedRepresentation` — never a silent entry-array conversion.
//! Duplicate `str` keys are NOT rejected: object projection uses jqf's own first-position/final-value duplicate law
//! (`jqf_data::ObjectBuilder`).
//!
//! **Extension identity.** Extension `n` projects to `Value::Tagged { tag: "msgpack:ext:<n>", payload: Bytes }` with
//! `<n>` a canonical signed decimal over the signed 8-bit space. Extension `-1` is the timestamp: in-core-range → UTC
//! [`OffsetDateTime`] retaining the intrinsic tag `"msgpack:ext:-1"`; out-of-range → the exact `{seconds,
//! nanoseconds}` tagged object; an invalid reserved `-1` payload is rejected at the semantic build
//! (`UnsupportedRepresentation`).
//!
//! **The adjacent-value law.** The `messagepack` format declares no [`RouteCapability::AdjacentValues`] and keeps the
//! single-document contract: one source is ONE `MessagePack` object, and every byte after the object is rejected as
//! trailing content.

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
    reason = "wire-format bit work (marker argument decoding, float widening, epoch math) uses explicit checked casts and conversions"
)]

#[cfg(test)]
extern crate std;

extern crate alloc;

mod decode;
mod encode;
mod error;
mod keys;
mod lazy;
mod marker;
mod materialize;
mod options;
mod provider;
mod routes;
mod scan;
mod tag;
mod walk;

#[cfg(test)]
mod test_support;

use jqf_codec_core::{
    CodecDescriptor, CodecOperations, CodecRegistration, DecoderFactoryRecord, EncoderFactoryRecord, ItemByteOwner,
    RegistrationError, RouteCapability, TagValidatorFactoryRecord,
};
use jqf_data::{DialectIdRef, FormatIdRef};

pub use options::{
    MESSAGEPACK_DETERMINISTIC_DIALECT_ID, MESSAGEPACK_DETERMINISTIC_FLOAT64_DIALECT_ID,
    MESSAGEPACK_KEY_EQUIVALENCE_DIALECT_ID, MESSAGEPACK_UTF8_DIALECT_ID, MESSAGEPACK_WIRE_DIALECT_ID,
};

/// Stable `MessagePack` format identity text.
pub const FORMAT_ID: &str = "messagepack";

/// The registration's dialect set: the advertised UTF-8 input dialect, the key-equivalence input dialect (the
/// duplicate-key rejection is its observable behaviour), the registered-unadvertised wire identity, and the two output
/// profiles (the exact deterministic default and the lossy float64 opt-in). The catalog matches decoder and encoder
/// against the SAME descriptor list, so one registration carries the input dialects and the output profiles together.
const DIALECTS: [DialectIdRef<'static>; 5] = [
    DialectIdRef::from_static(MESSAGEPACK_UTF8_DIALECT_ID),
    DialectIdRef::from_static(MESSAGEPACK_KEY_EQUIVALENCE_DIALECT_ID),
    DialectIdRef::from_static(MESSAGEPACK_WIRE_DIALECT_ID),
    DialectIdRef::from_static(MESSAGEPACK_DETERMINISTIC_DIALECT_ID),
    DialectIdRef::from_static(MESSAGEPACK_DETERMINISTIC_FLOAT64_DIALECT_ID),
];

/// The CLI-facing routes the `MessagePack` registration serves: the source-preserving edit lane (per-item spans bind
/// header-through-payload; a leaf edit re-encodes its span in place, while every structural growth/shrink changes the
/// count-bearing header the insert/cut seams cannot express, so those splices decline to the whole-document floor —
/// served by declaration, not by splice mechanics). Not a record route and never adjacent-values: one `MessagePack`
/// object per source, and the access routes the provider advertises are not CLI-facing capabilities.
const ROUTES: [RouteCapability; 1] = [RouteCapability::Edit];

/// Stable physical identity of whole-document decoding.
///
/// # Panics
///
/// Panics when the route identity cannot be derived (the identity is nonzero by construction for these fixed inputs).
#[must_use]
pub(crate) const fn decode_route_id() -> jqf_codec_core::PhysicalRouteId {
    match jqf_codec_core::PhysicalRouteId::derive(FORMAT_ID, 1, 1) {
        Some(id) => id,
        None => panic!("nonzero route identity"),
    }
}

/// Stable physical identity of the native scoped (exact-path) route.
#[must_use]
pub(crate) const fn scoped_route_id() -> jqf_codec_core::PhysicalRouteId {
    match jqf_codec_core::PhysicalRouteId::derive(FORMAT_ID, 2, 1) {
        Some(id) => id,
        None => panic!("nonzero route identity"),
    }
}

/// Stable physical identity of deterministic semantic encoding. Kind 3 matches the CBOR convention (encode=(3,1),
/// located/scoped=(2,1)): two different routes must not share one identity, or a route receipt cannot tell them apart.
#[must_use]
pub(crate) const fn encode_route_id() -> jqf_codec_core::PhysicalRouteId {
    match jqf_codec_core::PhysicalRouteId::derive(FORMAT_ID, 3, 1) {
        Some(id) => id,
        None => panic!("nonzero route identity"),
    }
}

/// Constructs the allocation-free validated `MessagePack` registration.
pub fn registration() -> Result<CodecRegistration<'static>, RegistrationError> {
    CodecRegistration::try_new(
        CodecDescriptor::new(
            FormatIdRef::from_static(FORMAT_ID),
            &DIALECTS,
            CodecOperations::new(true, true, true),
            &ROUTES,
            &["msgpack", "mpk"],
            // Binary: a facade newline would corrupt the item.
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
    use super::{CodecOperations, RegistrationError};

    #[test]
    fn the_route_identities_are_pairwise_distinct() {
        // Two different routes must not share one physical identity — a byte-identical (format, kind, spec) triple
        // makes their receipts indistinguishable. The CBOR convention is decode=(1,1), scoped/located=(2,1),
        // encode=(3,1).
        let decode = super::decode_route_id();
        let scoped = super::scoped_route_id();
        let encode = super::encode_route_id();
        assert_ne!(decode, scoped);
        assert_ne!(decode, encode);
        assert_ne!(scoped, encode);
    }

    #[test]
    fn the_registration_is_valid_and_serves_its_dialects() {
        let registration = super::registration().expect("registration");
        let descriptor = registration.descriptor();
        assert_eq!(descriptor.format().as_str(), super::FORMAT_ID);
        assert_eq!(
            descriptor.dialects().len(),
            5,
            "utf8@1 + key-equivalence@1 + wire@1 + deterministic@1 + deterministic-float64@1"
        );
        assert_eq!(descriptor.operations(), CodecOperations::new(true, true, true));
        assert_eq!(descriptor.extensions(), ["msgpack", "mpk"]);
        assert!(registration.decoder().is_some());
        assert!(registration.encoder().is_some());
        assert!(registration.tag_validator().is_some());
    }

    /// `messagepack.wire@1` is REGISTERED (present in the descriptor's dialect set, so a direct SDK request can name
    /// it) but never advertised — the CLI and the capability gate's surface table carry only `messagepack.utf8@1`
    /// (the args.rs single-table law makes advertisement a separate fact from registration).
    #[test]
    fn the_wire_dialect_is_registered_but_not_advertised() {
        let registration = super::registration().expect("registration");
        let names: alloc::vec::Vec<&str> = registration
            .descriptor()
            .dialects()
            .iter()
            .map(|dialect| dialect.as_str())
            .collect();
        assert!(
            names.contains(&super::MESSAGEPACK_WIRE_DIALECT_ID),
            "wire@1 is registered"
        );
        assert!(names.contains(&super::MESSAGEPACK_UTF8_DIALECT_ID));
        assert!(names.contains(&super::MESSAGEPACK_KEY_EQUIVALENCE_DIALECT_ID));
        assert!(names.contains(&super::MESSAGEPACK_DETERMINISTIC_DIALECT_ID));
        assert!(names.contains(&super::MESSAGEPACK_DETERMINISTIC_FLOAT64_DIALECT_ID));
    }

    #[test]
    fn the_dialect_set_has_no_duplicates() {
        let registration = super::registration().expect("registration");
        let mut seen: alloc::vec::Vec<&str> = alloc::vec::Vec::new();
        for dialect in registration.descriptor().dialects() {
            let name = dialect.as_str();
            assert!(
                !seen.contains(&name),
                "dialect {name} appears twice in the MessagePack set"
            );
            seen.push(name);
        }
    }

    #[test]
    fn a_bad_registration_is_rejected() {
        let dialects = [
            jqf_data::DialectIdRef::from_static(super::MESSAGEPACK_UTF8_DIALECT_ID),
            jqf_data::DialectIdRef::from_static(super::MESSAGEPACK_UTF8_DIALECT_ID),
        ];
        let result = jqf_codec_core::CodecRegistration::try_new(
            jqf_codec_core::CodecDescriptor::new(
                jqf_data::FormatIdRef::from_static(super::FORMAT_ID),
                &dialects,
                CodecOperations::new(true, true, true),
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
        assert!(result.is_err());
        let _ = RegistrationError::DuplicateDialect;
    }
}

/// Compiles the README examples as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;
