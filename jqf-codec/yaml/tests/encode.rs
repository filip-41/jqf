//! Encoder decision branches: quote-vs-plain (including `<<`), chomping, and decode-after-encode identity.

use jqf_codec_core::{
    AccessGuarantees, AccessOutcome, AccessRequirement, CodecDemand, CodecError, DiagnosticPolicy, EncodeItem,
    EncodeRequest, ErasedProvider, PreservationRequest, ValidationMode, VecByteSink,
};
use jqf_data::{DialectId, FormatId, ObjectBuilder, ObjectKey, Value};
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

fn source(bytes: &[u8]) -> ResolvedSource<'_> {
    ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "test.yaml",
        bytes,
        0,
    )
}

fn string(text: &str) -> Value {
    Value::try_string(text).expect("string")
}

fn encode_block(value: &Value) -> Result<Vec<u8>, CodecError> {
    let registration = jqf_codec_yaml::registration().expect("registration");
    let mut resources = resources();
    let options = jqf_codec_yaml::YamlTargetSchema::Core;
    let format = FormatId::try_new(jqf_codec_yaml::FORMAT_ID).expect("format");
    let dialect = DialectId::try_new(jqf_codec_yaml::YAML_BLOCK_DIALECT_ID).expect("dialect");
    let request = EncodeRequest {
        format: &format,
        dialect: &dialect,
        diagnostics: DiagnosticPolicy::ErrorsOnly,
        preservation: PreservationRequest::None,
        options: Some(&options as &(dyn core::any::Any + Send + Sync)),
    };
    let factory = registration
        .encoder()
        .expect("encoder")
        .create_factory(request, &mut resources)?;
    let mut session = factory.start(EncodeItem::Owned(value), PreservationRequest::None, &mut resources)?;
    let mut out = Vec::new();
    {
        let mut sink = VecByteSink::new(&mut out);
        let mut context = jqf_codec_core::CodecRunContext::new(&mut resources);
        context.set_cooperative_credits(4_096);
        session.encode(&mut sink, &mut context)?;
    }
    // The block profile's item newline is facade-owned. The product path appends it; without it a final clip block has
    // no last content break.
    if !out.ends_with(b"\n") {
        out.push(b'\n');
    }
    Ok(out)
}

fn decode(bytes: &[u8]) -> Result<Value, CodecError> {
    let registration = jqf_codec_yaml::registration().expect("registration");
    let decoder = registration.decoder().expect("decoder");
    let mut resources = resources();
    let dialect: &'static DialectId = Box::leak(Box::new(
        DialectId::try_new(jqf_codec_yaml::YAML_CORE_DIALECT_ID).expect("dialect"),
    ));
    let request = jqf_codec_core::DecodeRequest {
        validation: ValidationMode::Strict,
        diagnostics: DiagnosticPolicy::ErrorsOnly,
        dialect,
        options: None,
        allow_adjacent_values: false,
        value_separator: &[],
    };
    let owned = bytes.to_vec();
    let owned: &'static [u8] = Box::leak(owned.into_boxed_slice());
    let mut provider: ErasedProvider = decoder.create_provider(source(owned), request, &mut resources)?;
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
    product.document().materialize_root(&mut resources).map_err(|_error| {
        CodecError::new(jqf_codec_core::CodecFailureKind::InternalContractViolation {
            contract: "materialize root",
        })
    })
}

fn object_with(key: &str, value: Value) -> Value {
    let mut builder = ObjectBuilder::new();
    builder
        .try_insert_last(ObjectKey::try_from_str(key).expect("key"), value)
        .expect("insert");
    Value::Object(builder.try_finish().expect("finish"))
}

fn round_trip(value: &Value) -> Value {
    let encoded = encode_block(value).expect("encode");
    decode(&encoded).unwrap_or_else(|error| {
        panic!(
            "re-decode failed: {error:?} bytes={}",
            String::from_utf8_lossy(&encoded)
        )
    })
}

#[test]
fn merge_indicator_key_round_trips() {
    let value = object_with("<<", string("x"));
    let encoded = encode_block(&value).expect("encode");
    let text = String::from_utf8(encoded.clone()).expect("utf8");
    assert!(
        text.contains("\"<<\""),
        "the merge-indicator key must be quoted, got {text:?}"
    );
    let again = decode(&encoded).expect("re-decode");
    let Value::Object(object) = &again else {
        panic!("expected object, got {again:?}");
    };
    let Value::String(got) = object.get("<<").expect("<<") else {
        panic!("expected string value");
    };
    assert_eq!(got.as_str(), "x");
}

#[test]
fn nested_merge_indicator_key_round_trips() {
    let inner = object_with("<<", string("1"));
    let value = object_with("m", inner);
    let again = round_trip(&value);
    let Value::Object(outer) = &again else {
        panic!("expected object");
    };
    let Value::Object(inner) = outer.get("m").expect("m") else {
        panic!("expected nested object");
    };
    assert!(inner.get("<<").is_some());
}

#[test]
fn plain_string_stays_plain() {
    let encoded = encode_block(&string("hello")).expect("encode");
    let text = String::from_utf8(encoded).expect("utf8");
    assert!(!text.contains('"'), "hello stays plain, got {text:?}");
}

#[test]
fn clip_literal_round_trips() {
    let value = object_with("t", string("a\nb\n"));
    let encoded = encode_block(&value).expect("encode");
    let text = String::from_utf8(encoded.clone()).expect("utf8");
    assert!(text.contains("|\n"), "clip form, got {text:?}");
    assert!(!text.contains("|-"), "not strip, got {text:?}");
    let again = decode(&encoded).expect("re-decode");
    let Value::Object(object) = &again else {
        panic!("expected object");
    };
    let Value::String(got) = object.get("t").expect("t") else {
        panic!("expected string");
    };
    assert_eq!(got.as_str(), "a\nb\n");
}

#[test]
fn strip_literal_round_trips() {
    let value = object_with("t", string("a\nb"));
    let encoded = encode_block(&value).expect("encode");
    let text = String::from_utf8(encoded.clone()).expect("utf8");
    assert!(text.contains("|-"), "strip form, got {text:?}");
    let again = decode(&encoded).expect("re-decode");
    let Value::Object(object) = &again else {
        panic!("expected object");
    };
    let Value::String(got) = object.get("t").expect("t") else {
        panic!("expected string");
    };
    assert_eq!(got.as_str(), "a\nb");
}

#[test]
fn keep_shaped_value_round_trips() {
    // A second trailing newline is not one of the block dialect's two chomping states, so the encoder quotes; the
    // decoder must still recover the same text.
    let value = object_with("t", string("a\nb\n\n"));
    let encoded = encode_block(&value).expect("encode");
    let again = decode(&encoded).expect("re-decode");
    let Value::Object(object) = &again else {
        panic!("expected object");
    };
    let Value::String(got) = object.get("t").expect("t") else {
        panic!("expected string");
    };
    assert_eq!(got.as_str(), "a\nb\n\n");
}

#[test]
fn nested_tags_are_unrepresentable() {
    let inner =
        Value::try_tagged(jqf_data::TagId::try_new_unaccounted("!bar").expect("tag"), string("x")).expect("inner");
    let outer = Value::try_tagged(jqf_data::TagId::try_new_unaccounted("!foo").expect("tag"), inner).expect("outer");
    let error = encode_block(&outer).expect_err("nested tags");
    assert_eq!(
        error.kind(),
        jqf_codec_core::CodecFailureKind::UnsupportedRepresentation
    );
}

#[test]
fn single_document_accepts_one_item_per_unit() {
    let registration = jqf_codec_yaml::registration().expect("registration");
    let mut resources = resources();
    let options = jqf_codec_yaml::YamlTargetSchema::Core;
    let format = FormatId::try_new(jqf_codec_yaml::FORMAT_ID).expect("format");
    let dialect = DialectId::try_new(jqf_codec_yaml::YAML_SINGLE_DOCUMENT_DIALECT_ID).expect("dialect");
    let request = EncodeRequest {
        format: &format,
        dialect: &dialect,
        diagnostics: DiagnosticPolicy::ErrorsOnly,
        preservation: PreservationRequest::None,
        options: Some(&options as &(dyn core::any::Any + Send + Sync)),
    };
    let factory = registration
        .encoder()
        .expect("encoder")
        .create_factory(request, &mut resources)
        .expect("factory");
    let first = string("a");
    factory
        .start(EncodeItem::Owned(&first), PreservationRequest::None, &mut resources)
        .expect("first item");
    let second = string("b");
    factory
        .start(EncodeItem::Owned(&second), PreservationRequest::None, &mut resources)
        .expect("second item is another single document, not a factory refusal");
}
