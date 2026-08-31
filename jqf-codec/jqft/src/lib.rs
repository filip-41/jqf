//! The jqft native document format family: the `jqft` human text profile, the `jqfjson` JSON envelope, and the `jqfb`
//! binary image — one schema authority, three renderings.
//!
//! Core values (null/bool/exact number/binary64/string/bytes/the four temporal categories/array/object) round-trip
//! through jqft and jqfb. Tags are first-class grammar (`@tag("name") value`, chains by repetition, outermost first)
//! and are retained as [`jqf_data::Value::Tagged`]. Comments parse and attach as comment facts; they are not the value.
//! Markup nodes decode as an array of ordered children with name/attribute/content facts. Anchors, aliases, namespaced
//! markup names, and non-string object keys are refused with a dedicated diagnostic, never silently dropped.
//!
//! The text formats advertise two slots, Whole/`CompleteDocument` and Exact/`Located`; the binary `jqfb` image uses
//! the same pair, with Exact served by the node-table walk.

#![cfg_attr(not(test), no_std)]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
#![allow(
    clippy::missing_errors_doc,
    reason = "fallible APIs use closed structured codec errors"
)]

extern crate alloc;

mod decode;
mod encode;
mod error;
mod jqfb;
mod jqfb_decode;
mod jqfb_encode;
mod jqfb_routes;
mod json_escape;
mod locate;
mod parse;
mod provider;
mod scoped;

/// Compiles the README examples as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;

use jqf_codec_core::{
    CodecDescriptor, CodecOperations, CodecRegistration, DecoderFactoryRecord, EncoderFactoryRecord,
    ErasedTagValidator, ItemByteOwner, RegistrationError, RouteCapability, TagValidatorFactoryRecord,
};
use jqf_data::{DialectIdRef, FormatIdRef};

/// The shared JSON escape-table accessor, re-exported so json's cross-implementation receipt can compare this copy to
/// the owner table (see `json_escape`).
pub use json_escape::json_escape_byte;
pub use options::{JqfbEncodeOptions, JqftEncodeOptions};

mod options;

/// Stable jqft format identity text.
pub const FORMAT_ID: &str = "jqft";
/// Stable jqfjson format identity text.
pub const JQFJSON_FORMAT_ID: &str = "jqfjson";
/// Stable jqfb format identity text (the machine profile).
pub const FORMAT_ID_JQFB: &str = "jqfb";
/// Stable jqft input dialect identity text.
pub const JQFT_DOCUMENT_DIALECT_ID: &str = "jqft.document@1";
/// Stable jqft canonical output-profile identity text.
pub const JQFT_CANONICAL_DIALECT_ID: &str = "jqft.canonical@1";
/// Stable jqfjson input dialect identity text.
pub const JQFJSON_DOCUMENT_DIALECT_ID: &str = "jqfjson.document@1";
/// Stable jqfjson canonical output-profile identity text.
pub const JQFJSON_CANONICAL_DIALECT_ID: &str = "jqfjson.canonical@1";
/// Stable jqfb input dialect identity text.
pub const JQFB_DOCUMENT_DIALECT_ID: &str = "jqfb.document@1";
/// Stable jqfb canonical output-profile identity text.
pub const JQFB_CANONICAL_DIALECT_ID: &str = "jqfb.canonical@1";

/// The insignificant inter-document trivia of a jqft document stream: blank lines between `---`-separated documents.
/// The adjacent-value drive skips it; the set is byte-identical to JSON's `Ws` set (the jqft grammar's own trivia
/// alphabet). Crate-private: no consumer outside this registration.
const VALUE_SEPARATORS: &[u8] = b" \t\n\r";

/// The jqft registration's dialect set: its input dialect and its output profile. The catalog matches BOTH the decoder
/// and the encoder against the SAME descriptor list, so a registration carries its input dialect and output profile
/// together.
const JQFT_DIALECTS: [DialectIdRef<'static>; 2] = [
    DialectIdRef::from_static(JQFT_DOCUMENT_DIALECT_ID),
    DialectIdRef::from_static(JQFT_CANONICAL_DIALECT_ID),
];

/// The jqfjson registration's dialect set.
const JQFJSON_DIALECTS: [DialectIdRef<'static>; 2] = [
    DialectIdRef::from_static(JQFJSON_DOCUMENT_DIALECT_ID),
    DialectIdRef::from_static(JQFJSON_CANONICAL_DIALECT_ID),
];

/// Stable physical identity of the complete jqft document route.
pub const JQFT_FULL_PHYSICAL_ROUTE_ID: jqf_codec_core::PhysicalRouteId =
    match jqf_codec_core::PhysicalRouteId::derive(crate::FORMAT_ID, 1, 1) {
        Some(id) => id,
        None => panic!("nonzero route identity"),
    };

/// Stable physical identity of the jqft LOCATED route — the Exact/Located access slot whose product is the located
/// subtree republished as the document root.
pub const JQFT_LOCATED_PHYSICAL_ROUTE_ID: jqf_codec_core::PhysicalRouteId =
    match jqf_codec_core::PhysicalRouteId::derive(crate::FORMAT_ID, 2, 1) {
        Some(id) => id,
        None => panic!("nonzero route identity"),
    };

/// Stable physical identity of canonical jqft encoding.
pub const JQFT_ENCODE_PHYSICAL_ROUTE_ID: jqf_codec_core::PhysicalRouteId =
    match jqf_codec_core::PhysicalRouteId::derive(crate::FORMAT_ID, 3, 1) {
        Some(id) => id,
        None => panic!("nonzero route identity"),
    };

/// Stable physical identity of the complete jqfjson document route.
pub const JQFJSON_FULL_PHYSICAL_ROUTE_ID: jqf_codec_core::PhysicalRouteId =
    match jqf_codec_core::PhysicalRouteId::derive(crate::JQFJSON_FORMAT_ID, 1, 1) {
        Some(id) => id,
        None => panic!("nonzero route identity"),
    };

/// Stable physical identity of the jqfjson LOCATED route — the Exact/Located access slot whose product is the located
/// subtree republished as the document root.
pub const JQFJSON_LOCATED_PHYSICAL_ROUTE_ID: jqf_codec_core::PhysicalRouteId =
    match jqf_codec_core::PhysicalRouteId::derive(crate::JQFJSON_FORMAT_ID, 2, 1) {
        Some(id) => id,
        None => panic!("nonzero route identity"),
    };

/// Stable physical identity of canonical jqfjson encoding.
pub const JQFJSON_ENCODE_PHYSICAL_ROUTE_ID: jqf_codec_core::PhysicalRouteId =
    match jqf_codec_core::PhysicalRouteId::derive(crate::JQFJSON_FORMAT_ID, 3, 1) {
        Some(id) => id,
        None => panic!("nonzero route identity"),
    };

/// Stable physical identity of the complete jqfb document route.
pub const JQFB_FULL_PHYSICAL_ROUTE_ID: jqf_codec_core::PhysicalRouteId =
    match jqf_codec_core::PhysicalRouteId::derive(crate::FORMAT_ID_JQFB, 1, 1) {
        Some(id) => id,
        None => panic!("nonzero route identity"),
    };

/// Stable physical identity of the jqfb LOCATED route — the Exact/Located access slot whose product is the located
/// subtree as decoded.
pub const JQFB_LOCATED_PHYSICAL_ROUTE_ID: jqf_codec_core::PhysicalRouteId =
    match jqf_codec_core::PhysicalRouteId::derive(crate::FORMAT_ID_JQFB, 2, 1) {
        Some(id) => id,
        None => panic!("nonzero route identity"),
    };

/// Stable physical identity of canonical jqfb encoding.
pub const JQFB_ENCODE_PHYSICAL_ROUTE_ID: jqf_codec_core::PhysicalRouteId =
    match jqf_codec_core::PhysicalRouteId::derive(crate::FORMAT_ID_JQFB, 3, 1) {
        Some(id) => id,
        None => panic!("nonzero route identity"),
    };

/// The CLI-facing routes the jqft registration serves: the whole-document route over a `---`-separated DOCUMENT STREAM
/// (the adjacent-value input model — a jqft source is a document stream, not one text).
const JQFT_ROUTES: [RouteCapability; 1] = [RouteCapability::AdjacentValues];
/// The CLI-facing routes the jqfjson registration serves: the whole-document route plus the ADJACENT-VALUE input model
/// (§4) — a jqfjson SOURCE is one envelope, but a stream of adjacent envelopes is a multi-document stream the way plain
/// json's is, and the native dialect must not be narrower than the format it profiles.
const JQFJSON_ROUTES: [RouteCapability; 1] = [RouteCapability::AdjacentValues];
/// The CLI-facing routes the jqfb registration serves: the EDIT route — the parser binds every node's authored tail
/// span and the encoder supplies the leaf seam and the structural splice policy. One binary document per source: no
/// adjacent-value stream, not a record route. The node-table walk's `Located` demand route is an access route the
/// provider advertises, not a CLI-facing capability.
const JQFB_ROUTES: [RouteCapability; 1] = [RouteCapability::Edit];

/// Constructs the validated jqft codec registration.
pub fn registration_jqft() -> Result<CodecRegistration<'static>, RegistrationError> {
    registration_for(
        &JQFT_DIALECTS,
        FORMAT_ID,
        decode::create_jqft_provider,
        encode::create_jqft_factory,
        &JQFT_ROUTES,
        &["jqft"],
        &[ItemByteOwner::Facade, ItemByteOwner::Facade],
        // The `---` stream is an adjacent-value lane: it scans the same whitespace set between documents.
        VALUE_SEPARATORS,
    )
}

/// Constructs the validated jqfjson codec registration.
pub fn registration_jqfjson() -> Result<CodecRegistration<'static>, RegistrationError> {
    registration_for(
        &JQFJSON_DIALECTS,
        JQFJSON_FORMAT_ID,
        decode::create_jqfjson_provider,
        encode::create_jqfjson_factory,
        &JQFJSON_ROUTES,
        &["jqfjson"],
        &[ItemByteOwner::Facade, ItemByteOwner::Facade],
        // An adjacent-envelope stream scans the same inter-value whitespace.
        VALUE_SEPARATORS,
    )
}

/// The jqfb registration's dialect set.
const JQFB_DIALECTS: [DialectIdRef<'static>; 2] = [
    DialectIdRef::from_static(JQFB_DOCUMENT_DIALECT_ID),
    DialectIdRef::from_static(JQFB_CANONICAL_DIALECT_ID),
];

/// Constructs the validated jqfb codec registration (the machine profile).
pub fn registration_jqfb() -> Result<CodecRegistration<'static>, RegistrationError> {
    registration_for(
        &JQFB_DIALECTS,
        FORMAT_ID_JQFB,
        jqfb_decode::create_jqfb_provider,
        jqfb_encode::create_jqfb_factory,
        &JQFB_ROUTES,
        &["jqfb"],
        // Binary: the facade newline would corrupt the image's footer.
        &[ItemByteOwner::Codec, ItemByteOwner::Codec],
        // Edit-only route: no adjacent-value lane ever scans inter-value whitespace, so the binary envelope declares
        // none.
        &[],
    )
}

#[allow(clippy::too_many_arguments)]
fn registration_for(
    dialects: &'static [DialectIdRef<'static>],
    format_id: &'static str,
    decoder: for<'a, 'b> fn(
        jqf_source::ResolvedSource<'a>,
        jqf_codec_core::DecodeRequest<'b>,
        &mut jqf_resource::ResourceContext<'_>,
    ) -> Result<jqf_codec_core::ErasedProvider<'a>, jqf_codec_core::CodecError>,
    encoder: for<'a, 'b> fn(
        jqf_codec_core::EncodeRequest<'a, 'b>,
        &mut jqf_resource::ResourceContext<'_>,
    ) -> Result<jqf_codec_core::ErasedEncoderFactory, jqf_codec_core::CodecError>,
    routes: &'static [RouteCapability],
    extensions: &'static [&'static str],
    inter_item_byte: &'static [ItemByteOwner],
    value_separators: &'static [u8],
) -> Result<CodecRegistration<'static>, RegistrationError> {
    let format = FormatIdRef::from_static(format_id);
    CodecRegistration::try_new(
        CodecDescriptor::new(
            format,
            dialects,
            CodecOperations::new(true, true, true),
            routes,
            extensions,
            inter_item_byte,
            &[],
            // The jqft whitespace set is byte-identical to JSON's `Ws` set; the drive skips it between `---`-separated
            // documents. The jqfb sibling passes empty: its edit-only route never scans inter-value whitespace.
            value_separators,
        ),
        Some(DecoderFactoryRecord::new(decoder)),
        Some(EncoderFactoryRecord::new(encoder)),
        Some(TagValidatorFactoryRecord::new(tag_validator_factory)),
        None,
    )
}

/// The v1 tag-validator dispatch answer. jqft's tags are first-class GRAMMAR — decode retains them from the node table
/// and the encoder emits the `TagId` text verbatim — so no tag ever routes through the validator channel; the
/// registration records the no-tags validator, which accepts exactly the empty set.
fn tag_validator_factory<'a>(
    _request: jqf_codec_core::EncodeRequest<'a, 'a>,
    _resources: &mut jqf_resource::ResourceContext<'_>,
) -> Result<jqf_codec_core::ErasedTagValidator, jqf_codec_core::CodecError> {
    ErasedTagValidator::try_new_validator(|| Ok(jqf_codec_core::NoTagsValidator))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registrations_are_valid_and_serve_their_dialect() {
        for (registration, format, dialect) in [
            (registration_jqft().expect("jqft"), FORMAT_ID, JQFT_DOCUMENT_DIALECT_ID),
            (
                registration_jqfjson().expect("jqfjson"),
                JQFJSON_FORMAT_ID,
                JQFJSON_DOCUMENT_DIALECT_ID,
            ),
            (
                registration_jqfb().expect("jqfb"),
                FORMAT_ID_JQFB,
                JQFB_DOCUMENT_DIALECT_ID,
            ),
        ] {
            let descriptor = registration.descriptor();
            assert_eq!(descriptor.format().as_str(), format);
            assert_eq!(descriptor.operations(), CodecOperations::new(true, true, true));
            assert!(registration.decoder().is_some());
            assert!(registration.encoder().is_some());
            assert!(registration.tag_validator().is_some());
            let ids: alloc::vec::Vec<&str> = descriptor.dialects().iter().map(|d| d.as_str()).collect();
            assert!(ids.contains(&dialect), "registration for {dialect} must list it");
        }
    }

    #[test]
    fn registrations_keep_disjoint_dialect_sets() {
        let jqft: alloc::vec::Vec<&str> = registration_jqft()
            .expect("jqft")
            .descriptor()
            .dialects()
            .iter()
            .map(|d| d.as_str())
            .collect();
        let jqfjson: alloc::vec::Vec<&str> = registration_jqfjson()
            .expect("jqfjson")
            .descriptor()
            .dialects()
            .iter()
            .map(|d| d.as_str())
            .collect();
        let jqfb_dialects: alloc::vec::Vec<&str> = registration_jqfb()
            .expect("jqfb")
            .descriptor()
            .dialects()
            .iter()
            .map(|d| d.as_str())
            .collect();
        for (name, item) in [("jqft", &jqft), ("jqfjson", &jqfjson), ("jqfb", &jqfb_dialects)] {
            for other in [&jqft, &jqfjson, &jqfb_dialects] {
                if core::ptr::eq(item, other) {
                    continue;
                }
                for id in other {
                    assert!(
                        !item.contains(id),
                        "dialect {id} of {name} appears in another registration"
                    );
                }
            }
        }
    }

    #[test]
    fn physical_route_ids_are_stable() {
        // The identities are DERIVED from (format, kind, specialization), so the stability pin is the derivation
        // itself: the constant must equal what the triple derives.
        let derive = |kind, spec| jqf_codec_core::PhysicalRouteId::derive("jqft", kind, spec).expect("derived");
        assert_eq!(JQFT_FULL_PHYSICAL_ROUTE_ID, derive(1, 1));
        assert_eq!(JQFT_LOCATED_PHYSICAL_ROUTE_ID, derive(2, 1));
        assert_eq!(JQFT_ENCODE_PHYSICAL_ROUTE_ID, derive(3, 1));
        let derive = |kind, spec| jqf_codec_core::PhysicalRouteId::derive("jqfjson", kind, spec).expect("derived");
        assert_eq!(JQFJSON_FULL_PHYSICAL_ROUTE_ID, derive(1, 1));
        assert_eq!(JQFJSON_LOCATED_PHYSICAL_ROUTE_ID, derive(2, 1));
        assert_eq!(JQFJSON_ENCODE_PHYSICAL_ROUTE_ID, derive(3, 1));
        let derive = |kind, spec| jqf_codec_core::PhysicalRouteId::derive("jqfb", kind, spec).expect("derived");
        assert_eq!(JQFB_FULL_PHYSICAL_ROUTE_ID, derive(1, 1));
        assert_eq!(JQFB_LOCATED_PHYSICAL_ROUTE_ID, derive(2, 1));
        assert_eq!(JQFB_ENCODE_PHYSICAL_ROUTE_ID, derive(3, 1));
    }
}
