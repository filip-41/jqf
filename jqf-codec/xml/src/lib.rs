//! XML 1.0 decoder registration and access provider.
//!
//! This crate implements the XML vertical under the §4.9 law. The parser is a secure
//! non-validating XML 1.0 processor with a namespace stack (namespaces,
//! attributes, mixed content, entities, declarations, comments, processing
//! instructions, CDATA) producing a private element [`Tree`]. The
//! whole-document route projects that tree into the format-neutral
//! [`jqf_data::Document`].

#![cfg_attr(not(test), no_std)]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
#![allow(
    clippy::missing_errors_doc,
    reason = "fallible APIs use closed structured codec errors"
)]
#![allow(
    clippy::too_many_lines,
    clippy::result_large_err,
    clippy::unnecessary_wraps,
    clippy::if_not_else,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::struct_excessive_bools,
    reason = "state-machine functions mirror the reference algorithm's shape; errors carry accounted diagnostics; the signed-index law is inherently i64, and container lengths are bounded by the u32::MAX source ceiling; the parser's retention flags are a compact state word"
)]

extern crate alloc;

mod decode;
mod document;
mod encode;
mod lazy;
mod locate;
mod options;
mod parse;
mod provider;
mod scoped;
mod session;
mod value;

/// Compiles the README examples as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;

use jqf_codec_core::{
    AccessInput, AccessOutcome, AccessSession, CodecDescriptor, CodecOperations, CodecRegistration,
    DecoderFactoryRecord, EncoderFactoryRecord, ItemByteOwner, RegistrationError, RouteCapability,
};
use jqf_data::{DialectIdRef, FormatIdRef};

/// Stable XML format identity text.
pub const FORMAT_ID: &str = "xml";
/// The predeclared `xml` namespace URI, bound at parse start per Namespaces in
/// XML.
pub(crate) const XML_NAMESPACE: &str = "http://www.w3.org/XML/1998/namespace";

/// Stable XML input dialect identity text.
pub const XML_DOCUMENT_DIALECT_ID: &str = "xml.document@1";
/// Stable source-echo output-profile identity text.
pub const XML_SOURCE_DIALECT_ID: &str = "xml.source@1";
/// Stable deterministic rewrite output-profile identity text.
pub const XML_DETERMINISTIC_DIALECT_ID: &str = "xml.jqf-deterministic@1";

/// The registration's dialect set: the input dialect and the two output
/// profiles.
const DIALECTS: [DialectIdRef<'static>; 3] = [
    DialectIdRef::from_static(XML_DOCUMENT_DIALECT_ID),
    DialectIdRef::from_static(XML_SOURCE_DIALECT_ID),
    DialectIdRef::from_static(XML_DETERMINISTIC_DIALECT_ID),
];

/// Stable physical identity of the complete XML document route.
pub(crate) const FULL_PHYSICAL_ROUTE_ID: jqf_codec_core::PhysicalRouteId =
    match jqf_codec_core::PhysicalRouteId::derive(crate::FORMAT_ID, 1, 1) {
        Some(id) => id,
        None => panic!("nonzero route identity"),
    };

/// Stable physical identity of XML encoding.
pub(crate) const ENCODE_PHYSICAL_ROUTE_ID: jqf_codec_core::PhysicalRouteId =
    match jqf_codec_core::PhysicalRouteId::derive(crate::FORMAT_ID, 3, 1) {
        Some(id) => id,
        None => panic!("nonzero route identity"),
    };

/// Stable physical identity of the XML scoped (Located) route.
pub(crate) const SCOPED_PHYSICAL_ROUTE_ID: jqf_codec_core::PhysicalRouteId =
    match jqf_codec_core::PhysicalRouteId::derive(crate::FORMAT_ID, 2, 1) {
        Some(id) => id,
        None => panic!("nonzero route identity"),
    };

/// The CLI-facing routes the XML registration serves: the source-preserving
/// edit lane (per-node spans bind text leaves, attribute values,
/// and element extents, and the three hooks splice the bytes, so `--edit`
/// over XML is served by declaration) and the record route's output side
/// (`RouteCapability::Record`: an NDJSON/json-seq/CSV/TSV stream publishing
/// one synthesized `<root>` document per record). Not adjacent-values: one
/// XML document per source, and the access routes the provider advertises
/// are not CLI-facing capabilities.
const ROUTES: [RouteCapability; 2] = [RouteCapability::Edit, RouteCapability::Record];

/// Constructs the allocation-free validated XML codec registration.
pub fn registration() -> Result<CodecRegistration<'static>, RegistrationError> {
    let format = FormatIdRef::from_static(FORMAT_ID);
    CodecRegistration::try_new(
        CodecDescriptor::new(
            format,
            &DIALECTS,
            CodecOperations::new(true, true, false),
            &ROUTES,
            &["xml"],
            // Input dialect (document) retains its edit document's trailing
            // byte; output profiles (source/deterministic) have the facade
            // supply the item newline.
            &[ItemByteOwner::Codec, ItemByteOwner::Facade, ItemByteOwner::Facade],
            &[],
            // No insignificant inter-value bytes: every byte reaches the decoder.
            &[],
        ),
        Some(DecoderFactoryRecord::new(decode::create_provider)),
        Some(EncoderFactoryRecord::new(encode::create_factory)),
        None,
        None,
    )
}

/// Decodes one XML document, returning the owned root [`jqf_data::Value`].
///
/// This is the codec-level product, used by the conformance smoke and
/// available to SDK callers that need the whole document at once. It drives
/// the whole-document session directly and materializes the root.
///
/// # Errors
///
/// Returns a structured [`jqf_codec_core::CodecError`] when the document
/// cannot be decoded to its semantic value.
pub fn decode_document(
    source: jqf_source::ResolvedSource<'_>,
    resources: &mut jqf_resource::ResourceContext<'_>,
) -> Result<jqf_data::Value, jqf_codec_core::CodecError> {
    let mut state = session::XmlSession::new(source, false, true, jqf_data::BuilderCoverage::complete())?;
    let mut context = jqf_codec_core::CodecRunContext::new(resources);
    let result = state.decode(AccessInput::Source(source), &mut context)?;
    let AccessOutcome::FullDocument(product) = result.outcome() else {
        return Err(jqf_codec_core::CodecError::new(
            jqf_codec_core::CodecFailureKind::RequirementMismatch,
        ));
    };
    product
        .document()
        .materialize_root(resources)
        .map_err(document::map_data)
}

#[cfg(test)]
mod tests {
    use super::{CodecOperations, RegistrationError};
    use jqf_codec_core::{CodecDescriptor, CodecRegistration};

    #[test]
    fn the_registration_dialect_set_has_no_duplicates() {
        let mut seen: alloc::vec::Vec<&str> = alloc::vec::Vec::new();
        for dialect in super::DIALECTS.iter().map(|d| d.as_str()) {
            assert!(
                !seen.contains(&dialect),
                "dialect {dialect} appears twice in the XML set"
            );
            seen.push(dialect);
        }
    }

    #[test]
    fn physical_route_ids_are_stable_and_distinct() {
        let ids = [
            super::FULL_PHYSICAL_ROUTE_ID.get(),
            super::ENCODE_PHYSICAL_ROUTE_ID.get(),
            super::SCOPED_PHYSICAL_ROUTE_ID.get(),
        ];
        // The identities are DERIVED from (format, kind, specialization)
        // so the stability pin is the derivation itself: the
        // constant must equal what the triple derives.
        let derive = |kind, spec| jqf_codec_core::PhysicalRouteId::derive("xml", kind, spec).expect("derived");
        assert_eq!(super::FULL_PHYSICAL_ROUTE_ID, derive(1, 1));
        assert_eq!(super::ENCODE_PHYSICAL_ROUTE_ID, derive(3, 1));
        assert_eq!(super::SCOPED_PHYSICAL_ROUTE_ID, derive(2, 1));
        for (index, left) in ids.iter().enumerate() {
            for right in &ids[index + 1..] {
                assert_ne!(left, right, "XML physical route identities collide");
            }
        }
    }

    #[test]
    fn duplicate_dialects_are_rejected() {
        let dialects = [
            jqf_data::DialectIdRef::from_static(super::XML_DOCUMENT_DIALECT_ID),
            jqf_data::DialectIdRef::from_static(super::XML_DOCUMENT_DIALECT_ID),
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
    fn source_echo_and_deterministic_round_trip() {
        use jqf_codec_core::{
            AccessInput, AccessOutcome, AccessSession, EncodeItem, EncodeRequest, PreservationRequest,
        };
        use jqf_data::{DialectId, FormatId};
        use jqf_resource::{ContinueControl, RequestAccount, ResourceLimits, WorkMeter};

        static CONTROL: ContinueControl = ContinueControl;
        let mut resources = jqf_resource::ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                .expect("account"),
            &CONTROL,
            WorkMeter::try_new_v1(4_096).expect("work"),
        )
        .expect("resources");
        let bytes = b"<a b=\"1\">hi<x/>tail</a>";
        let source = jqf_source::ResolvedSource::new(
            jqf_source::SourceRef::new(jqf_source::SourceId::new(7), jqf_source::SourceKind::Input),
            "echo.xml",
            bytes,
            0,
        );
        let mut state = crate::session::XmlSession::new(source, false, true, jqf_data::BuilderCoverage::complete())
            .expect("session");
        let mut context = jqf_codec_core::CodecRunContext::new(&mut resources);
        let result = state.decode(AccessInput::Source(source), &mut context).expect("decode");
        let product = match result.outcome() {
            AccessOutcome::FullDocument(product) => product.try_clone().expect("clone"),
            AccessOutcome::Located(_) => panic!("expected full document"),
        };
        assert!(
            product.document().source_segment().is_some(),
            "the whole-document route must retain the source"
        );
        let root = product.document().root_handle();
        let format = FormatId::try_new(super::FORMAT_ID).expect("format");

        let encode = |dialect: &str, resources: &mut jqf_resource::ResourceContext<'_>| {
            let dialect = DialectId::try_new(dialect).expect("dialect");
            let factory = super::encode::create_factory(
                EncodeRequest {
                    format: &format,
                    dialect: &dialect,
                    diagnostics: jqf_codec_core::DiagnosticPolicy::ErrorsOnly,
                    preservation: PreservationRequest::None,
                    options: None,
                },
                resources,
            )
            .expect("factory");
            let mut session = factory
                .start(
                    EncodeItem::try_located(&product, root).expect("item"),
                    PreservationRequest::None,
                    resources,
                )
                .expect("session");
            let mut output = alloc::vec::Vec::new();
            {
                let mut sink = jqf_codec_core::VecByteSink::new(&mut output);
                let mut run = jqf_codec_core::CodecRunContext::new(resources);
                session.encode(&mut sink, &mut run).expect("encode");
            }
            output
        };

        // Deterministic profile re-renders with the §4.9 byte law.
        let rendered = String::from_utf8(encode(super::XML_DETERMINISTIC_DIALECT_ID, &mut resources)).expect("utf8");
        assert_eq!(rendered, "<a b=\"1\">hi<x></x>tail</a>\n");

        // Source profile echoes the exact retained bytes.
        let echoed = encode(super::XML_SOURCE_DIALECT_ID, &mut resources);
        assert_eq!(echoed, bytes.to_vec());
    }

    #[test]
    fn attributes_and_comments_attach_the_accessor_facts() {
        use jqf_codec_core::{AccessInput, AccessOutcome, AccessSession};
        use jqf_data::{FactPayloadView, LocalOwnerRef, ReaderPoll, unbounded_batch_limit};
        use jqf_resource::{ContinueControl, RequestAccount, ResourceLimits, WorkMeter};

        static CONTROL: ContinueControl = ContinueControl;
        let mut resources = jqf_resource::ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                .expect("account"),
            &CONTROL,
            WorkMeter::try_new_v1(4_096).expect("work"),
        )
        .expect("resources");
        let bytes = b"<a b=\"1\" c=\"2\"><!-- one -->hi<!-- two --></a>";
        let source = jqf_source::ResolvedSource::new(
            jqf_source::SourceRef::new(jqf_source::SourceId::new(7), jqf_source::SourceKind::Input),
            "accessors.xml",
            bytes,
            0,
        );
        let mut state = crate::session::XmlSession::new(source, false, true, jqf_data::BuilderCoverage::complete())
            .expect("session");
        let mut context = jqf_codec_core::CodecRunContext::new(&mut resources);
        let result = state.decode(AccessInput::Source(source), &mut context).expect("decode");
        let product = match result.outcome() {
            AccessOutcome::FullDocument(product) => product.try_clone().expect("clone"),
            AccessOutcome::Located(_) => panic!("expected full document"),
        };
        let document = product.document();
        let owner = LocalOwnerRef::Node(document.root());

        let limit = unbounded_batch_limit();
        let mut reader = document.fact_reader(&mut resources).expect("fact reader");
        let mut attrs: alloc::collections::BTreeMap<String, String> = alloc::collections::BTreeMap::new();
        let mut comments: Option<alloc::vec::Vec<String>> = None;
        let mut name: Option<String> = None;
        loop {
            match reader.poll_batch(limit, &mut resources).expect("poll") {
                ReaderPoll::Batch(batch) => {
                    for fact in batch.iter() {
                        if fact.owner() != owner {
                            continue;
                        }
                        match fact.role().as_str() {
                            super::document::ATTRIBUTE_FACT => {
                                if let FactPayloadView::Text(text) = fact.payload() {
                                    attrs.insert(fact.kind().as_str().to_owned(), text.to_owned());
                                }
                            }
                            super::document::COMMENT_FACT => {
                                if let FactPayloadView::List(list) = fact.payload() {
                                    comments = Some(
                                        list.iter()
                                            .map(|item| match item {
                                                FactPayloadView::Text(t) => t.to_owned(),
                                                _ => String::new(),
                                            })
                                            .collect(),
                                    );
                                }
                            }
                            super::document::NAME_FACT => {
                                if let FactPayloadView::Text(text) = fact.payload() {
                                    name = Some(text.to_owned());
                                }
                            }
                            _ => {}
                        }
                    }
                }
                ReaderPoll::Pending => {
                    resources.try_begin_next_cooperative_entry(4_096).expect("resume");
                }
                ReaderPoll::End(_) => break,
            }
        }

        // The element's expanded name and the per-attribute table remain.
        assert_eq!(name.as_deref(), Some("a"));
        // One `.&`-serving fact per attribute, keyed by the expanded name.
        // `.@attrs` is the map projection of this same table.
        assert_eq!(attrs.get("b").map(String::as_str), Some("1"));
        assert_eq!(attrs.get("c").map(String::as_str), Some("2"));
        assert_eq!(attrs.len(), 2);
        // The direct comment children, in order, as the `.@comment` fact.
        assert_eq!(comments, Some(vec![" one ".to_owned(), " two ".to_owned()]));
    }
}
