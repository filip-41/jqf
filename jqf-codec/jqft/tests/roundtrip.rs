//! jqft / jqfjson round-trip tests: decode → canonical encode → decode keeps the semantic value, byte-for-byte on the
//! canonical spelling.

use jqf_codec_core::{
    AccessGuarantees, AccessOutcome, AccessRequirement, CodecDemand, CodecFailureKind, CodecRunContext, DecodeRequest,
    DiagnosticPolicy, EncodeItem, EncodeRequest, PreservationRequest, ValidationMode,
};
use jqf_data::{DialectId, FormatId, Value};
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

static CONTROL: ContinueControl = ContinueControl;

fn resources() -> ResourceContext<'static> {
    ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
            .expect("account"),
        &CONTROL,
        WorkMeter::try_new_v1(4_096).expect("work"),
    )
    .expect("resources")
}

fn source(bytes: &[u8]) -> ResolvedSource<'_> {
    ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "roundtrip.jqft",
        bytes,
        0,
    )
}

/// Decodes one document through the whole-document route, returning the materialized root value or the failure kind.
fn whole_value(bytes: &[u8], format: &str, dialect: &str) -> Result<Value, CodecFailureKind> {
    let registration = if format == jqf_codec_jqft::FORMAT_ID {
        jqf_codec_jqft::registration_jqft().map_err(|_e| CodecFailureKind::InternalContractViolation {
            contract: "jqft registration",
        })?
    } else {
        jqf_codec_jqft::registration_jqfjson().map_err(|_e| CodecFailureKind::InternalContractViolation {
            contract: "jqfjson registration",
        })?
    };
    let mut resources = resources();
    let source = source(bytes);
    let mut provider = registration
        .decoder()
        .expect("decoder")
        .create_provider(
            source,
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(dialect).expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            &mut resources,
        )
        .map_err(|error| error.kind())?;
    let demand = CodecDemand::try_new(&resources);
    let requirement = AccessRequirement::try_whole(
        demand,
        AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
        &resources,
    )
    .map_err(|_| CodecFailureKind::Overflow)?;
    let handle = provider
        .bind(&requirement)
        .map_err(|_| CodecFailureKind::RequirementMismatch)?;
    let mut session = provider.open(&handle, &mut resources).map_err(|error| error.kind())?;
    let mut context = CodecRunContext::new(&mut resources);
    context.set_cooperative_credits(4_096);
    let result = session.decode(&mut context).map_err(|error| error.kind())?;
    let AccessOutcome::FullDocument(product) = result.into_parts().0 else {
        return Err(CodecFailureKind::RequirementMismatch);
    };
    product
        .document()
        .materialize_root(&mut resources)
        .map_err(|_| CodecFailureKind::UnsupportedRepresentation)
}

/// Encodes one owned value, driving the encoder session to its finished bytes.
fn encode_value(value: &Value, format: &str, dialect: &str) -> Result<Vec<u8>, CodecFailureKind> {
    let registration = if format == jqf_codec_jqft::FORMAT_ID {
        jqf_codec_jqft::registration_jqft().map_err(|_e| CodecFailureKind::InternalContractViolation {
            contract: "jqft registration",
        })?
    } else {
        jqf_codec_jqft::registration_jqfjson().map_err(|_e| CodecFailureKind::InternalContractViolation {
            contract: "jqfjson registration",
        })?
    };
    let mut resources = resources();
    let request = EncodeRequest {
        format: &FormatId::try_new(format).map_err(|_| CodecFailureKind::RequirementMismatch)?,
        dialect: &DialectId::try_new(dialect).map_err(|_| CodecFailureKind::RequirementMismatch)?,
        diagnostics: DiagnosticPolicy::ErrorsOnly,
        preservation: PreservationRequest::None,
        options: None,
    };
    let factory = registration
        .encoder()
        .expect("encoder")
        .create_factory(request, &mut resources)
        .map_err(|error| error.kind())?;
    let mut session = factory
        .start(EncodeItem::owned(value), PreservationRequest::None, &mut resources)
        .map_err(|error| error.kind())?;
    let mut out = Vec::new();
    {
        let mut sink = jqf_codec_core::VecByteSink::new(&mut out);
        let mut context = CodecRunContext::new(&mut resources);
        context.set_cooperative_credits(4_096);
        session.encode(&mut sink, &mut context).map_err(|error| error.kind())?;
    }
    Ok(out)
}

/// A deterministic render used to compare values semantically.
fn render(value: &Value) -> String {
    use jqf_data::Value as V;
    match value {
        V::Null => "null".into(),
        V::Bool(true) => "true".into(),
        V::Bool(false) => "false".into(),
        V::Number(number) => {
            if let Some(integer) = number.to_integer() {
                integer.as_str().into()
            } else if let Some(float) = number.as_float() {
                format!("f{}", float.get())
            } else if let Some(decimal) = number.as_decimal() {
                format!("d{}/{}", decimal.coefficient().as_str(), decimal.scale())
            } else {
                "?".into()
            }
        }
        V::String(text) => format!("s{text:?}"),
        V::Bytes(bytes) => format!("h{:?}", bytes.as_ref()),
        V::LocalDate(date) => format!("ld{}-{:02}-{:02}", date.year(), date.month(), date.day()),
        V::LocalTime(time) => format!("lt{}:{:02}:{:02}", time.hour(), time.minute(), time.second()),
        V::LocalDateTime(datetime) => format!(
            "ldt{}-{:02}-{:02}T{}:{:02}:{:02}",
            datetime.date.year(),
            datetime.date.month(),
            datetime.date.day(),
            datetime.time.hour(),
            datetime.time.minute(),
            datetime.time.second()
        ),
        V::OffsetDateTime(datetime) => format!(
            "odt{}-{:02}-{:02}T{}:{:02}:{:02}",
            datetime.local.date.year(),
            datetime.local.date.month(),
            datetime.local.date.day(),
            datetime.local.time.hour(),
            datetime.local.time.minute(),
            datetime.local.time.second()
        ),
        V::Tagged { tag, payload } => format!("@{} {}", tag.as_str(), render(payload)),
        V::Array(array) => {
            let items: Vec<String> = array.iter().map(render).collect();
            format!("[{}]", items.join(","))
        }
        V::Object(object) => {
            let entries: Vec<String> = object
                .iter()
                .map(|entry| format!("{}:{}", entry.key(), render(entry.value())))
                .collect();
            format!("{{{}}}", entries.join(","))
        }
    }
}

fn jqft_roundtrip(bytes: &[u8]) {
    let first = whole_value(bytes, "jqft", "jqft.document@1")
        .unwrap_or_else(|k| panic!("decode {k:?} for {:?}", String::from_utf8_lossy(bytes)));
    let encoded = encode_value(&first, "jqft", "jqft.canonical@1").expect("encode");
    let second = whole_value(&encoded, "jqft", "jqft.document@1").expect("re-decode");
    assert_eq!(
        render(&first),
        render(&second),
        "jqft round-trip drifted for {bytes:?} -> {}",
        String::from_utf8_lossy(&encoded)
    );
}

/// Canonical jqft/jqfjson output is self-inverse over the exact-number lattice: every probe decodes, encodes, and
/// re-decodes to the same value, including the zero-integer-part decimals and `-0.0` the builder used to reject, and
/// the integer-valued decimal whose encoder used to emit scientific notation.
#[test]
fn exact_number_lattice_is_self_inverse_on_jqft_and_jqfjson() {
    let jqft_probes: [&[u8]; 6] = [
        b"%jqft 1\n0.85\n",
        b"%jqft 1\n0.05\n",
        b"%jqft 1\n0.1\n",
        b"%jqft 1\n-0.0\n",
        b"%jqft 1\n100.0\n",
        b"%jqft 1\n1.05\n",
    ];
    for case in jqft_probes {
        jqft_roundtrip(case);
    }

    let jqfjson_probes: [&[u8]; 6] = [b"0.85\n", b"0.05\n", b"0.1\n", b"-0.0\n", b"100.0\n", b"1.05\n"];
    for bytes in jqfjson_probes {
        let first = whole_value(bytes, "jqfjson", "jqfjson.document@1")
            .unwrap_or_else(|k| panic!("jqfjson decode {k:?} for {:?}", String::from_utf8_lossy(bytes)));
        let encoded = encode_value(&first, "jqfjson", "jqfjson.canonical@1").expect("encode");
        let second = whole_value(&encoded, "jqfjson", "jqfjson.document@1").expect("re-decode");
        assert_eq!(
            render(&first),
            render(&second),
            "jqfjson round-trip drifted for {bytes:?} -> {}",
            String::from_utf8_lossy(&encoded)
        );
    }

    let value = whole_value(b"%jqft 1\n100.0\n", "jqft", "jqft.document@1").expect("decode");
    let encoded = encode_value(&value, "jqft", "jqft.canonical@1").expect("encode");
    let text = String::from_utf8(encoded).expect("utf8");
    assert!(
        text.contains("100.0"),
        "canonical output must be a plain decimal, got {text:?}"
    );
    assert!(
        !text.contains('E') && !text.contains('e'),
        "canonical output must not use scientific notation, got {text:?}"
    );
}

#[test]
fn core_values_round_trip() {
    let cases: [&[u8]; 9] = [
        b"%jqft 1\nnull\n",
        b"%jqft 1\ntrue\n",
        b"%jqft 1\n42\n",
        b"%jqft 1\n-7\n",
        b"%jqft 1\n1250.00\n",
        b"%jqft 1\n1e3\n",
        b"%jqft 1\n\"hello, \\\"world\\\"\"\n",
        b"%jqft 1\n[1, 2, [true, null]]\n",
        b"%jqft 1\n{name: \"ada\", id: 1, tags: [\"a\", \"b\"]}\n",
    ];
    for case in cases {
        jqft_roundtrip(case);
    }
}

#[test]
fn floats_bytes_temporals_round_trip() {
    jqft_roundtrip(b"%jqft 1\n2.5f\n");
    jqft_roundtrip(b"%jqft 1\ninf\n");
    jqft_roundtrip(b"%jqft 1\n-0f\n");
    jqft_roundtrip(b"%jqft 1\n0x\"9f86d081884c7d65\"\n");
    jqft_roundtrip(b"%jqft 1\nb64\"aGVsbG8gd29ybGQ=\"\n");
    jqft_roundtrip(b"%jqft 1\n2026-08-02T21:14:00+02:00\n");
    jqft_roundtrip(b"%jqft 1\n2026-08-02T19:02:11\n");
    jqft_roundtrip(b"%jqft 1\n2026-08-02\n");
    jqft_roundtrip(b"%jqft 1\n02:00:00\n");
}

#[test]
fn tags_round_trip() {
    jqft_roundtrip(b"%jqft 1\n@tag(\"uuid\") \"0198c5b2-7e01-7c3a\"\n");
    jqft_roundtrip(b"%jqft 1\n@tag(\"outer\") @tag(\"inner\") [1, 2]\n");
    jqft_roundtrip(b"%jqft 1\n{id: @tag(\"uuid\") \"0198\", total: 12.5}\n");
    // A tag wraps the CONTAINER that follows it, never a container's elements.
    let value = whole_value(
        b"%jqft 1\n{chain: @tag(\"a\") @tag(\"b\") [1, 2]}\n",
        "jqft",
        "jqft.document@1",
    )
    .expect("decode");
    let Value::Object(object) = &value else {
        panic!("expected an object");
    };
    let chain = object.iter().next().expect("one entry").value();
    let Value::Tagged { tag, payload } = chain else {
        panic!("the chain must be tagged");
    };
    assert_eq!(tag.as_str(), "a");
    let Value::Tagged { tag, payload } = &**payload else {
        panic!("the inner layer must be tagged");
    };
    assert_eq!(tag.as_str(), "b");
    assert!(matches!(&**payload, Value::Array(_)));
}

#[test]
fn bare_keys_round_trip() {
    jqft_roundtrip(b"%jqft 1\n# a comment\n{name: \"ada\"}\n");
    jqft_roundtrip(b"%jqft 1\n{\n  # leading comment\n  a: 1, # trailing comment\n  b: 2,\n}\n");
    jqft_roundtrip(b"%jqft 1\n{quoted-key: 1, \"not-a-bare-key!\": 2, \"a b\": 3}\n");
}

#[test]
fn first_document_of_a_stream_round_trips() {
    jqft_roundtrip(b"%jqft 1\n{a: 1}\n---\n{b: 2}\n");
}

#[test]
fn jqfjson_is_strict_json_and_round_trips() {
    let cases: [&[u8]; 5] = [
        b"{\"a\":1,\"b\":[true,null,\"x\"]}\n",
        b"42\n",
        b"\"str\"\n",
        b"1.50\n",
        b"[]\n",
    ];
    for bytes in cases {
        let first = whole_value(bytes, "jqfjson", "jqfjson.document@1").expect("decode");
        let encoded = encode_value(&first, "jqfjson", "jqfjson.canonical@1").expect("encode");
        let second = whole_value(&encoded, "jqfjson", "jqfjson.document@1").expect("re-decode");
        assert_eq!(render(&first), render(&second));
    }
}

#[test]
fn jqfjson_rejects_trailing_content() {
    assert!(
        whole_value(b"{\"a\":1} extra\n", "jqfjson", "jqfjson.document@1").is_err(),
        "trailing content must be rejected"
    );
    assert!(
        whole_value(b"{\"a\":1}\n", "jqfjson", "jqfjson.document@1").is_ok(),
        "one document per source"
    );
}

#[test]
fn jqft_rejects_bytes_after_base64_padding() {
    // RFC 4648 §3.3: nothing but padding may follow the first `=`.
    for literal in ["b64\"QQ==junk\"", "b64\"QQ=junk\"", "b64\"==junk\"", "b64\"QQ== \""] {
        let source = format!("%jqft 1\n{literal}\n");
        assert!(
            whole_value(source.as_bytes(), "jqft", "jqft.document@1").is_err(),
            "trailing content after base64 padding must be refused: {source:?}"
        );
    }
    let canonical = whole_value(b"%jqft 1\nb64\"aGVsbG8gd29ybGQ=\"\n", "jqft", "jqft.document@1");
    assert!(canonical.is_ok(), "canonical padding still decodes");
}

#[test]
fn jqft_decodes_markup_and_refuses_reserved_spellings() {
    // Markup nodes decode (the angle form), a string bracket key `{("a"): 1}` is an ordinary key, and a NON-STRING
    // bracket key refuses with the projection narrowing's law.
    assert!(
        whole_value(b"%jqft 1\n<p \"text\">\n", "jqft", "jqft.document@1").is_ok(),
        "markup nodes decode"
    );
    assert!(
        whole_value(b"%jqft 1\n{(\"a\"): 1}\n", "jqft", "jqft.document@1").is_ok(),
        "a string bracket key is an ordinary key"
    );
    assert!(
        whole_value(b"%jqft 1\n{(1): \"x\"}\n", "jqft", "jqft.document@1").is_err(),
        "a non-string bracket key is not projectable"
    );
    assert!(
        whole_value(b"%jqft 1\n{1: \"x\"}\n", "jqft", "jqft.document@1").is_err(),
        "a bare non-string key is refused"
    );
    assert!(
        whole_value(b"%jqft 1\n&anchor\n", "jqft", "jqft.document@1").is_err(),
        "anchors are reserved"
    );
    assert!(
        whole_value(b"%jqft 1\n<foo:bar>\n", "jqft", "jqft.document@1").is_err(),
        "namespaced markup names are reserved"
    );
}

#[test]
fn jqft_unicode_escapes_do_not_skip_the_next_byte() {
    let value = whole_value(b"%jqft 1\n\"\\u0041\"\n", "jqft", "jqft.document@1").expect("\\u0041");
    assert_eq!(render(&value), "s\"A\"");
    let value = whole_value(b"%jqft 1\n\"\\u0041B\"\n", "jqft", "jqft.document@1").expect("\\u0041B");
    assert_eq!(render(&value), "s\"AB\"");
}

#[test]
fn jqft_refuses_a_missing_object_value() {
    assert!(
        whole_value(b"%jqft 1\n{a: }\n", "jqft", "jqft.document@1").is_err(),
        "{{a: }} is a missing value, not an empty object"
    );
}

#[test]
fn jqft_refuses_trailing_garbage_on_the_whole_document_route() {
    assert!(
        whole_value(b"%jqft 1\n1 garbage\n", "jqft", "jqft.document@1").is_err(),
        "non-separator trailing content is rejected"
    );
    assert!(
        whole_value(b"%jqft 1\n{a: 1}\n---\n{b: 2}\n", "jqft", "jqft.document@1").is_ok(),
        "a --- stream still yields the first document"
    );
}

#[test]
fn jqft_requires_the_header() {
    assert!(
        whole_value(b"{a: 1}\n", "jqft", "jqft.document@1").is_err(),
        "missing %jqft 1 header must be refused"
    );
    assert!(
        whole_value(b"%jqft 1\n{a: 1}\n", "jqft", "jqft.document@1").is_ok(),
        "header present"
    );
}

#[test]
fn canonical_bytes_are_pinned() {
    let value = whole_value(b"%jqft 1\n{a: 1, b: [true, \"x\"]}\n", "jqft", "jqft.document@1").expect("decode");
    let encoded = encode_value(&value, "jqft", "jqft.canonical@1").expect("encode");
    assert_eq!(
        String::from_utf8(encoded).expect("utf8"),
        "%jqft 1\n{\n  a: 1,\n  b: [\n    true,\n    \"x\"\n  ]\n}",
        "the canonical form is pinned"
    );
}

/// The text encoder's recursion is depth-guarded. The parser is ITERATIVE (deep nesting costs heap, never call stack —
/// see parse.rs's module doc), so a document the parser is guaranteed to accept reaches the recursive encoder; without
/// the guard a deep document would overflow the request thread's stack on re-encode. The guard raises the configured
/// nesting ceiling cleanly instead.
#[test]
fn deep_document_raises_the_nesting_ceiling_on_encode() {
    fn low_ceiling(limit: u32) -> ResourceContext<'static> {
        ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, limit))
                .expect("account"),
            &CONTROL,
            WorkMeter::try_new_v1(4_096).expect("work"),
        )
        .expect("resources")
    }

    // A 1,000-deep document: the ITERATIVE parser accepts it (verified by the decode below), so it is exactly the shape
    // that used to reach the recursive encoder and blow the stack.
    let deep_text = format!("%jqft 1\n{}0{}\n", "[".repeat(1_000), "]".repeat(1_000));
    let value = whole_value(deep_text.as_bytes(), "jqft", "jqft.document@1")
        .expect("the iterative parser accepts a 1,000-deep document");

    let mut ctx = low_ceiling(100);
    let format = FormatId::try_new(jqf_codec_jqft::FORMAT_ID).expect("format");
    let dialect = DialectId::try_new(jqf_codec_jqft::JQFT_CANONICAL_DIALECT_ID).expect("dialect");
    let registration = jqf_codec_jqft::registration_jqft().expect("registration");
    let factory = registration
        .encoder()
        .expect("encoder")
        .create_factory(
            EncodeRequest {
                format: &format,
                dialect: &dialect,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                preservation: PreservationRequest::None,
                options: None,
            },
            &mut ctx,
        )
        .map_err(|error| error.kind())
        .expect("factory");
    let mut session = factory
        .start(EncodeItem::owned(&value), PreservationRequest::None, &mut ctx)
        .map_err(|error| error.kind())
        .expect("session");
    let mut saw_ceiling = false;
    let mut discarded = Vec::new();
    {
        let mut sink = jqf_codec_core::VecByteSink::new(&mut discarded);
        let mut run = CodecRunContext::new(&mut ctx);
        run.set_cooperative_credits(4_096);
        if let Err(error) = session.encode(&mut sink, &mut run) {
            assert!(
                matches!(
                    error.kind(),
                    CodecFailureKind::Resource(jqf_resource::ResourceError::LimitExceeded {
                        limit_kind: jqf_resource::ResourceLimit::NestingDepth,
                        ..
                    })
                ),
                "expected a clean nesting-depth ceiling raise, got {error:?}"
            );
            saw_ceiling = true;
        }
    }
    assert!(saw_ceiling, "the 1,000-deep value must hit the depth ceiling");
}
