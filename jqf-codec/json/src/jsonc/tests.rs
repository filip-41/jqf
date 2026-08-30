//! The JSONC codec's own tests: decode through both dialects, comment-fact attachment, and the edit splices that must
//! preserve comments.
//!
//! Decode: the JSONC ⊇ JSON law (every strict document decodes to exactly the value the strict JSON codec reads,
//! through BOTH dialects), the trailing-comma dial that separates them, byte-order-mark consumption,
//! unterminated-block-comment rejection, a `//` inside a lazily-deferred span re-reading under JSONC grammar, and
//! comments whose text crosses one or several work-admission boundaries yet decode complete.
//!
//! Facts: where a comment attaches — the leading edge of the member's VALUE node it precedes, a trailer on the root
//! — and that `FactIntent::None` attaches none. CRLF files end a line comment's fact at the carriage return. Lazy
//! materialize of a commented object with Preserve/facts attaches the same leading `jsonc.comment@1` list-of-texts.
//!
//! Edits: an object-append splice lands new members BEFORE a trailing comment block (comma-and-comment shapes included)
//! and re-emits a multi-line comment payload line by line.
//!
//! Tags: validation per JSONC target dialect answers the `NoTags` law (the empty set valid, any tag invalid).

use jqf_codec_core::{
    AccessAdapter, AccessFootprint, AccessGuarantees, AccessOutcome, AccessRequirement, CodecRunContext, DecodeRequest,
    DiagnosticPolicy, ExactPath, ExactSelectionRecord, FactIntent, ValidationMode,
};
use jqf_data::{DialectId, LocalOwnerRef, ReaderPoll};
use jqf_resource::ResourceContext;
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

use crate::test_support::{self, demand, requirement, requirement_preserving_facts};

use super::{
    DEFAULT_DIALECT_ID, DEFAULT_JQF_DIALECT_ID, FORMAT_ID, JQF_1_0_DIALECT_ID, TRAILING_DIALECT_ID,
    TRAILING_JQF_DIALECT_ID, registration,
};

fn source(bytes: &'static [u8]) -> ResolvedSource<'static> {
    ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "jsonc-test",
        bytes,
        0,
    )
}

/// Runs a decode and returns the owned product, or panics on failure.
fn decode_product(
    bytes: &'static [u8],
    dialect: &'static str,
    resources: &mut ResourceContext<'static>,
) -> jqf_codec_core::DocumentProduct<'static> {
    decode_product_with(registration(), dialect, bytes, resources)
}

/// The same decode against an explicit registration, so a test can read the same bytes through a sibling codec.
fn decode_product_with(
    registration: Result<jqf_codec_core::CodecRegistration<'static>, jqf_codec_core::RegistrationError>,
    dialect: &'static str,
    bytes: &'static [u8],
    resources: &mut ResourceContext<'static>,
) -> jqf_codec_core::DocumentProduct<'static> {
    let dialect = DialectId::try_new(dialect).expect("dialect");
    let mut provider = registration
        .expect("registration")
        .decoder()
        .expect("decoder")
        .create_provider(
            source(bytes),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &dialect,
                options: None,
                allow_adjacent_values: false,
                value_separator: crate::VALUE_SEPARATORS,
            },
            resources,
        )
        .expect("provider");
    let requirement = requirement_preserving_facts(resources);
    let handle = provider.bind(&requirement).expect("bind");
    let mut session = provider.open(&handle, resources).expect("open");
    let result = {
        let mut run = CodecRunContext::new(resources);
        run.set_cooperative_credits(4_096);
        session.decode(&mut run).expect("decode")
    };
    let (outcome, _) = result.into_parts();
    let AccessOutcome::FullDocument(product) = outcome else {
        panic!("expected full document")
    };
    product
}

fn exact_preserving(members: &[&str], resources: &ResourceContext<'_>) -> AccessRequirement {
    let guarantees = AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly);
    let mut exact = ExactPath::try_new(resources);
    for member in members {
        exact.try_push_semantic_member(member, resources).expect("member");
    }
    let footprint = AccessFootprint::try_exact(exact, resources);
    AccessRequirement::try_exact(footprint, demand(resources), guarantees, resources)
        .expect("exact requirement")
        .with_fact_intent(FactIntent::Preserve)
}

fn decode_located(
    bytes: &'static [u8],
    dialect: &'static str,
    members: &[&str],
    resources: &mut ResourceContext<'static>,
) -> jqf_codec_core::AccessResult<'static> {
    let dialect = DialectId::try_new(dialect).expect("dialect");
    let mut provider = registration()
        .expect("registration")
        .decoder()
        .expect("decoder")
        .create_provider(
            source(bytes),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &dialect,
                options: None,
                allow_adjacent_values: false,
                value_separator: crate::VALUE_SEPARATORS,
            },
            resources,
        )
        .expect("provider");
    let requirement = exact_preserving(members, resources);
    let handle = provider.bind(&requirement).expect("bind");
    let mut session = provider.open(&handle, resources).expect("open");
    let mut run = CodecRunContext::new(resources);
    run.set_cooperative_credits(4_096);
    session.decode(&mut run).expect("decode")
}

fn collect_comment_facts(
    product: &jqf_codec_core::DocumentProduct<'_>,
    resources: &mut ResourceContext<'_>,
) -> alloc::vec::Vec<(jqf_data::NodeId, alloc::vec::Vec<alloc::string::String>)> {
    let document = product.document();
    let limit = jqf_data::BatchLimit::new(usize::MAX).expect("limit");
    let mut reader = document.fact_reader(resources).expect("reader");
    let mut out = alloc::vec::Vec::new();
    loop {
        match reader.poll_batch(limit, resources).expect("poll") {
            ReaderPoll::Batch(batch) => {
                for fact in batch.iter() {
                    let LocalOwnerRef::Node(node) = fact.owner() else {
                        continue;
                    };
                    if fact.role().as_str() != "jsonc.comment@1" {
                        continue;
                    }
                    let jqf_data::FactPayloadView::List(texts) = fact.payload() else {
                        continue;
                    };
                    let mut lines = alloc::vec::Vec::new();
                    for entry in texts.iter() {
                        if let jqf_data::FactPayloadView::Text(text) = entry {
                            lines.push(alloc::string::String::from(text));
                        }
                    }
                    out.push((node, lines));
                }
            }
            ReaderPoll::Pending => {
                resources.try_begin_next_cooperative_entry(4_096).expect("resume");
            }
            ReaderPoll::End(_) => break,
        }
    }
    out
}

/// The leading comment attaches to the VALUE node of the member it precedes (`.compilerOptions.@comment` serves it),
/// and the trailing comment (no following value) attaches to the root: the comment fact's scope is leading-only, and
/// the trailer is the TOML law.
#[test]
fn comments_attach_as_leading_facts() {
    let bytes =
        b"{\n  // compiler options\n  \"compilerOptions\": {\n    \"target\": \"ES2020\",\n  },\n  // the trailer\n}\n";
    let mut resources = test_support::resources();
    let product = decode_product(bytes, TRAILING_DIALECT_ID, &mut resources);
    let facts = collect_comment_facts(&product, &mut resources);
    assert_eq!(facts.len(), 2, "facts: {facts:?}");
    // One fact on the compilerOptions VALUE node, one on the root.
    let (node, lines) = &facts[0];
    assert_eq!(lines, &alloc::vec![alloc::string::String::from("compiler options")]);
    // The root trailer fact carries the trailer text.
    assert_eq!(facts[1].1, alloc::vec![alloc::string::String::from("the trailer")]);
    // Sanity: the fact owner is NOT the root for the member comment.
    assert_ne!(*node, product.document().root());
}

/// `FactIntent::None` (the default) parses comments as trivia and attaches none: identity JSONC → JSON must not carry
/// facts the JSON encoder cannot emit.
#[test]
fn fact_intent_none_does_not_attach_comment_facts() {
    let bytes = b"{\n  // compiler options\n  \"compilerOptions\": {\n    \"target\": \"ES2020\",\n  },\n}\n";
    let mut resources = test_support::resources();
    let dialect = DialectId::try_new(TRAILING_DIALECT_ID).expect("dialect");
    let mut provider = registration()
        .expect("registration")
        .decoder()
        .expect("decoder")
        .create_provider(
            source(bytes),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &dialect,
                options: None,
                allow_adjacent_values: false,
                value_separator: crate::VALUE_SEPARATORS,
            },
            &mut resources,
        )
        .expect("provider");
    let requirement = requirement(&resources);
    assert_eq!(requirement.fact_intent(), FactIntent::None);
    let handle = provider.bind(&requirement).expect("bind");
    let mut session = provider.open(&handle, &mut resources).expect("open");
    let result = {
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        session.decode(&mut run).expect("decode")
    };
    let (outcome, _) = result.into_parts();
    let AccessOutcome::FullDocument(product) = outcome else {
        panic!("expected full document")
    };
    assert!(
        !product
            .document()
            .coverage()
            .contains(jqf_data::DocumentCapability::AttachedFacts),
        "None must not retain attached-fact coverage"
    );
}

/// A `//` inside a deferred array must re-read under JSONC grammar, not STRICT. The leading comment is trivia above the
/// root; the inner comment sits in the span the frontier of 1 defers.
#[test]
fn lazy_frontier_materializes_a_comment_bearing_span() {
    let bytes = b"// c\n{\"a\":[1, // inner\n2, 3]}";
    let mut resources = test_support::resources();
    let dialect = DialectId::try_new(TRAILING_DIALECT_ID).expect("dialect");
    let mut provider = registration()
        .expect("registration")
        .decoder()
        .expect("decoder")
        .create_provider(
            source(bytes),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &dialect,
                options: None,
                allow_adjacent_values: false,
                value_separator: crate::VALUE_SEPARATORS,
            },
            &mut resources,
        )
        .expect("provider");
    let requirement = requirement_preserving_facts(&resources).with_lazy_frontier(1);
    let handle = provider.bind(&requirement).expect("bind");
    let mut session = provider.open(&handle, &mut resources).expect("open");
    let result = {
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        session.decode(&mut run).expect("decode")
    };
    let (outcome, _) = result.into_parts();
    let AccessOutcome::FullDocument(product) = outcome else {
        panic!("expected full document")
    };
    let value = product
        .document()
        .materialize_root(&mut resources)
        .expect("a // in a deferred span must not take the STRICT materializer");
    let jqf_data::Value::Object(object) = value else {
        panic!("expected object, got {value:?}")
    };
    let a = object.get("a").expect("member a");
    let jqf_data::Value::Array(items) = a else {
        panic!("expected array .a, got {a:?}")
    };
    assert_eq!(items.len(), 3);
}

/// Lazy materialize of a commented JSONC object with Preserve/facts attaches the leading comment as
/// `jsonc.comment@1` list-of-texts on the member's value node — the same ownership [`crate::parse::JsonParseState`]
/// uses. Exact does not take this path.
#[test]
fn lazy_materialize_of_a_commented_jsonc_object_attaches_the_leading_comment() {
    let text = "{ // leading\n  \"a\": 1 }";
    let mut resources = test_support::resources();
    let product = crate::lazy::JSONC_TRAILING_SPAN_MATERIALIZER_FACTS
        .parse_document(text, &mut resources)
        .expect("JSONC facts materializer must re-read a commented object");
    let facts = collect_comment_facts(&product, &mut resources);
    assert_eq!(facts.len(), 1, "facts: {facts:?}");
    assert_eq!(facts[0].1, alloc::vec![alloc::string::String::from("leading")]);
    assert_ne!(
        facts[0].0,
        product.document().root(),
        "a comment inside the object leads the member value, not the object root"
    );
}

/// Strict JSON documents decode through BOTH JSONC dialects to exactly the value the strict JSON codec reads (JSONC ⊇
/// JSON, the conformance corpus's first stand-in).
#[test]
fn every_strict_document_decodes() {
    for bytes in [
        &b"null"[..],
        b"true",
        b"123",
        b"1.5e3",
        b"\"a\\n\\u0041\"",
        b"[]",
        b"[1,2,{\"a\":[]}]",
        b"{\"a\":1,\"b\":[true,false,null]}",
        // A `/` inside a string literal is the flagship comments-as-trivia hazard: it must never open a comment.
        b"\"http://x/?a=1&b=2\"",
        b"{\"url\":\"https://example.com/a//b\",\"q\":\"?\"}",
    ] {
        let strict = root_value(bytes, None);
        for dialect in [TRAILING_DIALECT_ID, DEFAULT_DIALECT_ID] {
            assert_eq!(
                root_value(bytes, Some(dialect)),
                strict,
                "{dialect} must read {bytes:?} as the strict codec does"
            );
        }
    }
}

/// The materialized root of `bytes`, read through one JSONC dialect or — with `dialect` absent — through the strict
/// JSON codec.
fn root_value(bytes: &'static [u8], dialect: Option<&'static str>) -> alloc::string::String {
    let mut resources = test_support::resources();
    let product = match dialect {
        Some(dialect) => decode_product(bytes, dialect, &mut resources),
        None => decode_product_with(
            crate::registration::registration(),
            crate::RFC8259_DIALECT_ID,
            bytes,
            &mut resources,
        ),
    };
    let value = product
        .document()
        .materialize_root(&mut resources)
        .expect("materialize");
    alloc::format!("{value:?}")
}

/// A leading byte-order mark is consumed, not rejected. JSONC's real corpus is editor-written configuration — VS
/// Code's `settings.json`, anything a Windows editor saved — which is exactly where marks live, and the sibling
/// dialects already accepted the same bytes (strict JSON strips the mark before the first value, JSON5 reads U+FEFF as
/// whitespace). Only JSONC refused.
#[test]
fn a_leading_byte_order_mark_is_consumed() {
    for dialect in [TRAILING_DIALECT_ID, DEFAULT_DIALECT_ID] {
        let mut resources = test_support::resources();
        let product = decode_product(b"\xef\xbb\xbf{\n  // lead\n  \"a\": 1\n}\n", dialect, &mut resources);
        let facts = collect_comment_facts(&product, &mut resources);
        assert_eq!(facts.len(), 1, "the comment still attaches: {facts:?}");
        assert_eq!(facts[0].1, alloc::vec![alloc::string::String::from("lead")]);
    }
}

/// The strict dialect rejects a trailing comma; the trailing dialect accepts it.
#[test]
fn trailing_comma_dialect_difference() {
    let bytes = b"{\"a\": 1,}";
    assert_eq!(
        root_value(bytes, Some(TRAILING_DIALECT_ID)),
        root_value(b"{\"a\": 1}", None),
        "the trailing dialect reads the same value the comma-free source has"
    );
    // The ARRAY form is the same dial: only the object form was pinned before.
    assert_eq!(
        root_value(b"[1, 2,]", Some(TRAILING_DIALECT_ID)),
        root_value(b"[1, 2]", None),
        "the trailing dialect reads an array trailing comma too"
    );
    let array_bytes = b"[1, 2,]";
    let mut resources = test_support::resources();
    let dialect = DialectId::try_new(DEFAULT_DIALECT_ID).expect("dialect");
    let mut provider = registration()
        .expect("registration")
        .decoder()
        .expect("decoder")
        .create_provider(
            source(bytes),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &dialect,
                options: None,
                allow_adjacent_values: false,
                value_separator: crate::VALUE_SEPARATORS,
            },
            &mut resources,
        )
        .expect("provider");
    let object_requirement = requirement(&resources);
    let handle = provider.bind(&object_requirement).expect("bind");
    let mut session = provider.open(&handle, &mut resources).expect("open");
    let result = {
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        session.decode(&mut run)
    };
    let rejected = match result {
        Ok(result) => !matches!(result.into_parts().0, AccessOutcome::FullDocument(_)),
        Err(_) => true,
    };
    assert!(
        rejected,
        "the strict-comma dialect must reject an object trailing comma"
    );
    // The array shape rides the same dial.
    let mut resources = test_support::resources();
    let dialect = DialectId::try_new(DEFAULT_DIALECT_ID).expect("dialect");
    let mut array_provider = registration()
        .expect("registration")
        .decoder()
        .expect("decoder")
        .create_provider(
            source(array_bytes),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &dialect,
                options: None,
                allow_adjacent_values: false,
                value_separator: crate::VALUE_SEPARATORS,
            },
            &mut resources,
        )
        .expect("array provider");
    let array_requirement = requirement(&resources);
    let handle = array_provider.bind(&array_requirement).expect("bind");
    let mut session = array_provider.open(&handle, &mut resources).expect("open");
    let result = {
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        session.decode(&mut run)
    };
    let rejected = match result {
        Ok(result) => !matches!(result.into_parts().0, AccessOutcome::FullDocument(_)),
        Err(_) => true,
    };
    assert!(rejected, "the strict-comma dialect must reject an array trailing comma");
}

/// An unterminated block comment is a clean rejection, never a hang or a partial value.
#[test]
fn unterminated_comment_is_rejected() {
    let bytes = b"{\"a\": 1 /* never closes}";
    let mut resources = test_support::resources();
    let dialect = DialectId::try_new(TRAILING_DIALECT_ID).expect("dialect");
    let mut provider = registration()
        .expect("registration")
        .decoder()
        .expect("decoder")
        .create_provider(
            source(bytes),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &dialect,
                options: None,
                allow_adjacent_values: false,
                value_separator: crate::VALUE_SEPARATORS,
            },
            &mut resources,
        )
        .expect("provider");
    let requirement = requirement(&resources);
    let handle = provider.bind(&requirement).expect("bind");
    let mut session = provider.open(&handle, &mut resources).expect("open");
    let result = {
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        session.decode(&mut run)
    };
    let rejected = match result {
        Ok(result) => !matches!(result.into_parts().0, AccessOutcome::FullDocument(_)),
        Err(_) => true,
    };
    assert!(
        rejected,
        "an unterminated block comment must be rejected, never completed"
    );
}

/// The append splice never orphans a trailing comment: the new member lands BEFORE the comment block, which stays a
/// trailer.
#[test]
fn append_splice_preserves_trailing_comment() {
    let bytes = b"{\n  \"a\": 1,\n  // trailing\n}\n";
    let mut resources = test_support::resources();
    let product = decode_product(bytes, TRAILING_DIALECT_ID, &mut resources);
    let document = product.document();
    let root = document.root();
    let two = jqf_data::Number::try_json_literal("2").expect("2");
    let two = jqf_data::Value::Number(two);
    let members = jqf_codec_core::EditAppendMembers::Table(&[("x", &two)]);
    let insertions =
        super::encode::jsonc_render_edit_append(document, root, bytes, members, &mut resources).expect("append");
    // The insertion lands after the last value, before the comment.
    assert_eq!(insertions.len(), 1);
    let insertion = &insertions[0];
    let patched: alloc::vec::Vec<u8> = bytes
        .iter()
        .copied()
        .take(insertion.at)
        .chain(insertion.bytes.iter().copied())
        .chain(bytes.iter().copied().skip(insertion.at))
        .collect();
    let text = alloc::string::String::from_utf8(patched).expect("utf8");
    assert!(text.contains("// trailing"), "trailing comment must survive: {text}");
    // The new member must NOT be orphaned below the comment.
    let after = text.find("\"x\"").expect("new member present");
    let comment = text.find("// trailing").expect("comment present");
    assert!(after < comment, "new member lands before the comment: {text}");
}

/// Applies one object-append splice of `"x": 2` and returns the patched bytes. An empty insertion set is the floor;
/// these shapes must splice.
fn append_x(bytes: &'static [u8]) -> alloc::string::String {
    let mut resources = test_support::resources();
    let product = decode_product(bytes, TRAILING_DIALECT_ID, &mut resources);
    let document = product.document();
    let root = document.root();
    let two = jqf_data::Number::try_json_literal("2").expect("2");
    let two = jqf_data::Value::Number(two);
    let members = jqf_codec_core::EditAppendMembers::Table(&[("x", &two)]);
    let insertions =
        super::encode::jsonc_render_edit_append(document, root, bytes, members, &mut resources).expect("append");
    assert_eq!(
        insertions.len(),
        1,
        "comma+comment append must splice, not decline to the floor: {bytes:?}"
    );
    let insertion = &insertions[0];
    let patched: alloc::vec::Vec<u8> = bytes
        .iter()
        .copied()
        .take(insertion.at)
        .chain(insertion.bytes.iter().copied())
        .chain(bytes.iter().copied().skip(insertion.at))
        .collect();
    alloc::string::String::from_utf8(patched).expect("utf8")
}

/// New members land before the trailer when the last member's trailing comma is separated from the value by whitespace
/// or a comment. The last member keeps its own trailing comma; sibling `/* */` bytes stay verbatim (a floor re-encode
/// would respell them as `//` lines).
#[test]
fn append_splice_comma_and_comment_together() {
    let cases: &[(&'static [u8], &str)] = &[
        // Same-line comma then comment (`}` is on the next line: `//` runs to the line feed).
        (b"{ \"a\": 1, // c\n}", "{ \"a\": 1, \"x\": 2, // c\n}"),
        // Space before the comma: the scan's value_end is whitespace, not the comma — the insert walks to the comma
        // so it stays a trailer.
        (b"{ \"a\": 1 , // c\n}", "{ \"a\": 1 , \"x\": 2, // c\n}"),
        // Comma on the comment line.
        (b"{ \"a\": 1\n, // c\n}", "{ \"a\": 1\n, \"x\": 2, // c\n}"),
        // Block comment between the value and the comma.
        (br#"{ "a": 1 /* c */,}"#, r#"{ "a": 1,"x":2 /* c */,}"#),
        // Leading comment fact plus a trailing comma.
        (b"{ // lead\n  \"a\": 1,\n}", "{ // lead\n  \"a\": 1,\n  \"x\": 2,\n}"),
        // tsconfig-shaped last member.
        (
            b"{\n  \"strict\": true, // type-check\n}\n",
            "{\n  \"strict\": true, \"x\": 2, // type-check\n}\n",
        ),
        // Sibling block comment stays verbatim (the floor would rewrite it).
        (
            b"{\n  /* keep */\n  \"a\": 1 , // c\n}",
            "{\n  /* keep */\n  \"a\": 1 , \"x\": 2, // c\n}",
        ),
    ];
    for (bytes, expected) in cases {
        let text = append_x(bytes);
        assert_eq!(text, *expected, "splice of {bytes:?}");
        if let Some(comment) = text.find("// c").or_else(|| text.find("// type-check")) {
            let member = text.find("\"x\"").expect("new member present");
            assert!(member < comment, "new member lands before the trailer: {text}");
        }
        if bytes.windows(8).any(|window| window == b"/* keep */") {
            assert!(
                text.contains("/* keep */"),
                "sibling block comment must stay verbatim: {text}"
            );
        }
    }
}

/// A last member whose value is itself a multi-member nested container still splices: the scan advances past nested
/// commas instead of falling into its bare-word decline (the tsconfig shape used to take the whole-document floor).
#[test]
fn append_splice_survives_nested_commas() {
    let cases: &[(&'static [u8], &str)] = &[
        // Nested object with an internal comma.
        (
            b"{\"outer\": {\"a\": 1, \"b\": 2}}",
            "{\"outer\": {\"a\": 1, \"b\": 2},\"x\":2}",
        ),
        // Nested array with an internal comma.
        (b"{\"list\": [1, 2]}", "{\"list\": [1, 2],\"x\":2}"),
        // Nested containers before a later flat member.
        (
            b"{\"m\": [{\"i\": 1, \"j\": 2}], \"k\": true}",
            "{\"m\": [{\"i\": 1, \"j\": 2}], \"k\": true,\"x\":2}",
        ),
    ];
    for (bytes, expected) in cases {
        assert_eq!(append_x(bytes), *expected, "splice of {bytes:?}");
    }
}

/// A multi-line comment payload re-emits every line through the splice path, exactly as the whole-document floor does
/// — the two write paths must agree (the splice used to keep only the first line).
#[test]
fn comment_write_multiline_payload_emits_every_line() {
    let bytes = b"{\n  \"a\": 1\n}\n";
    let mut resources = test_support::resources();
    let product = decode_product(bytes, TRAILING_DIALECT_ID, &mut resources);
    let document = product.document();
    let text = jqf_data::Shared::try_from_str("line1\nline2").expect("shared str");
    let array = jqf_data::Array::try_from_vec(alloc::vec![jqf_data::Value::String(text)]).expect("array");
    let payload = jqf_data::Value::Array(array);
    let patches = super::encode::render_comment_write(document, document.root(), bytes, &payload)
        .expect("comment write")
        .expect("a comment on a spanned node splices");
    assert_eq!(patches.len(), 1);
    let replacement = alloc::string::String::from_utf8(patches[0].replacement.clone()).expect("utf8");
    assert!(
        replacement.contains("// line1\n") && replacement.contains("// line2\n"),
        "both lines emitted as separate comments: {replacement:?}"
    );
}

/// A tag-validation request targeting a `JSONC` profile opens THIS codec's validator and answers the `NoTags` law
/// (empty set valid, any tag invalid). The shared strict-JSON factory would refuse the same request as a foreign target
/// — the registration names its own factory, and this pins it.
#[test]
fn tag_validator_answers_the_no_tags_law_for_jsonc_targets() {
    use jqf_codec_core::{EncodeRequest, PreservationRequest};
    use jqf_data::TagId;

    for dialect_text in super::DIALECT_TEXTS {
        let format = jqf_data::FormatId::try_new(super::FORMAT_ID).expect("format id");
        let dialect = DialectId::try_new(dialect_text).expect("dialect");
        let request = EncodeRequest {
            format: &format,
            dialect: &dialect,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            preservation: PreservationRequest::None,
            options: None,
        };
        let mut resources = test_support::resources();
        let validator = crate::tag::create_jsonc_validator(request, &mut resources)
            .expect("a JSONC target opens the jsonc validator");
        validator.validate(&[], &resources).expect("empty set valid");
        let tag = TagId::try_new_unaccounted("!money").expect("tag");
        assert!(
            validator.validate(&[&tag], &resources).is_err(),
            "{dialect_text}: a non-core tag is invalid for JSONC output"
        );
    }
}

/// Leaks a fixture so `decode_product` can borrow it as `&'static [u8]`.
fn leaked(bytes: alloc::vec::Vec<u8>) -> &'static [u8] {
    alloc::boxed::Box::leak(bytes.into_boxed_slice())
}

/// A line comment whose body crosses the first work-admission boundary (one admission covers at most 256 bytes) must
/// decode with its COMPLETE text: the scan used to stop at the GRANT end, publish the truncated text as the `.@comment`
/// fact, and resume mid-comment, rejecting valid JSONC.
#[test]
fn line_comment_longer_than_one_grant_decodes_with_complete_fact() {
    let body = "jqf keeps configuration comments intact. ".repeat(9);
    assert!(body.len() + 4 > 256, "fixture must cross one grant");
    let mut source = alloc::vec::Vec::new();
    source.extend_from_slice(b"// ");
    source.extend_from_slice(body.as_bytes());
    source.extend_from_slice(b"\n{\"a\": 1}\n");
    let mut resources = test_support::resources();
    let product = decode_product(leaked(source), TRAILING_DIALECT_ID, &mut resources);
    let facts = collect_comment_facts(&product, &mut resources);
    assert_eq!(facts.len(), 1, "facts: {facts:?}");
    assert_eq!(facts[0].1, alloc::vec![alloc::string::String::from(&body)]);
}

/// The same law for a block comment spanning several admissions: the `*/` terminator lies beyond more than one 256-byte
/// grant, so the scan must accumulate grants instead of treating the window end as the source end.
#[test]
fn block_comment_spanning_several_grants_decodes_with_complete_fact() {
    let body = "block comment body crossing many work windows. ".repeat(16);
    assert!(body.len() + 6 > 512, "fixture must cross several grants");
    let mut source = alloc::vec::Vec::new();
    source.extend_from_slice(b"/* ");
    source.extend_from_slice(body.as_bytes());
    source.extend_from_slice(b" */\n{\"a\": 1}\n");
    let mut resources = test_support::resources();
    let product = decode_product(leaked(source), TRAILING_DIALECT_ID, &mut resources);
    let facts = collect_comment_facts(&product, &mut resources);
    assert_eq!(facts.len(), 1, "facts: {facts:?}");
    assert_eq!(facts[0].1, alloc::vec![alloc::string::String::from(&body)]);
}

/// On a CRLF file a line comment's fact text ends at the `\r`: it is half of the line break, not content, matching the
/// TOML extraction twin.
#[test]
fn crlf_line_comment_fact_has_no_trailing_carriage_return() {
    let bytes = b"{\r\n  // compiler options\r\n  \"target\": \"ES2020\"\r\n}\r\n";
    let mut resources = test_support::resources();
    let product = decode_product(bytes, TRAILING_DIALECT_ID, &mut resources);
    let facts = collect_comment_facts(&product, &mut resources);
    assert_eq!(facts.len(), 1, "facts: {facts:?}");
    assert_eq!(facts[0].1, alloc::vec![alloc::string::String::from("compiler options")]);
}

/// Exact Direct-binds slot 1, and a member's leading comment survives as `.@comment` on the located subtree root.
#[test]
fn exact_preserves_leading_comment_on_the_located_member() {
    let bytes =
        b"{\n  // compiler options\n  \"compilerOptions\": {\n    \"target\": \"ES2020\"\n  },\n  // the trailer\n}\n";
    let mut resources = test_support::resources();
    assert_eq!(
        registration()
            .expect("registration")
            .decoder()
            .expect("decoder")
            .create_provider(
                source(bytes),
                DecodeRequest {
                    validation: ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect: &DialectId::try_new(TRAILING_DIALECT_ID).expect("dialect"),
                    options: None,
                    allow_adjacent_values: false,
                    value_separator: crate::VALUE_SEPARATORS,
                },
                &mut resources,
            )
            .expect("provider")
            .route_descriptions()
            .len(),
        2
    );
    let result = decode_located(bytes, TRAILING_DIALECT_ID, &["compilerOptions"], &mut resources);
    assert_eq!(result.report().adapter(), AccessAdapter::None);
    assert_eq!(
        result.report().route().expect("receipt").route(),
        super::provider::SCOPED_PHYSICAL_ROUTE_ID
    );
    assert_eq!(result.report().route().expect("receipt").slot().get(), 1);
    let (outcome, _) = result.into_parts();
    let AccessOutcome::Located(located) = outcome else {
        panic!("expected located")
    };
    let ExactSelectionRecord::Node { node, .. } = located.result() else {
        panic!("node")
    };
    assert_eq!(*node, located.product().document().root_handle());
    let facts = collect_comment_facts(located.product(), &mut resources);
    assert_eq!(facts.len(), 1, "facts: {facts:?}");
    assert_eq!(facts[0].0, located.product().document().root());
    assert_eq!(facts[0].1, alloc::vec![alloc::string::String::from("compiler options")]);
}

/// A trailing-comma document locates through Exact; the strict-comma dialect still rejects the same bytes.
#[test]
fn exact_honours_the_trailing_comma_dialect() {
    let bytes = b"{\"a\": 1,}";
    let mut resources = test_support::resources();
    let located = decode_located(bytes, TRAILING_DIALECT_ID, &["a"], &mut resources);
    let AccessOutcome::Located(selected) = located.into_parts().0 else {
        panic!("trailing dialect Exact must accept a trailing comma")
    };
    let ExactSelectionRecord::Node { .. } = selected.result() else {
        panic!("node")
    };

    let dialect = DialectId::try_new(DEFAULT_DIALECT_ID).expect("dialect");
    let mut provider = registration()
        .expect("registration")
        .decoder()
        .expect("decoder")
        .create_provider(
            source(bytes),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &dialect,
                options: None,
                allow_adjacent_values: false,
                value_separator: crate::VALUE_SEPARATORS,
            },
            &mut resources,
        )
        .expect("provider");
    let requirement = exact_preserving(&["a"], &resources);
    let handle = provider.bind(&requirement).expect("bind");
    let mut session = provider.open(&handle, &mut resources).expect("open");
    let mut run = CodecRunContext::new(&mut resources);
    run.set_cooperative_credits(4_096);
    assert!(
        session.decode(&mut run).is_err(),
        "the strict-comma dialect must reject a trailing comma on Exact"
    );
}

/// Identity Exact still rejects unread non-trivia; trailing comments are trivia and must not fail validation.
#[test]
fn identity_exact_validates_unread_bytes() {
    let mut resources = test_support::resources();
    let commented = decode_located(b"{\"a\": 1}\n// trailer\n", TRAILING_DIALECT_ID, &[], &mut resources);
    assert_eq!(commented.report().adapter(), AccessAdapter::None);
    let AccessOutcome::Located(_) = commented.into_parts().0 else {
        panic!("trailing comment is trivia")
    };

    let dialect = DialectId::try_new(TRAILING_DIALECT_ID).expect("dialect");
    let mut provider = registration()
        .expect("registration")
        .decoder()
        .expect("decoder")
        .create_provider(
            source(b"{\"a\": 1} garbage"),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &dialect,
                options: None,
                allow_adjacent_values: false,
                value_separator: crate::VALUE_SEPARATORS,
            },
            &mut resources,
        )
        .expect("provider");
    let requirement = exact_preserving(&[], &resources);
    let handle = provider.bind(&requirement).expect("bind");
    let mut session = provider.open(&handle, &mut resources).expect("open");
    let mut run = CodecRunContext::new(&mut resources);
    run.set_cooperative_credits(4_096);
    assert!(
        session.decode(&mut run).is_err(),
        "unread trailing value must fail Exact validation"
    );
}

/// Encodes one owned array through the registered factory path with an OPTIONS-LESS request, so the request DIALECT
/// alone selects the comma law.
fn encode_array_under(dialect_text: &str) -> alloc::vec::Vec<u8> {
    use jqf_codec_core::{ByteSink, EncodeItem, EncodeRequest, ErasedEncoderFactory, PreservationRequest};

    struct Collect(alloc::vec::Vec<u8>);
    impl ByteSink for Collect {
        fn write(
            &mut self,
            bytes: &[u8],
            _resources: &mut ResourceContext<'_>,
        ) -> Result<usize, jqf_codec_core::CodecError> {
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> Result<(), jqf_codec_core::CodecError> {
            Ok(())
        }
    }

    let format = jqf_data::FormatId::try_new(crate::jsonc::FORMAT_ID).expect("format id");
    let dialect = DialectId::try_new(dialect_text).expect("dialect");
    let request = EncodeRequest {
        format: &format,
        dialect: &dialect,
        diagnostics: DiagnosticPolicy::ErrorsOnly,
        preservation: PreservationRequest::None,
        options: None,
    };
    let mut resources = test_support::resources();
    let factory = crate::jsonc::encode::create_factory(request, &mut resources).expect("jsonc factory");
    let one = jqf_data::Value::Number(jqf_data::Number::try_json_literal("1").expect("1"));
    let two = jqf_data::Value::Number(jqf_data::Number::try_json_literal("2").expect("2"));
    let array = jqf_data::Array::try_from_vec(alloc::vec![one, two]).expect("array");
    let value = jqf_data::Value::Array(array);
    let mut session = ErasedEncoderFactory::start(
        &factory,
        EncodeItem::Owned(&value),
        PreservationRequest::None,
        &mut resources,
    )
    .expect("session");
    let mut sink = Collect(alloc::vec::Vec::new());
    let mut run = jqf_codec_core::CodecRunContext::new(&mut resources);
    run.set_cooperative_credits(4_096);
    session.encode(&mut sink, &mut run).expect("encode");
    sink.0
}

/// The encoder honors a NON-default dialect selection END TO END: through the registered factory path, an options-less
/// request naming the strict-comma output profiles writes NO trailing comma where the trailing profile writes one —
/// the dialect names the law, not the format.
#[test]
fn encode_honors_a_non_default_dialect_selection() {
    // The trailing profile keeps its comma...
    assert_eq!(encode_array_under(TRAILING_JQF_DIALECT_ID), &b"[1,2,]"[..]);
    // ...the NON-default strict-comma profiles do not.
    assert_eq!(encode_array_under(DEFAULT_JQF_DIALECT_ID), &b"[1,2]"[..]);
    assert_eq!(
        encode_array_under(JQF_1_0_DIALECT_ID),
        &b"[1,2]"[..],
        "the edit floor's dialect renders strict commas too"
    );
    // A dialect this registration does not carry is the sibling factories' mismatch rejection — never another
    // profile's bytes.
    let format = jqf_data::FormatId::try_new(FORMAT_ID).expect("format id");
    let dialect = DialectId::try_new("jsonc.bogus@9").expect("dialect");
    let request = jqf_codec_core::EncodeRequest {
        format: &format,
        dialect: &dialect,
        diagnostics: DiagnosticPolicy::ErrorsOnly,
        preservation: jqf_codec_core::PreservationRequest::None,
        options: None,
    };
    let mut resources = test_support::resources();
    assert!(
        super::encode::create_factory(request, &mut resources).is_err(),
        "a foreign jsonc dialect must be a target mismatch"
    );
}
