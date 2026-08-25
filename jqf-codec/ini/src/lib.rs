//! The flat-config codec: `.properties`, INI, and dotenv.
//!
//! Three format ids, one crate, because the grammars disagree: `;` is a comment in INI and a value byte in
//! `.properties`; `key value` is a pair only in `.properties`; `export X=1` is a key named `export X` unless the
//! dialect is dotenv.
//!
//! The clause lists, value model, encode preflight, and edit splice live in `CONTRACTS.md`. Three registrations:
//! `properties` (extension `properties`), `ini` (extensions `ini`, `cfg` — not `conf`), and `dotenv` (filenames `.env`
//! and `.env.*`).

#![no_std]
#![deny(missing_docs)]
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]
#![allow(
    clippy::missing_errors_doc,
    reason = "fallible APIs use the codec crate's closed structured error vocabulary"
)]

extern crate alloc;

mod decode;
mod encode;
mod materialize;
mod options;
mod provider;
mod scan;
mod tag;

/// Compiles the README examples as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;

use jqf_codec_core::{
    CodecDescriptor, CodecOperations, CodecRegistration, DecoderFactoryRecord, EncoderFactoryRecord, ItemByteOwner,
    RegistrationError, RouteCapability, TagValidatorFactoryRecord,
};
use jqf_data::{DialectIdRef, FormatIdRef};

pub use options::{
    DOTENV_FORMAT_ID, DOTENV_JQF_1_0_DIALECT_ID, DOTENV_JQF_STRICT_DIALECT_ID, FORMAT_ID, INI_FORMAT_ID,
    INI_JQF_1_0_DIALECT_ID, INI_JQF_STRICT_DIALECT_ID, PROPERTIES_JDK_DIALECT_ID, PROPERTIES_JQF_1_0_DIALECT_ID,
};

/// The CLI-facing routes the flat-config registrations serve: the edit lane — every grammar binds retained source spans
/// and supplies the edit-render dialect and written splice policy. Flat config is not a record route and not an
/// adjacent-value format (one document per source).
const ROUTES: [RouteCapability; 1] = [RouteCapability::Edit];

/// The properties dialect pair (input + output profile).
const PROPERTIES_DIALECTS: [DialectIdRef<'static>; 2] = [
    DialectIdRef::from_static(PROPERTIES_JDK_DIALECT_ID),
    DialectIdRef::from_static(PROPERTIES_JQF_1_0_DIALECT_ID),
];

/// The ini dialect pair (input + output profile).
const INI_DIALECTS: [DialectIdRef<'static>; 2] = [
    DialectIdRef::from_static(INI_JQF_STRICT_DIALECT_ID),
    DialectIdRef::from_static(INI_JQF_1_0_DIALECT_ID),
];

/// The dotenv dialect pair (input + output profile).
const DOTENV_DIALECTS: [DialectIdRef<'static>; 2] = [
    DialectIdRef::from_static(DOTENV_JQF_STRICT_DIALECT_ID),
    DialectIdRef::from_static(DOTENV_JQF_1_0_DIALECT_ID),
];

/// Stable physical identity of whole-document decoding.
///
/// # Panics
///
/// Panics when the route identity cannot be derived (the identity is nonzero by construction for these fixed inputs).
#[must_use]
pub(crate) const fn decode_route_id(grammar: options::Grammar) -> jqf_codec_core::PhysicalRouteId {
    match jqf_codec_core::PhysicalRouteId::derive(grammar.format_id(), 1, 1) {
        Some(id) => id,
        None => panic!("nonzero route identity"),
    }
}

/// Stable physical identity of deterministic semantic encoding.
///
/// # Panics
///
/// Panics when the route identity cannot be derived (the identity is nonzero by construction for these fixed inputs).
#[must_use]
pub(crate) const fn encode_route_id(grammar: options::Grammar) -> jqf_codec_core::PhysicalRouteId {
    match jqf_codec_core::PhysicalRouteId::derive(grammar.format_id(), 2, 1) {
        Some(id) => id,
        None => panic!("nonzero route identity"),
    }
}

fn registration_for(
    grammar: options::Grammar,
    extensions: &'static [&'static str],
    filenames: &'static [&'static str],
) -> Result<CodecRegistration<'static>, RegistrationError> {
    let dialects: &'static [DialectIdRef<'static>] = match grammar {
        options::Grammar::Properties => &PROPERTIES_DIALECTS,
        options::Grammar::Ini => &INI_DIALECTS,
        options::Grammar::Dotenv => &DOTENV_DIALECTS,
    };
    let decoder = match grammar {
        options::Grammar::Properties => decode::create_properties_provider,
        options::Grammar::Ini => decode::create_ini_provider,
        options::Grammar::Dotenv => decode::create_dotenv_provider,
    };
    CodecRegistration::try_new(
        CodecDescriptor::new(
            FormatIdRef::from_static(grammar.format_id()),
            dialects,
            CodecOperations::new(true, true, true),
            &ROUTES,
            extensions,
            // Input dialect (jdk/ini-strict/dotenv-strict) retains its edit document's trailing byte; the output
            // profile has the facade supply the item newline.
            &[ItemByteOwner::Codec, ItemByteOwner::Facade],
            filenames,
            // No insignificant inter-value bytes: every byte reaches the decoder.
            &[],
        ),
        Some(DecoderFactoryRecord::new(decoder)),
        Some(EncoderFactoryRecord::new(encode::create_factory)),
        Some(TagValidatorFactoryRecord::new(tag::create_validator)),
        None,
    )
}

/// Registers the `properties` format (the first dialect): the `.properties` extension and the `properties.jdk@1` /
/// `properties.jqf-1.0@1` dialect pair.
pub fn registration() -> Result<CodecRegistration<'static>, RegistrationError> {
    registration_for(options::Grammar::Properties, &["properties"], &[])
}

/// Registers the `ini` format: the `ini` and `cfg` extensions and the `ini.jqf-strict@1` / `ini.jqf-1.0@1` dialect
/// pair. `conf` is deliberately NOT claimed — nginx/httpd/ssh `.conf` files are not this grammar, and a wrong detection
/// is worse than none.
pub fn registration_ini() -> Result<CodecRegistration<'static>, RegistrationError> {
    registration_for(options::Grammar::Ini, &["ini", "cfg"], &[])
}

/// Registers the `dotenv` format: no extension — `.env` is claimed by the exact-filename registration fact, with the
/// glob `.env.*` covering `.env.local`.
pub fn registration_dotenv() -> Result<CodecRegistration<'static>, RegistrationError> {
    registration_for(options::Grammar::Dotenv, &[], &[".env", ".env.*"])
}

#[cfg(test)]
mod tests {
    use super::{CodecOperations, RegistrationError};

    #[test]
    fn all_three_registrations_are_valid_and_serve_their_dialects() {
        let properties = super::registration().expect("properties");
        let descriptor = properties.descriptor();
        assert_eq!(descriptor.format().as_str(), super::FORMAT_ID);
        assert_eq!(
            descriptor
                .dialects()
                .iter()
                .map(|d| d.as_str())
                .collect::<alloc::vec::Vec<_>>(),
            [super::PROPERTIES_JDK_DIALECT_ID, super::PROPERTIES_JQF_1_0_DIALECT_ID]
        );
        assert_eq!(descriptor.operations(), CodecOperations::new(true, true, true));
        assert_eq!(descriptor.extensions(), ["properties"]);
        assert!(properties.decoder().is_some());
        assert!(properties.encoder().is_some());
        assert!(properties.tag_validator().is_some());

        let ini = super::registration_ini().expect("ini");
        assert_eq!(ini.descriptor().format().as_str(), super::INI_FORMAT_ID);
        assert_eq!(
            ini.descriptor()
                .dialects()
                .iter()
                .map(|d| d.as_str())
                .collect::<alloc::vec::Vec<_>>(),
            [super::INI_JQF_STRICT_DIALECT_ID, super::INI_JQF_1_0_DIALECT_ID]
        );
        assert_eq!(ini.descriptor().extensions(), ["ini", "cfg"]);
        assert_eq!(ini.descriptor().filenames(), [] as [&str; 0]);

        let dotenv = super::registration_dotenv().expect("dotenv");
        assert_eq!(dotenv.descriptor().format().as_str(), super::DOTENV_FORMAT_ID);
        assert_eq!(
            dotenv
                .descriptor()
                .dialects()
                .iter()
                .map(|d| d.as_str())
                .collect::<alloc::vec::Vec<_>>(),
            [super::DOTENV_JQF_STRICT_DIALECT_ID, super::DOTENV_JQF_1_0_DIALECT_ID]
        );
        assert_eq!(dotenv.descriptor().extensions(), [] as [&str; 0]);
        assert_eq!(dotenv.descriptor().filenames(), [".env", ".env.*"]);
    }

    #[test]
    fn registrations_keep_disjoint_dialect_sets() {
        let a = super::registration().expect("properties");
        let b = super::registration_ini().expect("ini");
        let c = super::registration_dotenv().expect("dotenv");
        let names: alloc::vec::Vec<&str> = [a, b, c]
            .iter()
            .flat_map(|r| r.descriptor().dialects().iter().map(|d| d.as_str()))
            .collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        let mut deduped = sorted.clone();
        deduped.dedup();
        assert_eq!(sorted, deduped, "a dialect appears in two registrations");
    }

    #[test]
    fn a_bad_registration_is_rejected() {
        let dialects = [
            jqf_data::DialectIdRef::from_static(super::PROPERTIES_JDK_DIALECT_ID),
            jqf_data::DialectIdRef::from_static(super::PROPERTIES_JDK_DIALECT_ID),
        ];
        let result = jqf_codec_core::CodecRegistration::try_new(
            jqf_codec_core::CodecDescriptor::new(
                jqf_data::FormatIdRef::from_static(super::FORMAT_ID),
                &dialects,
                CodecOperations::new(true, true, true),
                &[],
                &[],
                // Duplicate dialects make the framing arity irrelevant; the duplicate check fires first.
                &[],
                &[],
                &[],
            ),
            None,
            None,
            None,
            None,
        );
        assert_eq!(result.err(), Some(RegistrationError::DuplicateDialect));
    }
}
