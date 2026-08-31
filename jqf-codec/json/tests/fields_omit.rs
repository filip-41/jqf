//! Engine-packed Fields omit: unread object members are not materialized, and
//! a corrupt omitted member or trailing junk still fails validation.
//!
//! `{id,score}` is Whole prune. `.catalog | {id,name}` and `.users[0] | {id,score}`
//! are Exact prune on the located object. `map({id,score})` records construct
//! fields on the locate walk. Codecs see Whole or Exact only.

mod common;

use jqf_codec_core::{
    AccessOutcome, CodecRunContext, DecodeRequest, DiagnosticPolicy, ExactSelectionRecord, ValidationMode,
};
use jqf_data::{DialectId, Value};
use jqf_sdk::{CodecRequirementPolicy, CompileOptions, try_compile_program};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

const CREDITS: u32 = 4_096;

fn source(bytes: &[u8]) -> ResolvedSource<'_> {
    ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "fields-omit.json",
        bytes,
        0,
    )
}

fn try_decode(program: &str, bytes: &[u8]) -> Result<usize, jqf_codec_core::CodecError> {
    let mut resources = common::resources();
    let compiled = try_compile_program(
        program,
        CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly),
        CompileOptions::new(),
        &resources,
    )
    .expect("compiles");
    let requirement = compiled.try_requirement(&resources).expect("requirement");
    let mut provider = jqf_codec_json::registration()
        .expect("registration")
        .decoder()
        .expect("decoder")
        .create_provider(
            source(bytes),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new("rfc8259").expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: jqf_codec_json::VALUE_SEPARATORS,
            },
            &mut resources,
        )
        .expect("provider");
    let handle = provider.bind(&requirement).expect("bind");
    let mut session = provider.open(&handle, &mut resources).expect("open");
    let mut run = CodecRunContext::new(&mut resources);
    run.set_cooperative_credits(CREDITS);
    let result = session.decode(&mut run)?;
    Ok(match result.outcome() {
        AccessOutcome::FullDocument(product) => product.document().node_count(),
        AccessOutcome::Located(located) => located.product().document().node_count(),
    })
}

fn decode_nodes(program: &str, bytes: &[u8]) -> usize {
    try_decode(program, bytes).expect("decode")
}

fn decode_err(program: &str, bytes: &[u8]) {
    try_decode(program, bytes).expect_err("unread corruption and trailing junk must still fail");
}

#[test]
fn root_construct_omits_unread_siblings() {
    let fat = br#"{"id":1,"score":2,"blob":[1,2,3,4,5]}"#;
    assert_eq!(decode_nodes("{id,score}", fat), 3, "object + id + score");
    decode_err("{id,score}", br#"{"id":1,"score":2,"blob":[}"#);
    decode_err("{id,score}", br#"{"id":1,"score":2} false"#);
}

#[test]
fn exact_construct_omits_unread_siblings_of_the_located_object() {
    let fat = br#"{"catalog":{"id":1,"name":"a","blob":[1,2,3,4,5]}}"#;
    assert_eq!(
        decode_nodes(".catalog | {id,name}", fat),
        3,
        "located object + id + name"
    );
    decode_err(".catalog | {id,name}", br#"{"catalog":{"id":1,"name":"a","blob":[}"#);
    decode_err(".catalog | {id,name}", br#"{"catalog":{"id":1,"name":"a"}} false"#);
}

#[test]
fn indexed_construct_omits_unread_siblings_of_the_located_element() {
    let fat = br#"{"users":[{"id":1,"score":2,"blob":[1,2,3,4,5]}]}"#;
    assert_eq!(
        decode_nodes(".users[0] | {id,score}", fat),
        3,
        "located user + id + score"
    );
    decode_err(".users[0] | {id,score}", br#"{"users":[{"id":1,"score":2,"blob":[}"#);
}

#[test]
fn map_construct_records_named_fields_without_unread_siblings() {
    let mut resources = common::resources();
    let compiled = try_compile_program(
        ".users | map({id,score})",
        CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly),
        CompileOptions::new(),
        &resources,
    )
    .expect("compiles");
    let requirement = compiled.try_requirement(&resources).expect("requirement");
    assert!(requirement.element_construct().is_some());
    let bytes = br#"{"users":[{"id":1,"score":2,"blob":[1,2,3]}]}"#;
    let mut provider = jqf_codec_json::registration()
        .expect("registration")
        .decoder()
        .expect("decoder")
        .create_provider(
            source(bytes),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new("rfc8259").expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: jqf_codec_json::VALUE_SEPARATORS,
            },
            &mut resources,
        )
        .expect("provider");
    let handle = provider.bind(&requirement).expect("bind");
    let mut session = provider.open(&handle, &mut resources).expect("open");
    let mut run = CodecRunContext::new(&mut resources);
    run.set_cooperative_credits(CREDITS);
    let result = session.decode(&mut run).expect("decode");
    let AccessOutcome::Located(located) = result.outcome() else {
        panic!("map construct Exact-locates");
    };
    let ExactSelectionRecord::Node { node, .. } = located.result() else {
        panic!("node");
    };
    let values = located
        .product()
        .document()
        .container_span_values(*node)
        .expect("handle")
        .expect("cached");
    assert_eq!(values.len(), 1);
    let Value::Object(first) = values[0].untagged() else {
        panic!("object");
    };
    assert!(first.get("id").is_some());
    assert!(first.get("score").is_some());
    assert!(first.get("blob").is_none(), "unread blob is not in the construct cache");
    decode_err(".users | map({id,score})", br#"{"users":[{"id":1,"score":2,"blob":[}"#);
}
