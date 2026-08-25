//! End-to-end YAML decode tests: registration → provider → session → document → materialized value.

use jqf_codec_core::{
    AccessFootprint, AccessGuarantees, AccessOutcome, AccessRequirement, CodecDemand, CodecError, DiagnosticPolicy,
    ErasedProvider, ExactPath, PortableStep, ValidationMode,
};
use jqf_data::{DialectId, Value, ValueKind};
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

fn resources<'a>() -> ResourceContext<'a> {
    ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
            .expect("account"),
        &ContinueControl,
        WorkMeter::try_new_v1(4096).expect("work"),
    )
    .expect("context")
}

fn source(bytes: &'static [u8]) -> ResolvedSource<'static> {
    ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "test.yaml",
        bytes,
        0,
    )
}

fn simple_request() -> jqf_codec_core::DecodeRequest<'static> {
    let dialect: &'static jqf_data::DialectId = std::boxed::Box::leak(std::boxed::Box::new(
        jqf_data::DialectId::try_new(jqf_codec_yaml::YAML_CORE_DIALECT_ID).expect("dialect"),
    ));
    jqf_codec_core::DecodeRequest {
        validation: ValidationMode::Strict,
        diagnostics: DiagnosticPolicy::ErrorsOnly,
        dialect,
        options: None,
        allow_adjacent_values: false,
        value_separator: &[],
    }
}

/// Decodes one YAML document through the whole-document route and materializes it.
fn decode(bytes: &'static [u8]) -> Result<Value, CodecError> {
    let registration = jqf_codec_yaml::registration().expect("registration");
    let decoder = registration.decoder().expect("decoder");
    let mut resources = resources();
    let mut provider: ErasedProvider = decoder.create_provider(source(bytes), simple_request(), &mut resources)?;
    let demand = CodecDemand::try_new(&resources);
    let requirement = AccessRequirement::try_whole(
        demand,
        AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
        &resources,
    )
    .expect("requirement");
    let handle = provider.bind(&requirement).expect("bind");
    let mut session = provider.open(&handle, &mut resources)?;
    let mut context = jqf_codec_core::CodecRunContext::new(&mut resources);
    let result = session.decode(&mut context)?;
    let AccessOutcome::FullDocument(product) = result.outcome() else {
        panic!("expected a full document");
    };
    let mut materialize_resources = resources;
    product
        .document()
        .materialize_root(&mut materialize_resources)
        .map_err(|_error| {
            CodecError::new(jqf_codec_core::CodecFailureKind::InternalContractViolation {
                contract: "materialize root",
            })
        })
}

/// The canonical integer text of an object member (the merge tests assert exact values, not just kinds).
fn member_int(value: &Value, key: &str) -> String {
    let Value::Object(object) = value else {
        panic!("expected object, got {:?}", value.kind());
    };
    let member = object.get(key).unwrap_or_else(|| panic!("missing key {key}"));
    let Value::Number(number) = member else {
        panic!("expected number at {key}, got {:?}", member.kind());
    };
    number.to_integer().expect("integer").as_str().to_owned()
}

fn member_str(value: &Value, key: &str) -> String {
    let Value::Object(object) = value else {
        panic!("expected object, got {:?}", value.kind());
    };
    let member = object.get(key).unwrap_or_else(|| panic!("missing key {key}"));
    let Value::String(text) = member else {
        panic!("expected string at {key}, got {:?}", member.kind());
    };
    text.as_str().to_owned()
}

#[test]
fn decodes_scalars() {
    let value = decode(b"42\n").expect("decode");
    assert_eq!(value.kind(), ValueKind::Number);
    let value = decode(b"hello\n").expect("decode");
    assert_eq!(value.kind(), ValueKind::String);
    let value = decode(b"true\n").expect("decode");
    assert_eq!(value.kind(), ValueKind::Bool);
    let value = decode(b"null\n").expect("decode");
    assert_eq!(value.kind(), ValueKind::Null);
}

#[test]
fn refuses_ill_formed_utf8_instead_of_mangling() {
    // The never-silently-mangled law for a byte-sourced format: an ill-formed scalar must REFUSE with the offset named,
    // not silently become `null` (`port: \x80\x81`), truncate at the first invalid byte (`msg: hello\x80world` →
    // `"hello"`), or lossy-replace.
    let error = decode(b"port: \x80\x81\n").expect_err("ill-formed scalar must refuse");
    assert_eq!(error.kind(), jqf_codec_core::CodecFailureKind::InvalidInput);
    assert_eq!(
        error
            .diagnostic()
            .and_then(|diagnostic| diagnostic.labels().first())
            .map(|label| label.span().start()),
        Some(6)
    );
    let error = decode(b"msg: hello\x80world\n").expect_err("mid-string byte must refuse");
    assert_eq!(error.kind(), jqf_codec_core::CodecFailureKind::InvalidInput);
    assert_eq!(
        error
            .diagnostic()
            .and_then(|diagnostic| diagnostic.labels().first())
            .map(|label| label.span().start()),
        Some(10)
    );
    // A UTF-8 BOM is still served (the BOM path validates after the skip).
    assert_eq!(decode(b"\xEF\xBB\xBFa: 1\n").expect("decode").kind(), ValueKind::Object);
    // A valid UTF-16 source still decodes (the transcoders emit valid UTF-8).
    let utf16 = b"\xFF\xFEa\x00:\x00 \x001\x00\n\x00";
    assert_eq!(decode(utf16).expect("decode").kind(), ValueKind::Object);
}

#[test]
fn decodes_block_mapping() {
    let value = decode(b"name: Ada\nage: 37\n").expect("decode");
    let Value::Object(object) = value else {
        panic!("expected object, got {:?}", value.kind());
    };
    assert_eq!(object.get("name").expect("name").kind(), ValueKind::String);
    assert_eq!(object.get("age").expect("age").kind(), ValueKind::Number);
}

#[test]
fn decodes_block_sequence() {
    let value = decode(b"- 1\n- 2\n- 3\n").expect("decode");
    let Value::Array(array) = value else {
        panic!("expected array");
    };
    assert_eq!(array.len(), 3);
}

#[test]
fn decodes_nested_and_quoted() {
    let yaml = b"server:\n  host: localhost\n  ports:\n    - 80\n    - 443\n";
    let value = decode(yaml).expect("decode");
    let Value::Object(object) = value else {
        panic!("expected object");
    };
    let server = object.get("server").expect("server");
    let Value::Object(server) = server else {
        panic!("expected nested object");
    };
    assert_eq!(server.get("host").expect("host").kind(), ValueKind::String);
    let ports = server.get("ports").expect("ports");
    let Value::Array(ports) = ports else {
        panic!("expected ports array");
    };
    assert_eq!(ports.len(), 2);
}

#[test]
fn decodes_flow_collections() {
    let value = decode(b"[1, 2, 3]\n").expect("decode");
    let Value::Array(array) = value else {
        panic!("expected array");
    };
    assert_eq!(array.len(), 3);
    let value = decode(b"{a: 1, b: 2}\n").expect("decode");
    let Value::Object(object) = value else {
        panic!("expected object");
    };
    assert_eq!(object.len(), 2);
}

#[test]
fn decodes_anchors_and_aliases() {
    let yaml = b"base: &b {x: 1}\ncopy: *b\n";
    let value = decode(yaml).expect("decode");
    let Value::Object(object) = value else {
        panic!("expected object");
    };
    let copy = object.get("copy").expect("copy");
    let Value::Object(copy) = copy else {
        panic!("expected aliased object");
    };
    assert_eq!(copy.get("x").expect("x").kind(), ValueKind::Number);
}

#[test]
fn decodes_block_scalars() {
    let yaml = "text: |\n  line one\n  line two\n";
    let value = decode(yaml.as_bytes()).expect("decode");
    let Value::Object(object) = value else {
        panic!("expected object");
    };
    let Value::String(text) = object.get("text").expect("text") else {
        panic!("expected string");
    };
    assert_eq!(text.as_str(), "line one\nline two\n");
}

#[test]
fn decodes_tags() {
    let yaml = b"!money \"10\"\n";
    let value = decode(yaml).expect("decode");
    assert_eq!(value.tag().map(jqf_data::TagId::as_str), Some("!money"));
    let yaml = b"!!str 123\n";
    let value = decode(yaml).expect("decode");
    assert_eq!(value.kind(), ValueKind::String);
}

#[test]
fn duplicate_keys_rejected() {
    let yaml = b"a: 1\na: 2\n";
    assert!(decode(yaml).is_err());
}

// --- YAML 1.1 merge keys (core schema; yaml.org/type/merge.html) ------------

#[test]
fn merges_aliased_mapping() {
    let value = decode(b"base: &b {x: 1}\nuse: {<<: *b, y: 2}\n").expect("decode");
    let Value::Object(object) = &value else {
        panic!("expected object");
    };
    let merged = object.get("use").expect("use");
    let Value::Object(entries) = merged else {
        panic!("expected merged object");
    };
    assert_eq!(entries.len(), 2, "the '<<' key itself never survives");
    assert!(entries.get("<<").is_none());
    assert_eq!(member_int(merged, "x"), "1");
    assert_eq!(member_int(merged, "y"), "2");
    // The anchored base mapping is untouched by the merge.
    let base = object.get("base").expect("base");
    let Value::Object(base) = base else {
        panic!("expected base object");
    };
    assert_eq!(base.len(), 1);
}

#[test]
fn host_keys_override_merged_entries() {
    let value = decode(b"base: &b {x: 1}\nuse: {<<: *b, x: 9}\n").expect("decode");
    let Value::Object(object) = &value else {
        panic!("expected object");
    };
    let merged = object.get("use").expect("use");
    let Value::Object(entries) = merged else {
        panic!("expected merged object");
    };
    assert_eq!(entries.len(), 1, "the host's own key wins, without duplication");
    assert_eq!(member_int(merged, "x"), "9");
}

#[test]
fn merge_sequence_earlier_items_override_later() {
    let yaml = b"a: &a {x: 1, z: 3}\nb: &b {x: 2, y: 4}\nuse: {<<: [*a, *b]}\n";
    let value = decode(yaml).expect("decode");
    let Value::Object(object) = &value else {
        panic!("expected object");
    };
    let merged = object.get("use").expect("use");
    assert_eq!(member_int(merged, "x"), "1", "the EARLIER sequence item wins");
    assert_eq!(member_int(merged, "y"), "4");
    assert_eq!(member_int(merged, "z"), "3");
}

#[test]
fn merges_chain_through_merged_mappings() {
    let yaml = b"base: &b {x: 1}\nmid: &m {<<: *b, y: 2}\nuse: {<<: *m, z: 3}\n";
    let value = decode(yaml).expect("decode");
    let Value::Object(object) = &value else {
        panic!("expected object");
    };
    let merged = object.get("use").expect("use");
    assert_eq!(
        member_int(merged, "x"),
        "1",
        "a merged-in mapping carries its own merges"
    );
    assert_eq!(member_int(merged, "y"), "2");
    assert_eq!(member_int(merged, "z"), "3");
}

#[test]
fn merges_inline_mapping_value() {
    let value = decode(b"use: {<<: {x: 1}, y: 2}\n").expect("decode");
    let Value::Object(object) = &value else {
        panic!("expected object");
    };
    let merged = object.get("use").expect("use");
    assert_eq!(member_int(merged, "x"), "1");
    assert_eq!(member_int(merged, "y"), "2");
}

#[test]
fn merge_of_non_mapping_rejected() {
    // DELIBERATE: a merge value that is neither a mapping nor a sequence of mappings is a typed decode error
    // (`merge-key` diagnostic), per the merge type's "must be a mapping or a sequence of mappings".
    assert!(decode(b"base: &b 5\nuse: {<<: *b}\n").is_err());
    assert!(decode(b"use: {<<: [1]}\n").is_err());
}

#[test]
fn duplicate_merge_keys_rejected() {
    // DELIBERATE: at most one merge key per mapping — a second `<<` is a duplicate of the first, mirroring the codec's
    // ordinary duplicate-key rejection.
    let yaml = b"a: &a {x: 1}\nb: &b {y: 2}\nuse: {<<: *a, <<: *b}\n";
    assert!(decode(yaml).is_err());
}

/// A merge-expanded entry whose KEY resolves to a non-scalar kind refuses AT the merge site under the merge-key
/// diagnostic. Merged keys are compared before any object-key check exists, so this position enforces the scalar law
/// itself instead of deferring to the build's complex-key coercion error; every refused input was already
/// unrepresentable, only the diagnostic moved earlier and named the rule.
#[test]
fn merge_expanded_non_scalar_keys_refuse_at_the_merge_site() {
    let error = decode(b"use: {<<: {[1, 2]: x}}\n").expect_err("sequence key");
    assert_eq!(error.kind(), jqf_codec_core::CodecFailureKind::InvalidInput);
    let names_the_rule = error
        .diagnostic()
        .is_some_and(|diagnostic| diagnostic.message().contains("must be a scalar"));
    assert!(names_the_rule, "expected the merge-site scalar refusal, got {error:?}");
    // An aliased key follows its target before the kind check: `*k` names a sequence, so the alias node refuses exactly
    // as the sequence would.
    let error = decode(b"k: &k [1]\nuse: {<<: {*k : x}}\n").expect_err("aliased sequence key");
    assert_eq!(error.kind(), jqf_codec_core::CodecFailureKind::InvalidInput);
    let names_the_rule = error
        .diagnostic()
        .is_some_and(|diagnostic| diagnostic.message().contains("must be a scalar"));
    assert!(names_the_rule, "expected the merge-site scalar refusal");
    // A mapping key refuses identically — including when a second merge source carries an equal one, the shape that
    // used to reach the comparator's mapping-matching arms before this refusal existed.
    let error = decode(b"use: {<<: [{ {a: 1}: x}, { {a: 1}: y }]}\n").expect_err("mapping key");
    assert_eq!(error.kind(), jqf_codec_core::CodecFailureKind::InvalidInput);
}

#[test]
fn self_merge_rejected() {
    // DELIBERATE: `<<` aliasing the (still-open) host mapping has no closed entries to merge; silence would mask the
    // cycle, so it is an error.
    assert!(decode(b"a: &a {<<: *a}\n").is_err());
}

#[test]
fn quoted_merge_key_stays_literal() {
    let value = decode(b"{'<<': 1}\n").expect("decode");
    let Value::Object(object) = &value else {
        panic!("expected object");
    };
    assert_eq!(object.len(), 1);
    assert!(object.get("<<").is_some(), "a quoted '<<' is an ordinary key");
}

#[test]
fn rejects_invalid_yaml() {
    assert!(decode(b"a: [1, 2\n").is_err());
    assert!(decode(b": bad : :\n").is_err());
}

// --- tagged-container iteration --------------------------------------------

#[test]
fn tagged_array_iterates() {
    // `.[]` over an owned tagged array must yield the elements (the payload-transparent iteration co-fix).
    let yaml = b"items: !list\n  - 1\n  - 2\n";
    let value = decode(yaml).expect("decode");
    let Value::Object(object) = value else {
        panic!("expected object");
    };
    let items = object.get("items").expect("items");
    let Value::Tagged { tag, payload } = items else {
        panic!("expected tagged payload, got {:?}", items.kind());
    };
    assert_eq!(tag.as_str(), "!list");
    let payload: &Value = payload;
    let Value::Array(array) = payload.untagged() else {
        panic!("expected array payload");
    };
    assert_eq!(array.len(), 2, "both elements present");
}

#[test]
fn tagged_object_iterates() {
    let yaml = b"m: !map\n  a: 1\n  b: 2\n";
    let value = decode(yaml).expect("decode");
    let Value::Object(object) = value else {
        panic!("expected object");
    };
    let m = object.get("m").expect("m");
    let Value::Tagged { tag, payload } = m else {
        panic!("expected tagged payload");
    };
    assert_eq!(tag.as_str(), "!map");
    let payload: &Value = payload;
    let Value::Object(payload) = payload.untagged() else {
        panic!("expected object payload");
    };
    assert_eq!(payload.len(), 2);
}

#[test]
fn tagged_descent_collects() {
    // `[..]` over a tagged container: the walk sees the tagged node and its children; the tag itself survives on the
    // container.
    let yaml = b"items: !list\n  - 1\n";
    let value = decode(yaml).expect("decode");
    let Value::Object(object) = value else {
        panic!("expected object");
    };
    let items = object.get("items").expect("items");
    assert_eq!(items.tag().map(jqf_data::TagId::as_str), Some("!list"));
}

#[test]
fn adjacent_documents_seal_their_own_segments() -> Result<(), CodecError> {
    // A multi-document YAML stream binds each document's OWN consumed extent, never the whole source — an earlier
    // document's segment must not swallow the `---` and later documents, and a later document's reopened segment keeps
    // the separator that opens it.
    let registration = jqf_codec_yaml::registration().expect("registration");
    let decoder = registration.decoder().expect("decoder");
    let mut resources = resources();
    let request = jqf_codec_core::DecodeRequest {
        validation: ValidationMode::Strict,
        diagnostics: DiagnosticPolicy::ErrorsOnly,
        dialect: &DialectId::try_new(jqf_codec_yaml::YAML_CORE_DIALECT_ID).expect("dialect"),
        options: None,
        allow_adjacent_values: true,
        value_separator: &[],
    };
    let mut provider: ErasedProvider =
        decoder.create_provider(source(b"a: 1\n---\nb: 2\n"), request, &mut resources)?;
    let demand = CodecDemand::try_new(&resources);
    let requirement = AccessRequirement::try_whole(
        demand,
        AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
        &resources,
    )
    .expect("requirement");
    let handle = provider.bind(&requirement).expect("bind");
    let mut reuse = jqf_codec_core::ReusableAccessSession::new();
    let mut offset = 0u64;
    let mut segments = Vec::new();
    loop {
        let session = provider.open_at_reusing(&handle, offset, &mut reuse, &mut resources)?;
        let mut context = jqf_codec_core::CodecRunContext::new(&mut resources);
        let result = session.decode(&mut context)?;
        let AccessOutcome::FullDocument(product) = result.outcome() else {
            panic!("expected full doc")
        };
        segments.push(
            product
                .document()
                .source_segment()
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .expect("adjacent document binds its own segment"),
        );
        let Some(consumed) = result.report().consumed_offset() else {
            break;
        };
        if consumed == 0 {
            break;
        }
        offset += consumed;
        if offset >= 12 {
            break;
        }
    }
    assert_eq!(segments, ["a: 1\n", "---\nb: 2\n"]);
    Ok(())
}

#[test]
fn crlf_literal_block_folds_breaks_to_lf() {
    let value = decode(b"a: |\r\n  x\r\n  y\r\n").expect("decode");
    assert_eq!(member_str(&value, "a"), "x\ny\n");
}

#[test]
fn cr_only_literal_block_folds_breaks_to_lf() {
    let value = decode(b"a: |\r  x\r  y\r").expect("decode");
    assert_eq!(member_str(&value, "a"), "x\ny\n");
}

#[test]
fn crlf_folded_block_joins_lines() {
    let value = decode(b"a: >\r\n  foo\r\n  bar\r\n").expect("decode");
    assert_eq!(member_str(&value, "a"), "foo bar\n");
}

#[test]
fn lf_folded_block_joins_lines() {
    let value = decode(b"a: >\n  foo\n  bar\n").expect("decode");
    assert_eq!(member_str(&value, "a"), "foo bar\n");
}

#[test]
fn folded_block_blank_line_is_a_break() {
    // Adjacent non-blank lines fold to a space; a blank line between them is a line break, not another space.
    let value = decode(b"a: >\n  foo\n\n  bar\n").expect("decode");
    assert_eq!(member_str(&value, "a"), "foo\nbar\n");
}

#[test]
fn folded_block_strip_drops_the_final_break() {
    let value = decode(b"a: >-\n  foo\n  bar\n").expect("decode");
    assert_eq!(member_str(&value, "a"), "foo bar");
}

#[test]
fn folded_block_keep_retains_trailing_breaks() {
    let value = decode(b"a: >+\n  foo\n\n").expect("decode");
    assert_eq!(member_str(&value, "a"), "foo\n\n");
}

#[test]
fn crlf_quoted_multiline_folds_to_space() {
    let value = decode(b"a: \"foo\r\n  bar\"\n").expect("decode");
    assert_eq!(member_str(&value, "a"), "foo bar");
}

#[test]
fn crlf_plain_multiline_folds_to_space() {
    let value = decode(b"a: foo\r\n  bar\n").expect("decode");
    assert_eq!(member_str(&value, "a"), "foo bar");
}

#[test]
fn single_line_crlf_value_is_unchanged() {
    let value = decode(b"a: hello\r\n").expect("decode");
    assert_eq!(member_str(&value, "a"), "hello");
}

#[test]
fn leading_zero_decimal_is_an_integer() {
    let value = decode(b"zip: 07030\n").expect("decode");
    assert_eq!(member_int(&value, "zip"), "7030");
    let value = decode(b"x: 007\n").expect("decode");
    assert_eq!(member_int(&value, "x"), "7");
}

#[test]
fn zero_padded_bigint_keeps_full_precision() {
    let value = decode(b"id: 0123456789012345678901234567890\n").expect("decode");
    assert_eq!(member_int(&value, "id"), "123456789012345678901234567890");
}

#[test]
fn underscore_and_uppercase_radix_are_strings() {
    let value = decode(b"a: 1_000\nb: 0X1F\nc: 0b101\n").expect("decode");
    assert_eq!(member_str(&value, "a"), "1_000");
    assert_eq!(member_str(&value, "b"), "0X1F");
    assert_eq!(member_str(&value, "c"), "0b101");
}

/// A `%TAG` directive as the stream's LAST bytes (no trailing line break) scans cleanly — end of input counts as the
/// directive's line break, as the reference's `IS_BREAKZ` accepts it. A DIRECTIVE-ONLY stream still fails, but at the
/// PARSER (`expected <document start>`, the same rejection the reference raises), never in the scanner's line-break
/// check.
#[test]
fn tag_directive_at_end_of_input_is_accepted() {
    // With a following document the directive applies and decoding succeeds, including when the file ends without a
    // final line break.
    let value = decode(b"%TAG !e! tag:example.com,2000:app/\n---\na: 1\n").expect("directive mid-stream");
    assert_eq!(value.kind(), ValueKind::Object);
    let value = decode(b"%TAG !e! tag:example.com,2000:app/\n---\na: 1").expect("no trailing break");
    assert_eq!(value.kind(), ValueKind::Object);
    // The directive as the last bytes: the scanner passes it through and the parser names the real problem.
    let error = decode(b"%TAG !e! tag:example.com,2000:app/").expect_err("directive-only");
    assert_eq!(error.kind(), jqf_codec_core::CodecFailureKind::InvalidInput);
    let mentions_document_start = error
        .diagnostic()
        .is_some_and(|diagnostic| diagnostic.message().contains("document start"));
    assert!(
        mentions_document_start,
        "expected the parser's document-start rejection, got {error:?}"
    );
}

/// A source ending in a partial UTF-16/32 code unit is a REFUSAL, never a silently dropped tail (the
/// refuse-don't-mangle law applied to the encoding itself).
#[test]
fn truncated_transcode_tails_are_refused() {
    let odd_utf16 = b"\xFF\xFEa\x00:\x00 \x001\x00\n\x00B";
    let error = decode(odd_utf16).expect_err("odd UTF-16 tail");
    assert!(matches!(error.kind(), jqf_codec_core::CodecFailureKind::InvalidInput));
    let short_utf32 = b"\x00\x00\xFE\xFFa\x00\x00\x00:\x00";
    let error = decode(short_utf32).expect_err("truncated UTF-32 tail");
    assert!(matches!(error.kind(), jqf_codec_core::CodecFailureKind::InvalidInput));
    // A complete even-length UTF-16 document still decodes.
    let value = decode(b"\xFF\xFEa\x00:\x00 \x001\x00\n\x00").expect("complete utf16");
    assert_eq!(value.kind(), ValueKind::Object);
}

/// An explicit `!!null` resolves like the implicit one: only the null spellings are nulls, any other content under the
/// tag is a schema error — never a silently published null.
#[test]
fn explicit_null_tag_rejects_non_null_content() {
    let error = decode(b"a: !!null hello\n").expect_err("text under !!null");
    assert!(matches!(error.kind(), jqf_codec_core::CodecFailureKind::InvalidInput));
    let error = decode(b"a: !!null \"x\"\n").expect_err("quoted text under !!null");
    assert!(matches!(error.kind(), jqf_codec_core::CodecFailureKind::InvalidInput));
    // The null spellings themselves stay legal, including an EMPTY value.
    let value = decode(b"a: !!null ~\nb: !!null null\nc: !!null\n").expect("null spellings");
    assert_eq!(value.kind(), ValueKind::Object);
}

// --- the Exact/Located scoped route -----------------------------------------

/// Decodes one located subtree through the Exact route (the same registration → provider → bind path as [`decode`],
/// with an exact-path requirement instead of a whole one).
fn decode_located(
    bytes: &'static [u8],
    steps: &[PortableStep],
) -> Result<jqf_codec_core::AccessResult<'static>, CodecError> {
    let registration = jqf_codec_yaml::registration().expect("registration");
    let decoder = registration.decoder().expect("decoder");
    let mut resources = resources();
    let mut provider: ErasedProvider = decoder.create_provider(source(bytes), simple_request(), &mut resources)?;
    let demand = CodecDemand::try_new(&resources);
    let mut path = ExactPath::try_new(&resources);
    for step in steps {
        match step {
            PortableStep::SemanticMember(name) => {
                path.try_push_semantic_member(name, &resources).expect("member");
            }
            PortableStep::SemanticIndex(index) => path.try_push_semantic_index(*index, &resources),
            PortableStep::SemanticRange { start, end } => {
                path.try_push_semantic_range(*start, *end, &resources);
            }
        }
    }
    let footprint = AccessFootprint::try_exact(path, &resources);
    let requirement = AccessRequirement::try_exact(
        footprint,
        demand,
        AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
        &resources,
    )
    .expect("requirement");
    let handle = provider.bind(&requirement).expect("bind");
    let mut session = provider.open(&handle, &mut resources)?;
    let mut context = jqf_codec_core::CodecRunContext::new(&mut resources);
    session.decode(&mut context)
}

/// The merge-site scalar refusal is a PARSER refusal, so the Exact/Located route — which shares that parser — refuses
/// the same input with the same diagnostic class. A whole-document-only enforcement would let the scoped route diverge
/// here.
#[test]
fn scoped_route_agrees_on_merge_expanded_sequence_key_refusal() {
    for yaml in [
        b"use: {<<: {[1, 2]: x}}\n".as_slice(),
        b"k: &k [1]\nuse: {<<: {*k : x}}\n",
        b"use: {<<: [{ {a: 1}: x}, { {a: 1}: y }]}\n",
    ] {
        let error = decode_located(yaml, &[PortableStep::SemanticMember("use".to_owned())])
            .expect_err("scoped route must refuse with the whole-document route");
        assert_eq!(error.kind(), jqf_codec_core::CodecFailureKind::InvalidInput);
        let names_the_rule = error
            .diagnostic()
            .is_some_and(|diagnostic| diagnostic.message().contains("must be a scalar"));
        assert!(names_the_rule, "expected the merge-site scalar refusal");
    }
}

#[test]
fn output_profile_on_decode_is_a_requirement_mismatch() {
    let dialect: &'static DialectId = std::boxed::Box::leak(std::boxed::Box::new(
        DialectId::try_new(jqf_codec_yaml::YAML_BLOCK_DIALECT_ID).expect("dialect"),
    ));
    let request = jqf_codec_core::DecodeRequest {
        validation: ValidationMode::Strict,
        diagnostics: DiagnosticPolicy::ErrorsOnly,
        dialect,
        options: None,
        allow_adjacent_values: false,
        value_separator: &[],
    };
    let registration = jqf_codec_yaml::registration().expect("registration");
    let decoder = registration.decoder().expect("decoder");
    let mut resources = resources();
    let error = decoder
        .create_provider(source(b"a: 1\n"), request, &mut resources)
        .expect_err("output profile cannot decode");
    assert_eq!(error.kind(), jqf_codec_core::CodecFailureKind::RequirementMismatch);
}

#[test]
fn unpaired_utf16_surrogate_refuses() {
    // UTF-16BE BOM + unpaired high surrogate. Truncated units already refuse; ill-formed scalar units must not become
    // U+FFFD.
    let error = decode(b"\xFE\xFF\xD8\x00").expect_err("unpaired surrogate must refuse");
    assert_eq!(error.kind(), jqf_codec_core::CodecFailureKind::InvalidInput);
}

#[test]
fn consecutive_document_end_markers_decode() {
    let docs = {
        let mut resources = resources();
        jqf_codec_yaml::decode_documents(source(b"...\n...\n...\na\n"), &mut resources).expect("decode")
    };
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0].kind(), ValueKind::String);
}
