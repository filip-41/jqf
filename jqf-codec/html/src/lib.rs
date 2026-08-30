//! The HTML codec: WHATWG-recovered documents and context-bound fragments.

#![cfg_attr(not(test), no_std)]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
#![allow(
    clippy::missing_errors_doc,
    clippy::too_many_lines,
    clippy::result_large_err,
    clippy::unnecessary_wraps,
    clippy::if_not_else,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::struct_excessive_bools,
    clippy::collapsible_if,
    clippy::match_same_arms,
    clippy::doc_markdown,
    clippy::semicolon_if_nothing_returned,
    clippy::needless_return,
    clippy::map_unwrap_or,
    clippy::must_use_candidate,
    clippy::vec_init_then_push,
    clippy::match_wildcard_for_single_variants,
    clippy::to_digit_is_some,
    clippy::unused_self,
    clippy::map_identity,
    clippy::needless_borrow,
    clippy::trivially_copy_pass_by_ref,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value,
    clippy::manual_range_contains,
    clippy::unnecessary_cast,
    clippy::identity_op,
    clippy::single_match_else,
    clippy::assigning_clones,
    clippy::items_after_statements,
    clippy::uninlined_format_args,
    clippy::format_push_string,
    clippy::manual_unwrap_or,
    clippy::redundant_closure_for_method_calls,
    clippy::default_trait_access,
    clippy::only_used_in_recursion,
    clippy::collapsible_else_if,
    clippy::too_many_arguments,
    clippy::needless_range_loop,
    clippy::nonminimal_bool,
    clippy::unnested_or_patterns,
    clippy::manual_let_else,
    clippy::explicit_iter_loop,
    clippy::for_kv_map,
    clippy::semicolon_inside_block,
    clippy::redundant_guards,
    reason = "state-machine functions mirror the reference algorithm's shape"
)]

extern crate alloc;

// Debug tracing (development only): build with `RUSTFLAGS="--cfg jqf_trace"` to enable `std::eprintln!` traces in the
// state machines.
#[cfg(jqf_trace)]
extern crate std;

mod decode;
mod document;
mod encode;
#[allow(clippy::unreadable_literal)]
mod entities_table;
mod locate;
mod options;
mod provider;
mod scoped;
mod session;
mod tokenize;
mod tree;

/// Compiles the README examples as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;

/// The tokenizer's public core, exposed for the conformance harness (the tree builder drives it through the same door).
pub mod tokenizer_core {
    pub use super::tokenize::{Attribute, InitialState, Token, TokenKind, Tokenizer, decode_utf8, decode_windows_1252};
}

/// The tree builder's public core, exposed for the conformance harness.
pub mod tree_core {
    pub use super::tree::{DoctypeData, Namespace, Node, NodeId, NodeKind, QuirksMode, Tree, TreeBuilder};
}

use jqf_codec_core::{
    CodecDescriptor, CodecOperations, CodecRegistration, DecoderFactoryRecord, EncoderFactoryRecord, ItemByteOwner,
    RegistrationError, RouteCapability,
};
use jqf_data::{DialectIdRef, FormatIdRef};
/// Stable HTML format identity text.
pub const FORMAT_ID: &str = "html";
/// Stable HTML document dialect identity text.
pub const HTML_DOCUMENT_DIALECT_ID: &str = "html.document@1";
/// Stable HTML fragment dialect identity text.
pub const HTML_FRAGMENT_DIALECT_ID: &str = "html.fragment@1";
/// Stable source-echo output-profile identity text.
pub const HTML_SOURCE_DIALECT_ID: &str = "html.source@1";
/// Stable deterministic serialize output-profile identity text.
pub const HTML_DOCUMENT_SERIALIZE_DIALECT_ID: &str = "html.document-serialize@1";

/// The fragment input dialect's default context element : the WHATWG fragment algorithm (12.4) names a context element
/// that sets the starting insertion mode and the tokenizer's initial state. v1 surfaces the dialect with one FIXED
/// default context, `div` — the tree builder's own fragment default — documented in `--help`; a per-invocation context
/// option is not served (its channel does not exist yet).
pub const FRAGMENT_DEFAULT_CONTEXT: &str = "div";

/// The document registration's dialect set: the document input dialect and the two output profiles.
/// `html.fragment-serialize@1` stays RESERVED (its context-bound encode needs the fragment context channel).
const DIALECTS_DOCUMENT: [DialectIdRef<'static>; 3] = [
    DialectIdRef::from_static(HTML_DOCUMENT_DIALECT_ID),
    DialectIdRef::from_static(HTML_SOURCE_DIALECT_ID),
    DialectIdRef::from_static(HTML_DOCUMENT_SERIALIZE_DIALECT_ID),
];

/// The fragment registration's dialect set: the fragment input dialect alone (the catalog matches BOTH decoder and
/// encoder against the same descriptor list, so a registration carries its input dialect and output profiles together
/// and the registrations keep disjoint sets).
const DIALECTS_FRAGMENT: [DialectIdRef<'static>; 1] = [DialectIdRef::from_static(HTML_FRAGMENT_DIALECT_ID)];

/// Stable physical identity of the complete HTML document route.
pub(crate) const FULL_PHYSICAL_ROUTE_ID: jqf_codec_core::PhysicalRouteId =
    match jqf_codec_core::PhysicalRouteId::derive(crate::FORMAT_ID, 1, 1) {
        Some(id) => id,
        None => panic!("nonzero route identity"),
    };

/// Stable physical identity of the HTML located (scoped) route.
pub(crate) const SCOPED_PHYSICAL_ROUTE_ID: jqf_codec_core::PhysicalRouteId =
    match jqf_codec_core::PhysicalRouteId::derive(crate::FORMAT_ID, 2, 1) {
        Some(id) => id,
        None => panic!("nonzero route identity"),
    };

/// Stable physical identity of HTML encoding.
pub(crate) const ENCODE_PHYSICAL_ROUTE_ID: jqf_codec_core::PhysicalRouteId =
    match jqf_codec_core::PhysicalRouteId::derive(crate::FORMAT_ID, 3, 1) {
        Some(id) => id,
        None => panic!("nonzero route identity"),
    };

/// The CLI-facing routes the HTML registration serves: the whole-document and located routes over a single document per
/// source (HTML is NOT an adjacent-value format), and the record route's output side (`RouteCapability::Record`: one
/// synthesized `<root>` document per record).
const ROUTES: [RouteCapability; 1] = [RouteCapability::Record];

/// Constructs the allocation-free validated HTML codec registration (document input dialect + the two output profiles).
pub fn registration() -> Result<CodecRegistration<'static>, RegistrationError> {
    registration_for(
        &DIALECTS_DOCUMENT,
        false,
        // The document dialect is HTML's default input dialect, so it owns the `html`/`htm` extensions; the fragment
        // registration declares none.
        &["html", "htm"],
        // HTML has no edit lane; the facade supplies the item newline on every output profile.
        &[ItemByteOwner::Facade; 3],
    )
}

/// Constructs the allocation-free validated HTML fragment registration (`html.fragment@1` input only — the fragment
/// dialect decodes under the WHATWG fragment algorithm with the fixed [`FRAGMENT_DEFAULT_CONTEXT`] context).
pub fn registration_fragment() -> Result<CodecRegistration<'static>, RegistrationError> {
    registration_for(&DIALECTS_FRAGMENT, true, &[], &[ItemByteOwner::Facade])
}

fn registration_for(
    dialects: &'static [DialectIdRef<'static>],
    fragment: bool,
    extensions: &'static [&'static str],
    inter_item_byte: &'static [ItemByteOwner],
) -> Result<CodecRegistration<'static>, RegistrationError> {
    let format = FormatIdRef::from_static(FORMAT_ID);
    CodecRegistration::try_new(
        CodecDescriptor::new(
            format,
            dialects,
            CodecOperations::new(true, !fragment, false),
            &ROUTES,
            extensions,
            inter_item_byte,
            &[],
            // No insignificant inter-value bytes: every byte reaches the decoder.
            &[],
        ),
        Some(DecoderFactoryRecord::new(provider::decoder_for(fragment))),
        if fragment {
            None
        } else {
            Some(EncoderFactoryRecord::new(encode::create_factory))
        },
        None,
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each dialect lives in exactly one of the two HTML registrations.
    #[test]
    fn the_registration_dialect_set_has_no_duplicates() {
        for set in [super::DIALECTS_DOCUMENT.as_slice(), super::DIALECTS_FRAGMENT.as_slice()] {
            let mut seen: alloc::vec::Vec<&str> = alloc::vec::Vec::new();
            for dialect in set.iter().map(|d| d.as_str()) {
                assert!(
                    !seen.contains(&dialect),
                    "dialect {dialect} appears twice in an HTML set"
                );
                seen.push(dialect);
            }
        }
        // The two registrations keep DISJOINT dialect sets (the catalog matches both the decoder and the encoder
        // against the same descriptor list).
        for left in super::DIALECTS_DOCUMENT.iter() {
            for right in super::DIALECTS_FRAGMENT.iter() {
                assert_ne!(
                    left.as_str(),
                    right.as_str(),
                    "dialect {} must live in exactly one HTML registration",
                    left.as_str()
                );
            }
        }
    }

    /// `html.source@1` echoes the sealed source of an unchanged whole document.
    #[test]
    fn source_echo_reproduces_the_retained_bytes() {
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
        let bytes = b"<!DOCTYPE html><html><body><p>hi</p></body></html>";
        let source = jqf_source::ResolvedSource::new(
            jqf_source::SourceRef::new(jqf_source::SourceId::new(7), jqf_source::SourceKind::Input),
            "echo.html",
            bytes,
            0,
        );
        let mut state = crate::session::HtmlSession::new(source, false, jqf_data::BuilderCoverage::complete(), false)
            .expect("session");
        let mut context = jqf_codec_core::CodecRunContext::new(&mut resources);
        let result = state.decode(AccessInput::Source(source), &mut context).expect("decode");
        let product = match result.outcome() {
            AccessOutcome::FullDocument(product) => product.try_clone().expect("clone"),
            AccessOutcome::Located(_) => panic!("expected full document"),
        };
        let root = product.document().root_handle();
        let format = FormatId::try_new(super::FORMAT_ID).expect("format");
        let dialect = DialectId::try_new(super::HTML_SOURCE_DIALECT_ID).expect("dialect");
        let factory = super::encode::create_factory(
            EncodeRequest {
                format: &format,
                dialect: &dialect,
                diagnostics: jqf_codec_core::DiagnosticPolicy::ErrorsOnly,
                preservation: PreservationRequest::None,
                options: None,
            },
            &mut resources,
        )
        .expect("factory");
        let mut session = factory
            .start(
                EncodeItem::try_located(&product, root).expect("item"),
                PreservationRequest::None,
                &mut resources,
            )
            .expect("session");
        let mut output = alloc::vec::Vec::new();
        {
            let mut sink = jqf_codec_core::VecByteSink::new(&mut output);
            let mut run = jqf_codec_core::CodecRunContext::new(&mut resources);
            session.encode(&mut sink, &mut run).expect("encode");
        }
        assert_eq!(output, bytes);
    }

    /// The three HTML physical route identities are the derived triples `(html, 1|2|3, 1)` and are pairwise distinct.
    #[test]
    fn physical_route_ids_are_stable_and_distinct() {
        let derive =
            |kind, spec| jqf_codec_core::PhysicalRouteId::derive(super::FORMAT_ID, kind, spec).expect("derived");
        assert_eq!(super::FULL_PHYSICAL_ROUTE_ID, derive(1, 1));
        assert_eq!(super::SCOPED_PHYSICAL_ROUTE_ID, derive(2, 1));
        assert_eq!(super::ENCODE_PHYSICAL_ROUTE_ID, derive(3, 1));
    }
}
