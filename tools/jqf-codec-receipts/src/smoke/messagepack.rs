//! `MessagePack` codec receipt battery (plan 136 S0/S1/S2).
//!
//! The route-slot duty: ONE advertised slot, Whole/`CompleteDocument` — the
//! same inventory `jqf-sdk-smoke` pins. The battery drives the registration's
//! own decoder and encoder factories: the whole marker grammar, extension
//! identity, the three timestamp encodings, the deterministic shortest-form
//! encoder, the decode∘encode fixpoint, the encoder-reuse law, and the
//! rejects (reserved `0xc1`, trailing bytes, a non-`str` map key, an invalid
//! reserved `-1` timestamp payload).

use jqf_codec_core::{
    AccessOutcome, CodecRunContext, DecodeRequest, DiagnosticPolicy, EncodeItem, EncodeRequest, ValidationMode,
};
use jqf_data::{DialectId, FormatId, Value, ValueKind};
use jqf_resource::ResourceContext;

use crate::drive::{resources, source, whole_requirement};

fn decode_one(bytes: &[u8], dialect: &str, resources: &mut ResourceContext<'_>) -> Result<Value, String> {
    let registration = jqf_codec_messagepack::registration().map_err(|e| format!("{e:?}"))?;
    let mut provider = registration
        .decoder()
        .expect("decoder")
        .create_provider(
            source(bytes),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(dialect).expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            resources,
        )
        .map_err(|e| format!("provider: {:?}", e.kind()))?;
    let requirement = whole_requirement(resources);
    let handle = provider.bind(&requirement).map_err(|e| format!("{e:?}"))?;
    let mut session = provider.open(&handle, resources).map_err(|e| format!("{e:?}"))?;
    let mut run = CodecRunContext::new(resources);
    run.set_cooperative_credits(4_096);
    let result = session.decode(&mut run).map_err(|e| format!("decode: {e:?}"))?;
    match result.outcome() {
        AccessOutcome::FullDocument(product) => product
            .document()
            .materialize_root(resources)
            .map_err(|e| e.to_string()),
        AccessOutcome::Located { .. } => Err("unexpected located outcome".into()),
    }
}

fn encode_one(value: &Value, resources: &mut ResourceContext<'_>) -> Result<Vec<u8>, String> {
    let registration = jqf_codec_messagepack::registration().map_err(|e| format!("{e:?}"))?;
    let format = FormatId::try_new(jqf_codec_messagepack::FORMAT_ID).map_err(|e| e.to_string())?;
    let dialect =
        DialectId::try_new(jqf_codec_messagepack::MESSAGEPACK_DETERMINISTIC_DIALECT_ID).map_err(|e| e.to_string())?;
    let factory = registration
        .encoder()
        .expect("encoder")
        .create_factory(
            EncodeRequest {
                format: &format,
                dialect: &dialect,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                preservation: jqf_codec_core::PreservationRequest::None,
                options: None,
            },
            resources,
        )
        .map_err(|e| format!("factory: {:?}", e.kind()))?;
    let mut session = factory
        .start(
            EncodeItem::Owned(value),
            jqf_codec_core::PreservationRequest::None,
            resources,
        )
        .map_err(|e| format!("session: {:?}", e.kind()))?;
    let mut out = Vec::new();
    {
        let mut sink = jqf_codec_core::VecByteSink::new(&mut out);
        let mut run = CodecRunContext::new(resources);
        run.set_cooperative_credits(4_096);
        session
            .encode(&mut sink, &mut run)
            .map_err(|e| format!("encode: {:?}", e.kind()))?;
    }
    Ok(out)
}

/// The smoke battery's value-equality law: structural (kind + primitive),
/// enough to compare fixpoints without importing the engine's semantic law.
/// This is a LOCAL comparator, deliberately not the shared
/// `jqf-codec-fuzz` `values_eq`: it has no Decimal coefficient/scale arm,
/// so two distinct decimals of one number category would compare equal.
/// That divergence is moot for this battery because msgpack decodes no
/// decimals; if a decimal-producing msgpack profile ever lands, port the
/// shared arm here rather than trusting this law.
fn values_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Number(a), Value::Number(b)) => {
            a.category() == b.category()
                && a.to_i64() == b.to_i64()
                && a.as_float().map(jqf_data::Float::bits) == b.as_float().map(jqf_data::Float::bits)
        }
        (Value::String(a), Value::String(b)) => a.as_str() == b.as_str(),
        (Value::Bytes(a), Value::Bytes(b)) => a.as_slice() == b.as_slice(),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b.iter()).all(|(l, r)| values_equal(l, r))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .zip(b.iter())
                    .all(|(l, r)| l.key() == r.key() && values_equal(l.value(), r.value()))
        }
        (Value::OffsetDateTime(a), Value::OffsetDateTime(b)) => {
            let mut a_text = String::new();
            let mut b_text = String::new();
            a.write_text(&mut a_text).is_ok() && b.write_text(&mut b_text).is_ok() && a_text == b_text
        }
        (
            Value::Tagged {
                tag: a_tag,
                payload: a_payload,
            },
            Value::Tagged {
                tag: b_tag,
                payload: b_payload,
            },
        ) => a_tag.as_str() == b_tag.as_str() && values_equal(a_payload, b_payload),
        _ => false,
    }
}

/// The `MessagePack` smoke battery.
#[allow(
    clippy::too_many_lines,
    reason = "one battery: registration surface, decode corpus, extensions, timestamps, fixpoint, reuse, rejects"
)]
pub fn run() -> Result<(), String> {
    let registration = jqf_codec_messagepack::registration().map_err(|e| format!("{e:?}"))?;
    let descriptor = registration.descriptor();
    if descriptor.format().as_str() != jqf_codec_messagepack::FORMAT_ID {
        return Err("registration names the wrong format".into());
    }
    let dialects: Vec<&str> = descriptor.dialects().iter().map(|dialect| dialect.as_str()).collect();
    for expected in [
        jqf_codec_messagepack::MESSAGEPACK_UTF8_DIALECT_ID,
        jqf_codec_messagepack::MESSAGEPACK_KEY_EQUIVALENCE_DIALECT_ID,
        jqf_codec_messagepack::MESSAGEPACK_WIRE_DIALECT_ID,
        jqf_codec_messagepack::MESSAGEPACK_DETERMINISTIC_DIALECT_ID,
    ] {
        if !dialects.contains(&expected) {
            return Err(format!("dialect {expected} missing from the registration"));
        }
    }
    if descriptor.extensions() != ["msgpack", "mpk"] {
        return Err(format!("extensions drifted: {:?}", descriptor.extensions()));
    }
    let mut resources = resources();

    // --- S0: the whole-document decode corpus -------------------------------
    let decoded = decode_one(
        &[0x82, 0xa1, b'a', 0x01, 0xa3, b'k', b'e', b'y', 0xa3, b'v', b'a', b'l'],
        jqf_codec_messagepack::MESSAGEPACK_UTF8_DIALECT_ID,
        &mut resources,
    )?;
    let Value::Object(object) = &decoded else {
        return Err("expected an object".into());
    };
    let Value::Number(number) = object.get("a").ok_or("missing a")? else {
        return Err("a is not a number".into());
    };
    assert_eq!(number.to_i64(), Some(1), "fixint projects");
    let Value::String(text) = object.get("key").ok_or("missing key")? else {
        return Err("key is not a string".into());
    };
    assert_eq!(text.as_str(), "val");

    // A uint64 above i64::MAX projects to the exact arbitrary-precision
    // integer (2^63).
    let decoded = decode_one(
        &[0xcf, 0x80, 0, 0, 0, 0, 0, 0, 0],
        jqf_codec_messagepack::MESSAGEPACK_UTF8_DIALECT_ID,
        &mut resources,
    )?;
    let Value::Number(number) = decoded else {
        return Err("expected a number".into());
    };
    assert_eq!(number.to_i64(), None, "2^63 does not fit i64");
    let integer = number.to_integer().ok_or("integer")?;
    assert_eq!(integer.as_str(), "9223372036854775808");

    // Float32 widens exactly (the promoted core widen_f32 law).
    let decoded = decode_one(
        &[0xca, 0x3f, 0x80, 0x00, 0x00],
        jqf_codec_messagepack::MESSAGEPACK_UTF8_DIALECT_ID,
        &mut resources,
    )?;
    assert_eq!(decoded.kind(), ValueKind::Number);
    let Value::Number(number) = decoded else { unreachable!() };
    assert_eq!(number.as_float().map(jqf_data::Float::get), Some(1.0));

    // --- S1: extensions and timestamps ---------------------------------------
    // Extension identity: msgpack:ext:42 with a byte payload.
    let decoded = decode_one(
        &[0xc7, 0x03, 42, 1, 2, 3],
        jqf_codec_messagepack::MESSAGEPACK_UTF8_DIALECT_ID,
        &mut resources,
    )?;
    let Value::Tagged { tag, payload } = decoded else {
        return Err("an extension must project to a tagged value".into());
    };
    assert_eq!(tag.as_str(), "msgpack:ext:42");
    let Value::Bytes(bytes) = &*payload else {
        return Err("extension payload is bytes".into());
    };
    assert_eq!(bytes.as_slice(), &[1, 2, 3]);

    // Timestamp 32-bit: 1 second → UTC OffsetDateTime with the intrinsic tag.
    let decoded = decode_one(
        &[0xd6, 0xff, 0, 0, 0, 1],
        jqf_codec_messagepack::MESSAGEPACK_UTF8_DIALECT_ID,
        &mut resources,
    )?;
    assert!(matches!(decoded, Value::OffsetDateTime(_)));

    // Timestamp 64-bit with nanoseconds.
    let decoded = decode_one(
        &[0xd7, 0xff, 0, 0, 0, 0, 0, 0, 0, 1],
        jqf_codec_messagepack::MESSAGEPACK_UTF8_DIALECT_ID,
        &mut resources,
    )?;
    assert!(matches!(decoded, Value::OffsetDateTime(_)));

    // Timestamp 96-bit out of the core year range → the exact
    // {seconds, nanoseconds} tagged object.
    let decoded = decode_one(
        &[
            0xc7, 0x0c, 0xff, 0, 0, 0, 1, 0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        ],
        jqf_codec_messagepack::MESSAGEPACK_UTF8_DIALECT_ID,
        &mut resources,
    )?;
    let Value::Tagged { tag, payload } = &decoded else {
        return Err("an out-of-range timestamp stays tagged".into());
    };
    assert_eq!(tag.as_str(), "msgpack:ext:-1");
    let Value::Object(object) = &**payload else {
        return Err("out-of-range payload is an object".into());
    };
    assert_eq!(object.len(), 2);

    // --- S2: encode + fixpoint + reuse law ------------------------------------
    let bytes = encode_one(&decoded, &mut resources)?;
    let again = decode_one(
        &bytes,
        jqf_codec_messagepack::MESSAGEPACK_UTF8_DIALECT_ID,
        &mut resources,
    )?;
    if !values_equal(&decoded, &again) {
        return Err(format!("fixpoint failed: {decoded:?} != {again:?}"));
    }

    // The shortest-form law: an array [1,2,3] encodes as fixarray 0x93.
    let mut items = jqf_data::Array::try_new().expect("array");
    for v in [1_i64, 2, 3] {
        items
            .try_push(Value::Number(
                jqf_data::Number::try_integer_unaccounted(jqf_data::Integer::from_i64(v)).expect("number"),
            ))
            .expect("push");
    }
    let array = Value::Array(items);
    let bytes = encode_one(&array, &mut resources)?;
    assert_eq!(bytes, vec![0x93, 0x01, 0x02, 0x03], "shortest array form");

    // The encoder-reuse law: a recycled encoder reproduces a fresh one.
    let registration = jqf_codec_messagepack::registration().map_err(|e| format!("{e:?}"))?;
    let format = FormatId::try_new(jqf_codec_messagepack::FORMAT_ID).map_err(|e| e.to_string())?;
    let dialect =
        DialectId::try_new(jqf_codec_messagepack::MESSAGEPACK_DETERMINISTIC_DIALECT_ID).map_err(|e| e.to_string())?;
    let factory = registration
        .encoder()
        .expect("encoder")
        .create_factory(
            EncodeRequest {
                format: &format,
                dialect: &dialect,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                preservation: jqf_codec_core::PreservationRequest::None,
                options: None,
            },
            &mut resources,
        )
        .map_err(|e| format!("factory: {:?}", e.kind()))?;
    let fresh = encode_one(&decoded, &mut resources)?;
    let mut session = factory
        .start(
            EncodeItem::Owned(&decoded),
            jqf_codec_core::PreservationRequest::None,
            &mut resources,
        )
        .map_err(|e| format!("session: {:?}", e.kind()))?;
    let mut reused = Vec::new();
    {
        let mut sink = jqf_codec_core::VecByteSink::new(&mut reused);
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        session
            .encode(&mut sink, &mut run)
            .map_err(|e| format!("reuse encode: {:?}", e.kind()))?;
    }
    if fresh != reused {
        return Err("encoder reuse law violated: recycled != fresh".into());
    }

    // --- Rejects --------------------------------------------------------------
    // The reserved 0xc1 byte.
    let error = decode_one(
        &[0xc1],
        jqf_codec_messagepack::MESSAGEPACK_UTF8_DIALECT_ID,
        &mut resources,
    )
    .expect_err("0xc1 rejects");
    assert!(error.contains("reserved-byte"), "{error}");
    // Trailing bytes.
    let error = decode_one(
        &[0x01, 0x02],
        jqf_codec_messagepack::MESSAGEPACK_UTF8_DIALECT_ID,
        &mut resources,
    )
    .expect_err("trailing bytes reject");
    assert!(error.contains("trailing-bytes"), "{error}");
    // An invalid-UTF-8 str under utf8@1.
    let error = decode_one(
        &[0xa2, 0xff, 0xfe],
        jqf_codec_messagepack::MESSAGEPACK_UTF8_DIALECT_ID,
        &mut resources,
    )
    .expect_err("invalid utf8 rejects");
    assert!(error.contains("invalid-utf8"), "{error}");
    // A non-str map key is unrepresentable (plan 136 §3.8).
    let error = decode_one(
        &[0x81, 0x01, 0x02],
        jqf_codec_messagepack::MESSAGEPACK_UTF8_DIALECT_ID,
        &mut resources,
    )
    .expect_err("a numeric map key rejects");
    assert!(error.contains("unrepresentable"), "{error}");
    // An invalid reserved -1 timestamp payload is refused at the semantic
    // build (a 3-byte ext -1).
    let error = decode_one(
        &[0xc7, 0x03, 0xff, 0, 0, 0],
        jqf_codec_messagepack::MESSAGEPACK_UTF8_DIALECT_ID,
        &mut resources,
    )
    .expect_err("invalid timestamp rejects");
    assert!(error.contains("unrepresentable"), "{error}");
    // The wire dialect accepts the invalid-UTF-8 str structurally; the
    // semantic build then refuses it.
    let error = decode_one(
        &[0xa2, 0xff, 0xfe],
        jqf_codec_messagepack::MESSAGEPACK_WIRE_DIALECT_ID,
        &mut resources,
    )
    .expect_err("wire still has no semantic text document");
    assert!(error.contains("unrepresentable"), "{error}");

    // --- The key-equivalence dialect (plan 145 lane K3) ----------------------
    // The dialect's one observable law on materializable documents: a
    // repeated `str` key — here the SAME bytes via fixstr and str8 — REJECTS,
    // where utf8@1 preserves the duplicate with jqf's own
    // first-position/final-value law.
    let duplicate = [0x82, 0xa1, b'a', 0x01, 0xd9, 0x01, b'a', 0x02];
    let error = decode_one(
        &duplicate,
        jqf_codec_messagepack::MESSAGEPACK_KEY_EQUIVALENCE_DIALECT_ID,
        &mut resources,
    )
    .expect_err("a duplicate str key rejects");
    assert!(error.contains("duplicate-key"), "{error}");
    // The base dialect accepts the same bytes: last-value-wins.
    let decoded = decode_one(
        &duplicate,
        jqf_codec_messagepack::MESSAGEPACK_UTF8_DIALECT_ID,
        &mut resources,
    )?;
    let Value::Object(object) = &decoded else {
        return Err("expected an object".into());
    };
    let Value::Number(number) = object.get("a").ok_or("missing merged key")? else {
        return Err("merged key is not a number".into());
    };
    assert_eq!(number.to_i64(), Some(2), "the final value wins");
    // A map with distinct str keys passes under the dialect.
    decode_one(
        &[0x82, 0xa1, b'a', 0x01, 0xa1, b'b', 0x02],
        jqf_codec_messagepack::MESSAGEPACK_KEY_EQUIVALENCE_DIALECT_ID,
        &mut resources,
    )?;
    // The law fires BEFORE the semantic build's non-str-key refusal:
    // `{uint8 5: 1, fixint 5: 2}` is a duplicate under the law (integers by
    // mathematical value ACROSS marker widths) and rejects with
    // duplicate-key, where utf8@1 rejects the same bytes with the §3.8
    // non-str-key unrepresentable.
    let numeric = [0x82, 0xcc, 0x05, 0x01, 0x05, 0x02];
    let error = decode_one(
        &numeric,
        jqf_codec_messagepack::MESSAGEPACK_KEY_EQUIVALENCE_DIALECT_ID,
        &mut resources,
    )
    .expect_err("a duplicate integer key rejects");
    assert!(error.contains("duplicate-key"), "{error}");
    let error = decode_one(
        &numeric,
        jqf_codec_messagepack::MESSAGEPACK_UTF8_DIALECT_ID,
        &mut resources,
    )
    .expect_err("utf8@1 has no semantic document for a non-str key set");
    assert!(error.contains("unrepresentable"), "{error}");
    // Distinct native keys do NOT fire the law: `{bin"a": 1, str"a": 2}`
    // and `{1: 1, 1.0: 2}` pass the key validation and only then refuse the
    // non-str key set at the semantic build — never a duplicate-key.
    for (name, bytes) in [
        ("str-vs-bin", &[0x82, 0xc4, 0x01, b'a', 0x01, 0xa1, b'a', 0x02][..]),
        (
            "int-vs-float",
            &[0x82, 0x01, 0x01, 0xca, 0x3f, 0x80, 0x00, 0x00, 0x02][..],
        ),
    ] {
        let error = decode_one(
            bytes,
            jqf_codec_messagepack::MESSAGEPACK_KEY_EQUIVALENCE_DIALECT_ID,
            &mut resources,
        )
        .expect_err("distinct native keys never fire the law");
        assert!(
            error.contains("unrepresentable") && !error.contains("duplicate-key"),
            "{name}: expected the non-str-key refusal, got {error}"
        );
    }
    Ok(())
}
