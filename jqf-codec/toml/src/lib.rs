//! TOML 1.0 / 1.1 decode, located access, and semantic encode.
//!
//! [`registration_1_0`] and [`registration_1_1`] are the catalog entries. Slot 0 is the whole document; slot 1 is an
//! exact-path walk. Encode is deterministic and topology-normalizing. Sibling: [`jqf_codec_core`].

#![no_std]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
#![allow(
    clippy::missing_errors_doc,
    reason = "fallible APIs use closed structured codec errors"
)]

extern crate alloc;

#[cfg(test)]
mod test_support;

mod decode;
mod encode;
mod error;
mod grammar;
mod lazy;
mod locate;
mod materialize;
mod parse;
mod provider;
mod scoped;
mod tag;
mod walk;

use jqf_codec_core::{
    CodecDescriptor, CodecError, CodecOperations, CodecRegistration, DecodeRequest, DecoderFactoryRecord,
    EncoderFactoryRecord, ErasedProvider, ItemByteOwner, RegistrationError, RouteCapability, TagValidatorFactoryRecord,
};
use jqf_data::{DialectIdRef, FormatIdRef};
use jqf_resource::ResourceContext;
use jqf_source::ResolvedSource;

/// Stable TOML format identity text.
pub const FORMAT_ID: &str = "toml";
/// Stable TOML 1.0 dialect identity text.
pub const TOML_1_0_DIALECT_ID: &str = "toml-1.0";
/// Stable TOML 1.1 dialect identity text.
pub const TOML_1_1_DIALECT_ID: &str = "toml-1.1";
/// Stable TOML 1.0 output-profile identity text.
pub const TOML_JQF_1_0_DIALECT_ID: &str = "toml.jqf-1.0@1";
/// Stable TOML 1.1 output-profile identity text.
pub const TOML_JQF_1_1_DIALECT_ID: &str = "toml.jqf-1.1@1";

/// Input dialect plus output profile for [`registration_1_0`].
const DIALECTS_1_0: [DialectIdRef<'static>; 2] = [
    DialectIdRef::from_static(TOML_1_0_DIALECT_ID),
    DialectIdRef::from_static(TOML_JQF_1_0_DIALECT_ID),
];

/// The TOML 1.1 registration's dialect set.
const DIALECTS_1_1: [DialectIdRef<'static>; 2] = [
    DialectIdRef::from_static(TOML_1_1_DIALECT_ID),
    DialectIdRef::from_static(TOML_JQF_1_1_DIALECT_ID),
];

/// Stable physical identity of the complete TOML document route.
pub const FULL_PHYSICAL_ROUTE_ID: jqf_codec_core::PhysicalRouteId =
    match jqf_codec_core::PhysicalRouteId::derive(crate::FORMAT_ID, 1, 1) {
        Some(id) => id,
        None => panic!("nonzero route identity"),
    };

/// Stable physical identity of the native TOML scoped (exact-path) route.
pub const SCOPED_PHYSICAL_ROUTE_ID: jqf_codec_core::PhysicalRouteId =
    match jqf_codec_core::PhysicalRouteId::derive(crate::FORMAT_ID, 2, 1) {
        Some(id) => id,
        None => panic!("nonzero route identity"),
    };

/// Stable physical identity of deterministic semantic TOML encoding.
pub const ENCODE_PHYSICAL_ROUTE_ID: jqf_codec_core::PhysicalRouteId =
    match jqf_codec_core::PhysicalRouteId::derive(crate::FORMAT_ID, 3, 1) {
        Some(id) => id,
        None => panic!("nonzero route identity"),
    };

/// Constructs the allocation-free validated TOML 1.0 codec registration.
///
/// One registration per (input dialect, output profile) pair, because the catalog matches BOTH the decoder and the
/// encoder against the SAME descriptor dialect list and hands the decoder factory no dialect. The two registrations
/// keep disjoint dialect sets, so catalog selection is never ambiguous and each decoder factory hardcodes the dialect
/// it serves.
pub fn registration_1_0() -> Result<CodecRegistration<'static>, RegistrationError> {
    registration_for(
        &DIALECTS_1_0,
        decode::create_provider_1_0,
        &["toml"],
        // Input dialect (toml-1.0) retains its edit document's trailing byte; the output profile has the facade supply
        // the item newline.
        &[ItemByteOwner::Codec, ItemByteOwner::Facade],
    )
}

/// Constructs the allocation-free validated TOML 1.1 codec registration.
pub fn registration_1_1() -> Result<CodecRegistration<'static>, RegistrationError> {
    registration_for(
        &DIALECTS_1_1,
        decode::create_provider_1_1,
        // The 1.0 registration is the default input dialect, so it owns the `toml` extension; the 1.1 registration
        // declares none.
        &[],
        &[ItemByteOwner::Codec, ItemByteOwner::Facade],
    )
}

/// The CLI-facing routes the TOML registrations serve: the edit lane (the codec binds retained source spans and
/// supplies the edit-render dialect and splice policy). TOML is not a record route and not an adjacent-value format
/// (one document per source), so the two input-model facts are absent.
const ROUTES: [RouteCapability; 1] = [RouteCapability::Edit];

fn registration_for(
    dialects: &'static [DialectIdRef<'static>],
    decoder: for<'source, 'options, 'control> fn(
        ResolvedSource<'source>,
        DecodeRequest<'options>,
        &mut ResourceContext<'control>,
    ) -> Result<ErasedProvider<'source>, CodecError>,
    extensions: &'static [&'static str],
    inter_item_byte: &'static [ItemByteOwner],
) -> Result<CodecRegistration<'static>, RegistrationError> {
    let format = FormatIdRef::from_static(FORMAT_ID);
    CodecRegistration::try_new(
        CodecDescriptor::new(
            format,
            dialects,
            CodecOperations::new(true, true, true),
            &ROUTES,
            extensions,
            inter_item_byte,
            &[],
            // No insignificant inter-value bytes: every byte reaches the decoder.
            &[],
        ),
        Some(DecoderFactoryRecord::new(decoder)),
        Some(EncoderFactoryRecord::new(encode::create_factory)),
        Some(TagValidatorFactoryRecord::new(tag::create_validator)),
        None,
    )
}

/// Compiles the README examples as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;

#[cfg(test)]
pub(crate) mod tests {
    use super::{CodecOperations, RegistrationError};
    use jqf_codec_core::{CodecDescriptor, CodecRegistration};

    #[test]
    fn both_registrations_are_valid_and_serve_their_dialect() {
        let r10 = super::registration_1_0().expect("valid 1.0 registration");
        let descriptor = r10.descriptor();
        assert_eq!(descriptor.format().as_str(), super::FORMAT_ID);
        assert_eq!(descriptor.dialects().len(), 2);
        assert_eq!(descriptor.operations(), CodecOperations::new(true, true, true));
        assert!(r10.decoder().is_some());
        assert!(r10.encoder().is_some());
        assert!(r10.tag_validator().is_some());

        let r11 = super::registration_1_1().expect("valid 1.1 registration");
        let dialects: alloc::vec::Vec<&str> = r11.descriptor().dialects().iter().map(|d| d.as_str()).collect();
        assert_eq!(dialects, ["toml-1.1", "toml.jqf-1.1@1"]);
    }

    #[test]
    fn registrations_keep_disjoint_dialect_sets() {
        let r10 = super::registration_1_0().expect("1.0");
        let r11 = super::registration_1_1().expect("1.1");
        let a: alloc::vec::Vec<&str> = r10.descriptor().dialects().iter().map(|d| d.as_str()).collect();
        let b: alloc::vec::Vec<&str> = r11.descriptor().dialects().iter().map(|d| d.as_str()).collect();
        for left in a {
            assert!(!b.contains(&left), "dialect {left} appears in both registrations");
        }
    }

    #[test]
    fn duplicate_dialects_are_rejected() {
        let dialects = [
            jqf_data::DialectIdRef::from_static(super::TOML_1_0_DIALECT_ID),
            jqf_data::DialectIdRef::from_static(super::TOML_1_0_DIALECT_ID),
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

    /// Materializes the whole document via the TREE route (`grammar::parse` + the shared located builder over the root
    /// table) — the route the parse-DIRECT whole route replaced. The parity tests below compare its bytes against the
    /// direct route's.
    pub(crate) fn internal_tree_route_materialize(bytes: &[u8]) -> Result<jqf_data::Value, jqf_codec_core::CodecError> {
        let mut resources = jqf_resource::ResourceContext::new(
            jqf_resource::RequestAccount::try_new(jqf_resource::ResourceLimits::new(
                u64::MAX,
                u64::MAX,
                u64::MAX,
                u64::MAX,
                u32::MAX,
            ))
            .expect("account"),
            &jqf_resource::ContinueControl,
            jqf_resource::WorkMeter::try_new_v1(4_096).expect("work"),
        )
        .expect("resources");
        let source = jqf_source::ResolvedSource::new(
            jqf_source::SourceRef::new(jqf_source::SourceId::new(99), jqf_source::SourceKind::Input),
            "tree-route.toml",
            bytes,
            0,
        );
        let parsed = crate::grammar::parse(source, crate::provider::DialectKind::Toml10, &mut resources)?;
        let located = crate::locate::locate(&parsed.root, &[])?;
        let (builder, root) = crate::materialize::build_located_document(&located, &parsed.names, bytes, &resources)?;
        let document = builder.finish(root, &resources).map_err(crate::parse::map_data)?;
        document
            .materialize_root(&mut resources)
            .map_err(crate::parse::map_data)
    }

    /// Decodes via the parse-DIRECT whole route and encodes, the parity witness's direct arm.
    fn direct_route_encode(bytes: &[u8]) -> Result<alloc::vec::Vec<u8>, jqf_codec_core::CodecError> {
        use jqf_codec_core::{CodecRunContext, DecodeRequest, DiagnosticPolicy, ValidationMode};
        use jqf_data::DialectId;
        let mut resources = super::test_support::resources();
        let registration = super::registration_1_0().expect("registration");
        let mut provider = registration.decoder().expect("decoder").create_provider(
            source(bytes),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(crate::TOML_JQF_1_0_DIALECT_ID).expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            &mut resources,
        )?;
        let requirement = whole_requirement(&resources);
        let handle = provider.bind(&requirement).expect("bind");
        let mut session = provider.open(&handle, &mut resources)?;
        let result = {
            let mut run = CodecRunContext::new(&mut resources);
            run.set_cooperative_credits(4_096);
            session.decode(&mut run)?
        };
        let jqf_codec_core::AccessOutcome::FullDocument(product) = result.outcome() else {
            panic!("expected full document");
        };
        let value = product.document().materialize_root(&mut resources).map_err(|_| {
            jqf_codec_core::CodecError::new(jqf_codec_core::CodecFailureKind::InternalContractViolation {
                contract: "materialize direct root",
            })
        })?;
        encode(&value, &mut resources)
    }

    fn source(bytes: &[u8]) -> jqf_source::ResolvedSource<'_> {
        jqf_source::ResolvedSource::new(
            jqf_source::SourceRef::new(jqf_source::SourceId::new(92), jqf_source::SourceKind::Input),
            "test.toml",
            bytes,
            0,
        )
    }

    fn whole_requirement(resources: &jqf_resource::ResourceContext<'_>) -> jqf_codec_core::AccessRequirement {
        use jqf_codec_core::{AccessGuarantees, AccessRequirement, CodecDemand, DemandClause, DiagnosticPolicy};
        let mut demand = CodecDemand::try_new(resources);
        demand.try_insert(&DemandClause::SemanticRoot).expect("semantic root");
        demand.try_insert(&DemandClause::ValueShape).expect("value shape");
        AccessRequirement::try_whole(
            demand,
            AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            resources,
        )
        .expect("requirement")
    }

    fn encode(
        value: &jqf_data::Value,
        resources: &mut jqf_resource::ResourceContext<'_>,
    ) -> Result<alloc::vec::Vec<u8>, jqf_codec_core::CodecError> {
        use jqf_codec_core::{EncodeRequest, PreservationRequest};
        use jqf_data::{DialectId, FormatId};
        let format = FormatId::try_new(super::FORMAT_ID).expect("format");
        let dialect = DialectId::try_new(super::TOML_JQF_1_0_DIALECT_ID).expect("dialect");
        let registration = super::registration_1_0().expect("registration");
        let factory = registration.encoder().expect("encoder").create_factory(
            EncodeRequest {
                format: &format,
                dialect: &dialect,
                diagnostics: jqf_codec_core::DiagnosticPolicy::ErrorsOnly,
                preservation: PreservationRequest::None,
                options: None,
            },
            resources,
        )?;
        let mut session = factory
            .start(
                jqf_codec_core::EncodeItem::Owned(value),
                jqf_codec_core::PreservationRequest::None,
                resources,
            )
            .expect("session");
        let mut out = alloc::vec::Vec::new();
        {
            let mut sink = jqf_codec_core::VecByteSink::new(&mut out);
            let mut run = jqf_codec_core::CodecRunContext::new(resources);
            run.set_cooperative_credits(4_096);
            session.encode(&mut sink, &mut run)?;
        }
        Ok(out)
    }

    #[test]
    fn direct_route_publishes_byte_identical_documents() {
        let fixtures = [
            "a = 1\nb = \"x\"\n",
            "[t]\nx = 2\n",
            "a.b = 1\n[a.c]\ny = 2\n",
            "[[p]]\nname = \"Hammer\"\n[[p]]\nname = \"Nail\"\n",
            "[[p]]\nx = 1\n[b]\ny = 2\n[p.extra]\nz = 3\n",
            "point = { x = 1, y = 2 }\nlist = [1, 2, 3]\n",
            "[a]\n[a.b]\n[a.b.c]\nleaf = \"deep\"\n",
            "d1 = 1979-05-27\nt1 = 07:32:00\n",
            "[a]\nx = 1\na.y = 2\n",
        ];
        for fixture in fixtures {
            let direct = direct_route_encode(fixture.as_bytes()).expect("direct");
            let tree = internal_tree_route_materialize(fixture.as_bytes()).expect("tree");
            let mut res = super::test_support::resources();
            let tree_bytes = encode(&tree, &mut res).expect("tree encode");
            assert_eq!(direct, tree_bytes, "direct vs tree encode drifted for {fixture:?}");
        }
    }

    #[test]
    fn direct_route_publishes_byte_identical_documents_on_large_shapes() {
        use alloc::fmt::Write as _;
        let mut fixture = alloc::string::String::new();
        for index in 0..2000u32 {
            fixture.push_str("[[catalog]]\n");
            writeln!(fixture, "id = {index}").expect("write");
            writeln!(fixture, "name = \"item-{index:06}\" ").expect("write");
            if index % 3 == 0 {
                fixture.push_str("[catalog.meta]\n");
                writeln!(fixture, "tag = \"t{index}\" ").expect("write");
            }
            if index % 5 == 0 {
                writeln!(fixture, "catalog.extra.v = {index} ").expect("write");
            }
        }
        let direct = direct_route_encode(fixture.as_bytes()).expect("direct");
        let tree = internal_tree_route_materialize(fixture.as_bytes()).expect("tree");
        let mut res = super::test_support::resources();
        let tree_bytes = encode(&tree, &mut res).expect("tree encode");
        assert_eq!(direct.len(), tree_bytes.len(), "encode length drifted");
        assert_eq!(direct, tree_bytes, "direct vs tree encode drifted");
    }

    #[test]
    fn physical_route_ids_are_stable() {
        // Literal pins for the packing law: format id `toml` fills the high bytes (`74 6F 6D 6C 00 00`), the kind byte
        // and specialization byte pack low. A change to `PhysicalRouteId::derive`, to FORMAT_ID, or to any route's
        // (kind, spec) triple moves a pinned receipt identity, and this test is where that shows.
        assert_eq!(
            super::FULL_PHYSICAL_ROUTE_ID.get(),
            0x746F_6D6C_0000_0101,
            "FULL route identity drifted from its pinned value"
        );
        assert_eq!(
            super::SCOPED_PHYSICAL_ROUTE_ID.get(),
            0x746F_6D6C_0000_0201,
            "SCOPED route identity drifted from its pinned value"
        );
        assert_eq!(
            super::ENCODE_PHYSICAL_ROUTE_ID.get(),
            0x746F_6D6C_0000_0301,
            "ENCODE route identity drifted from its pinned value"
        );
    }
}
