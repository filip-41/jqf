//! JSON5 decode/encode/comment-fact tests.

use alloc::boxed::Box;
use jqf_codec_core::{AccessOutcome, CodecRunContext, DecodeRequest, DiagnosticPolicy, ValidationMode};
use jqf_data::{DialectId, FormatId, LocalOwnerRef, ReaderPoll};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

use crate::test_support::{self, requirement, requirement_preserving_facts};

use super::{DOCUMENT_DIALECT_ID, registration};

fn source(bytes: &'static [u8]) -> ResolvedSource<'static> {
    ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "json5-test",
        bytes,
        0,
    )
}

/// Runs one decode; `Ok(())` when it completes as a full document. The error carries the failure kind plus the
/// structured diagnostic's message, so an unexpected rejection names its reason instead of erasing it.
fn decode_ok(bytes: &'static [u8]) -> Result<(), alloc::string::String> {
    let mut resources = test_support::resources();
    let dialect = DialectId::try_new(DOCUMENT_DIALECT_ID).expect("dialect");
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
    match result {
        Ok(result) => match result.into_parts().0 {
            AccessOutcome::FullDocument(_) => Ok(()),
            AccessOutcome::Located(_) => Err(alloc::string::String::from(
                "expected a full document, got a located one",
            )),
        },
        Err(error) => Err(match error.diagnostic() {
            Some(diagnostic) => {
                alloc::format!("{:?}: {}", error.kind(), diagnostic.message())
            }
            None => alloc::format!("{:?}", error.kind()),
        }),
    }
}

/// Decodes and returns the materialized root value's Debug text, or panics.
fn decode_value(bytes: &'static [u8]) -> alloc::string::String {
    decode_value_through(registration(), DOCUMENT_DIALECT_ID, bytes)
}

/// The same materialized shape, read through the STRICT JSON codec: the reference answer a JSON5 decode of a strict
/// document must equal.
fn decode_value_strict(bytes: &'static [u8]) -> alloc::string::String {
    decode_value_through(crate::registration::registration(), crate::RFC8259_DIALECT_ID, bytes)
}

fn decode_value_through(
    registration: Result<jqf_codec_core::CodecRegistration<'static>, jqf_codec_core::RegistrationError>,
    dialect_id: &str,
    bytes: &'static [u8],
) -> alloc::string::String {
    let mut resources = test_support::resources();
    let dialect = DialectId::try_new(dialect_id).expect("dialect");
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
            &mut resources,
        )
        .expect("provider");
    let requirement = requirement(&resources);
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
        .expect("materialize");
    alloc::format!("{value:?}")
}

/// The JSON5 grammar surface: unquoted keys, single quotes, hex, leading and trailing decimal points, `+`,
/// `Infinity`/`NaN`, comments, and trailing commas all decode.
#[test]
fn json5_grammar_features_decode() {
    // Unquoted keys + single-quoted string values + comments + a trailing comma, against the strict spelling of the
    // same document.
    let extended = decode_value(b"{\n  // lead\n  name: 'ada',\n  id: 7,\n}\n");
    let strict = decode_value(b"{\"name\":\"ada\",\"id\":7}");
    assert_eq!(extended, strict);
    // Hex integers are exact Integers.
    assert_eq!(decode_value(b"0x10"), decode_value(b"16"));
    assert_eq!(decode_value(b"-0xff"), decode_value(b"-255"));
    // Leading/trailing decimal points and an explicit `+`.
    assert_eq!(decode_value(b".5"), decode_value(b"0.5"));
    assert!(decode_ok(b"1.").is_ok(), "`1.` must decode (JSON5 trailing point)");
    assert_eq!(decode_value(b"+7"), decode_value(b"7"));
    assert!(
        decode_ok(b"1.e2").is_ok(),
        "`1.e2` must decode (JSON5 exponent after bare point)"
    );
    // Infinity/NaN are three DISTINCT non-finite values, none of them a finite number and none of them each other.
    assert!(decode_ok(b"-Infinity").is_ok());
    assert!(decode_ok(b"NaN").is_ok());
    let non_finite = [
        decode_value(b"Infinity"),
        decode_value(b"-Infinity"),
        decode_value(b"NaN"),
    ];
    assert_ne!(non_finite[0], non_finite[1]);
    assert_ne!(non_finite[0], non_finite[2]);
    assert_ne!(non_finite[1], non_finite[2]);
    for value in &non_finite {
        assert_ne!(*value, decode_value(b"1"), "a non-finite is not a number");
    }
    // A strict JSON document decodes to what the STRICT codec decodes.
    assert_eq!(
        decode_value(b"{\"a\":[1,2,true,null]}"),
        decode_value_strict(b"{\"a\":[1,2,true,null]}")
    );
}

/// The UTF-8 byte-order mark is JSON5 whitespace (`consume_bom: false` in the provider rides entirely on the grammar's
/// whitespace set accepting U+FEFF), so a BOM-marked document decodes.
#[test]
fn a_byte_order_mark_is_whitespace() {
    let marked = b"\xef\xbb\xbf{'a':1}";
    assert_eq!(decode_value(marked), decode_value(b"{'a':1}"));
}

/// The JSON5 escape arms: `\x`, `\0`, `\'`, and line continuations.
#[test]
fn json5_escapes_decode() {
    assert_eq!(decode_value(b"'\\x41'"), decode_value(b"\"A\""));
    assert_eq!(decode_value(b"'\\0'"), decode_value(b"'\\u0000'"));
    assert_eq!(decode_value(b"'it\\'s'"), decode_value(b"\"it's\""));
    assert_eq!(decode_value(b"'a\\\nb'"), decode_value(b"\"ab\""));
    // `\<CR><LF>` and `\<U+2028>`/`\<U+2029>` are continuations too (ES5 counts the line separators as terminators).
    assert_eq!(decode_value(b"'a\\\r\nb'"), decode_value(b"\"ab\""));
    assert_eq!(decode_value("'a\\\u{2028}b'".as_bytes()), decode_value(b"\"ab\""));
    assert_eq!(decode_value("'a\\\u{2029}b'".as_bytes()), decode_value(b"\"ab\""));
    // `\01` is invalid (JSON5 forbids octal-looking `\0`).
    assert!(decode_ok(b"'\\01'").is_err(), "`\\01` must be rejected");
    // A backslash before a NON-terminator byte that is not an escape stays rejected.
    assert!(decode_ok(b"'a\\xb'").is_err(), "`\\x` short form is not JSON5");
}

/// Strict JSON documents decode through the json5 dialect (JSON ⊂ JSON5, the conformance corpus's first stand-in).
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
    ] {
        assert!(
            decode_ok(bytes).is_ok(),
            "strict document must decode under json5: {bytes:?}"
        );
    }
}

/// An unterminated block comment is a clean rejection, never a hang.
#[test]
fn unterminated_comment_is_rejected() {
    assert!(decode_ok(b"{\"a\": 1 /* never closes}").is_err());
}

/// The leading comment attaches to the VALUE node of the member it precedes as a `json5.comment@1` fact.
#[test]
fn comments_attach_as_leading_facts() {
    let bytes = b"{\n  // name comment\n  name: 'ada',\n  // trailer\n}\n";
    let mut resources = test_support::resources();
    let dialect = DialectId::try_new(DOCUMENT_DIALECT_ID).expect("dialect");
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
    let requirement = requirement_preserving_facts(&resources);
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
    let document = product.document();
    let limit = jqf_data::BatchLimit::new(usize::MAX).expect("limit");
    let mut reader = document.fact_reader(&mut resources).expect("reader");
    let mut found = alloc::vec::Vec::new();
    loop {
        match reader.poll_batch(limit, &mut resources).expect("poll") {
            ReaderPoll::Batch(batch) => {
                for fact in batch.iter() {
                    let LocalOwnerRef::Node(node) = fact.owner() else {
                        continue;
                    };
                    if fact.role().as_str() != "json5.comment@1" {
                        continue;
                    }
                    let jqf_data::FactPayloadView::List(texts) = fact.payload() else {
                        continue;
                    };
                    let lines: alloc::vec::Vec<_> = texts
                        .iter()
                        .filter_map(|entry| match entry {
                            jqf_data::FactPayloadView::Text(text) => Some(alloc::string::String::from(text)),
                            _ => None,
                        })
                        .collect();
                    found.push((node, lines));
                }
            }
            ReaderPoll::Pending => {
                resources.try_begin_next_cooperative_entry(4_096).expect("resume");
            }
            ReaderPoll::End(_) => break,
        }
    }
    assert_eq!(found.len(), 2, "facts: {found:?}");
    assert_eq!(found[0].1, alloc::vec![alloc::string::String::from("name comment")]);
    // The member comment's owner is not the root; the trailer attaches to the root (the TOML trailer law).
    assert_ne!(found[0].0, document.root());
    assert_eq!(found[1].0, document.root());
}

/// The append splice never orphans a trailing comment: the new member lands BEFORE the comment block, which stays a
/// trailer.
#[test]
fn append_splice_preserves_trailing_comment() {
    let bytes = b"{\n  \"a\": 1,\n  // trailing\n}\n";
    let mut resources = test_support::resources();
    let dialect = DialectId::try_new(DOCUMENT_DIALECT_ID).expect("dialect");
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
        session.decode(&mut run).expect("decode")
    };
    let (outcome, _) = result.into_parts();
    let AccessOutcome::FullDocument(product) = outcome else {
        panic!("expected full document")
    };
    let document = product.document();
    let root = document.root();
    let two = jqf_data::Number::try_json_literal("2").expect("2");
    let two = jqf_data::Value::Number(two);
    let members = jqf_codec_core::EditAppendMembers::Table(&[("x", &two)]);
    let insertions =
        super::super::jsonc::encode::jsonc_render_edit_append(document, root, bytes, members, &mut resources)
            .expect("append");
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
    let after = text.find("\"x\"").expect("new member present");
    let comment = text.find("// trailing").expect("comment present");
    assert!(after < comment, "new member lands before the comment: {text}");
}

/// JSON5's one rendering never writes a trailing comma, however the input spelled its own — the factory's one
/// behavioral difference from the JSONC encoder. The flag rode entirely untested (compact CLI coverage cannot show it),
/// so a flip shipped silently.
#[test]
fn json5_encode_never_writes_a_trailing_comma() {
    use jqf_codec_core::{
        ByteSink, DiagnosticPolicy, EncodeItem, EncodeRequest, ErasedEncoderFactory, PreservationRequest,
    };

    struct Collect(alloc::vec::Vec<u8>);
    impl ByteSink for Collect {
        fn write(
            &mut self,
            bytes: &[u8],
            _resources: &mut jqf_resource::ResourceContext<'_>,
        ) -> Result<usize, jqf_codec_core::CodecError> {
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> Result<(), jqf_codec_core::CodecError> {
            Ok(())
        }
    }

    let format = FormatId::try_new(super::FORMAT_ID).expect("format id");
    let dialect = DialectId::try_new(super::DOCUMENT_DIALECT_ID).expect("dialect");
    let request = EncodeRequest {
        format: &format,
        dialect: &dialect,
        diagnostics: DiagnosticPolicy::ErrorsOnly,
        preservation: PreservationRequest::None,
        options: None,
    };
    let mut resources = crate::test_support::resources();
    let factory = super::encode::create_factory(request, &mut resources).expect("json5 factory");
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
    let text = alloc::string::String::from_utf8(sink.0).expect("utf8");
    assert!(
        !text.contains(",]") && !text.contains(",}"),
        "no trailing comma ever: {text}"
    );
}

/// A tag-validation request targeting a `JSON5` profile opens THIS codec's validator and answers the `NoTags` law
/// (empty set valid, any tag invalid). The shared strict-JSON factory would refuse the same request as a foreign target
/// — the registration names its own factory, and this pins it.
#[test]
fn tag_validator_answers_the_no_tags_law_for_json5_targets() {
    use jqf_codec_core::{DiagnosticPolicy, EncodeRequest, PreservationRequest};
    use jqf_data::TagId;

    for dialect_text in super::DIALECT_TEXTS {
        let format = FormatId::try_new(super::FORMAT_ID).expect("format id");
        let dialect = DialectId::try_new(dialect_text).expect("dialect");
        let request = EncodeRequest {
            format: &format,
            dialect: &dialect,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            preservation: PreservationRequest::None,
            options: None,
        };
        let mut resources = test_support::resources();
        let validator = crate::tag::create_json5_validator(request, &mut resources)
            .expect("a JSON5 target opens the json5 validator");
        validator.validate(&[], &resources).expect("empty set valid");
        let tag = TagId::try_new_unaccounted("!money").expect("tag");
        assert!(
            validator.validate(&[&tag], &resources).is_err(),
            "{dialect_text}: a non-core tag is invalid for JSON5 output"
        );
    }
}

/// A hex integer is grammar-valid at any length, but the exact-conversion cap (`MAX_HEX_DIGITS`, 4096) refuses beyond
/// it on every route — one decode refusal naming the hex bound, never a silent rounding step.
#[test]
fn hex_literal_past_the_exact_conversion_cap_is_rejected() {
    let mut literal = alloc::vec::Vec::from(&b"0x"[..]);
    literal.extend(alloc::vec![b'7'; crate::lex::MAX_HEX_DIGITS + 1]);
    let error = decode_ok(Box::leak(literal.into_boxed_slice())).expect_err("past the cap");
    assert!(
        error.contains("hex literal"),
        "the refusal must name the hex bound: {error}"
    );
}

/// `{null:1}` under the unquoted-key arm: an `UnquotedKey` is an ECMAScript 5.1 `IdentifierName`, whose grammar
/// INCLUDES the reserved words (only `Identifier` excludes them), and this parser arms exactly that run — so a
/// reserved-word key is accepted as its own key text, not rejected.
#[test]
fn reserved_word_unquoted_key_is_its_own_text() {
    assert_eq!(decode_value(b"{null:1}"), decode_value(b"{\"null\":1}"));
}

/// A complete non-finite spelling butted against a non-boundary byte is ONE malformed token (`nanx`), never NaN
/// followed by junk — the value-boundary law the family shares with the bare literals' `nullx`.
#[test]
fn nonfinite_spelling_butted_against_a_byte_is_one_malformed_token() {
    for bytes in [&b"nanx"[..], b"snanx", b"infx", b"infinityx", b"nan1"] {
        let error = decode_ok(bytes).expect_err("one malformed token");
        assert!(!error.is_empty(), "{bytes:?}: the reason must be surfaced");
    }
}

/// Trailing commas are JSON5 grammar in BOTH containers; the array form rides the same acceptance the object form
/// already pinned.
#[test]
fn array_trailing_comma_is_json5_grammar() {
    assert!(decode_ok(b"[1, 2,]").is_ok());
    assert_eq!(decode_value(b"[1, 2,]"), decode_value(b"[1,2]"));
}

/// U+2028/U+2029 join the JSON5 whitespace set beside U+FEFF: each separates tokens wherever RFC 8259 whitespace may
/// stand.
#[test]
fn line_separator_whitespace_is_accepted() {
    assert_eq!(
        decode_value("{\u{2028}\"a\": [1, 2]\u{2029}}".as_bytes()),
        decode_value(b"{\"a\":[1,2]}")
    );
}

/// Pinning the non-finite family's REALITY as the shared arms define it: every casing of `nan`, `snan`, `inf`,
/// `infinity` decodes, signed forms included; every NaN spelling (`snan` included) lands on the ONE fixed positive
/// quiet-NaN bit pattern regardless of sign, while the infinity spellings are sign-sensitive and distinct from NaN.
#[test]
fn nonfinite_spelling_breadth_is_case_insensitive_and_sign_aware() {
    for bytes in [
        &b"nan"[..],
        b"NaN",
        b"NAN",
        b"nAn",
        b"snan",
        b"sNaN",
        b"SNAN",
        b"Snan",
        b"inf",
        b"INF",
        b"Infinity",
        b"iNFiNiTy",
        b"INFINITY",
        b"-nan",
        b"+nan",
        b"-sNaN",
        b"+Infinity",
        b"-INFINITY",
    ] {
        assert!(decode_ok(bytes).is_ok(), "{bytes:?} must decode");
    }
    // One NaN value across every spelling.
    assert_eq!(decode_value(b"nan"), decode_value(b"NAN"));
    assert_eq!(decode_value(b"nan"), decode_value(b"sNaN"));
    assert_eq!(decode_value(b"nan"), decode_value(b"-SNAN"));
    // Infinities are sign-sensitive and not NaN.
    assert_eq!(decode_value(b"inf"), decode_value(b"+Infinity"));
    assert_eq!(decode_value(b"-inf"), decode_value(b"-INFINITY"));
    assert_ne!(decode_value(b"inf"), decode_value(b"-inf"));
    assert_ne!(decode_value(b"inf"), decode_value(b"nan"));
}

/// The `json5.jqf@1` request path renders CANONICAL JSON: quoted keys, decimal numbers, no trailing commas — so its
/// exact bytes re-decode through the STRICT codec to exactly the value the JSON5 decode produced.
#[test]
fn encode_via_the_jqf_dialect_round_trips_canonical_json() {
    use jqf_codec_core::{
        ByteSink, DiagnosticPolicy, EncodeItem, EncodeRequest, ErasedEncoderFactory, PreservationRequest,
    };

    struct Collect(alloc::vec::Vec<u8>);
    impl ByteSink for Collect {
        fn write(
            &mut self,
            bytes: &[u8],
            _resources: &mut jqf_resource::ResourceContext<'_>,
        ) -> Result<usize, jqf_codec_core::CodecError> {
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> Result<(), jqf_codec_core::CodecError> {
            Ok(())
        }
    }

    // Extended spellings in: unquoted + reserved-adjacent key, single quotes, a hex integer, a trailing comma.
    let source_bytes = "{name: 'ada', id: 0x10,}".as_bytes();

    let format = FormatId::try_new(super::FORMAT_ID).expect("format id");
    let dialect = DialectId::try_new(super::JQF_DIALECT_ID).expect("dialect");
    let request = EncodeRequest {
        format: &format,
        dialect: &dialect,
        diagnostics: DiagnosticPolicy::ErrorsOnly,
        preservation: PreservationRequest::None,
        options: None,
    };
    let mut resources = crate::test_support::resources();
    let factory = super::encode::create_factory(request, &mut resources).expect("json5 factory");
    let name = jqf_data::Shared::try_from_str("ada").expect("shared str");
    let mut builder = jqf_data::ObjectBuilder::try_with_capacity(2).expect("builder");
    builder
        .try_insert_last(
            jqf_data::ObjectKey::try_from_str("name").expect("key"),
            jqf_data::Value::String(name),
        )
        .expect("insert");
    builder
        .try_insert_last(
            jqf_data::ObjectKey::try_from_str("id").expect("key"),
            jqf_data::Value::Number(jqf_data::Number::try_json_literal("16").expect("16")),
        )
        .expect("insert");
    let value = jqf_data::Value::Object(builder.try_finish().expect("object"));
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
    assert_eq!(sink.0, &b"{\"name\":\"ada\",\"id\":16}"[..]);
    // The canonical bytes are strict-JSON-decodable to the same value.
    let encoded: &'static [u8] = Box::leak(sink.0.into_boxed_slice());
    assert_eq!(decode_value(source_bytes), decode_value_strict(encoded));
}
