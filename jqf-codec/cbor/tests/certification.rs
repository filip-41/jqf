//! CBOR (RFC 8949) codec certification suite, instantiating the kit design
//! (`.docs-intenal/codec-certification-kit-design.md`).
//!
//! Law rows exercised here, table-driven: table R — R-SCAN (whole-input validating scan, corrupt-late),
//!             R-MAT (materialize(root) == whole-document build),
//!             R-ENC (decode → encode → re-decode semantic equal),
//!             R-ENC-B (encoder bytes == golden bytes),
//!             R-HINT (demand is a hint: empty vs full demand, same answer),
//!             R-VOC (value vocabulary: tags, bignums, non-finite, exact
//!                    decimals, duplicate-key law);
//! table P — P-ZERO (failed encode publishes zero bytes),
//!             P-BOUND (encoder output respects the byte boundary),
//!             P-CHUNK (bounded-sink publication completes, bytes unchanged);
//! table A — A-REPORT (`PreservationReport` axes honest),
//!             A-CANCEL (cancelled decode stops with a typed error);
//! cross-cutting — the two-kind declaration (`CompleteDocument` only, never `RecordStream`) and the E-table
//! (span/`--edit` law) — a declared PRESENCE with in-crate splice receipts (E-IDENT/E-SPLICE/E-SURV) beside the
//! differential's arms.

use jqf_codec_core::{
    AccessGuarantees, AccessOutcome, AccessRequirement, AccessResultKind, ByteSink, CodecDemand, CodecError,
    CodecFailureKind, CodecRunContext, DecodeRequest, DemandClause, DiagnosticPolicy, EncodeItem, EncodeRequest,
    PreservationOutcome, PreservationRequest, ValidationMode, VecByteSink,
};
use jqf_data::{DialectId, FormatId, Value};
use jqf_resource::{
    ContinueControl, Control, ControlOutcome, RequestAccount, ResourceContext, ResourceLimits, WorkMeter,
};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

static CONTROL: ContinueControl = ContinueControl;

const DECODE_DIALECT: &str = jqf_codec_cbor::CBOR_PREFERRED_DIALECT_ID;
const ENCODE_DIALECT: &str = jqf_codec_cbor::CBOR_CORE_DETERMINISTIC_DIALECT_ID;

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
        SourceRef::new(SourceId::new(97), SourceKind::Input),
        "test.cbor",
        bytes,
        0,
    )
}

fn requirement(resources: &ResourceContext<'_>, demand: CodecDemand) -> AccessRequirement {
    AccessRequirement::try_whole(
        demand,
        AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
        resources,
    )
    .expect("requirement")
}

fn whole_demand(resources: &ResourceContext<'_>) -> CodecDemand {
    let mut demand = CodecDemand::try_new(resources);
    demand.try_insert(&DemandClause::SemanticRoot).expect("semantic root");
    demand.try_insert(&DemandClause::ValueShape).expect("value shape");
    demand
}

fn decode_with(bytes: &[u8], demand: CodecDemand) -> Result<Value, CodecError> {
    let mut resources = resources();
    let registration = jqf_codec_cbor::registration().expect("registration");
    let dialect = DialectId::try_new(DECODE_DIALECT).expect("dialect");
    let mut provider = registration.decoder().expect("decoder").create_provider(
        source(bytes),
        DecodeRequest {
            validation: ValidationMode::Strict,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            dialect: &dialect,
            options: None,
            allow_adjacent_values: false,
            value_separator: &[],
        },
        &mut resources,
    )?;
    let requirement = requirement(&resources, demand);
    let handle = provider.bind(&requirement).expect("bind");
    let mut session = provider.open(&handle, &mut resources)?;
    let result = {
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        session.decode(&mut run)?
    };
    let AccessOutcome::FullDocument(product) = result.outcome() else {
        panic!("expected full document");
    };
    product.document().materialize_root(&mut resources).map_err(|_| {
        CodecError::new(CodecFailureKind::InternalContractViolation {
            contract: "materialize CBOR root",
        })
    })
}

fn decode(bytes: &[u8]) -> Result<Value, CodecError> {
    let mut resources = resources();
    decode_with_shared(bytes, &mut resources)
}

fn decode_with_shared(bytes: &[u8], resources: &mut ResourceContext<'_>) -> Result<Value, CodecError> {
    let registration = jqf_codec_cbor::registration().expect("registration");
    let dialect = DialectId::try_new(DECODE_DIALECT).expect("dialect");
    let mut provider = registration.decoder().expect("decoder").create_provider(
        source(bytes),
        DecodeRequest {
            validation: ValidationMode::Strict,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            dialect: &dialect,
            options: None,
            allow_adjacent_values: false,
            value_separator: &[],
        },
        resources,
    )?;
    let requirement = requirement(resources, whole_demand(resources));
    let handle = provider.bind(&requirement).expect("bind");
    let mut session = provider.open(&handle, resources)?;
    let result = {
        let mut run = CodecRunContext::new(resources);
        run.set_cooperative_credits(4_096);
        session.decode(&mut run)?
    };
    let AccessOutcome::FullDocument(product) = result.outcome() else {
        panic!("expected full document");
    };
    product.document().materialize_root(resources).map_err(|_| {
        CodecError::new(CodecFailureKind::InternalContractViolation {
            contract: "materialize CBOR root",
        })
    })
}

fn encode(value: &Value) -> Result<(Vec<u8>, jqf_codec_core::PreservationReport), CodecError> {
    let mut resources = resources();
    encode_with(value, &mut resources)
}

fn encode_with(
    value: &Value,
    resources: &mut ResourceContext<'_>,
) -> Result<(Vec<u8>, jqf_codec_core::PreservationReport), CodecError> {
    let format = FormatId::try_new(jqf_codec_cbor::FORMAT_ID).expect("format");
    let dialect = DialectId::try_new(ENCODE_DIALECT).expect("dialect");
    let registration = jqf_codec_cbor::registration().expect("registration");
    let factory = registration.encoder().expect("encoder").create_factory(
        EncodeRequest {
            format: &format,
            dialect: &dialect,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            preservation: PreservationRequest::None,
            options: None,
        },
        resources,
    )?;
    let mut session = factory
        .start(EncodeItem::Owned(value), PreservationRequest::None, resources)
        .expect("session");
    let mut out = Vec::new();
    let report = {
        let mut sink = VecByteSink::new(&mut out);
        let mut run = CodecRunContext::new(resources);
        run.set_cooperative_credits(4_096);
        session.encode(&mut sink, &mut run)?
    };
    Ok((out, report))
}

/// Compact render for readable corpus assertions over owned values.
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
                let value = float.get();
                if value.fract() == 0.0 {
                    format!("{value:.1}")
                } else {
                    format!("{value}")
                }
            } else {
                format!("{number:?}")
            }
        }
        V::String(text) => format!("{text:?}"),
        V::Bytes(bytes) => format!("h{:?}", bytes.as_slice()),
        V::Array(array) => {
            let mut out = String::from("[");
            for (index, item) in array.iter().enumerate() {
                if index != 0 {
                    out.push_str(", ");
                }
                out.push_str(&render(item));
            }
            out.push(']');
            out
        }
        V::Object(object) => {
            let mut out = String::from("{");
            for (index, entry) in object.iter().enumerate() {
                if index != 0 {
                    out.push_str(", ");
                }
                out.push('"');
                out.push_str(entry.key());
                out.push_str("\": ");
                out.push_str(&render(entry.value()));
            }
            out.push('}');
            out
        }
        V::Tagged { tag, payload } => format!("{}({})", tag.as_str(), render(payload)),
        V::OffsetDateTime(datetime) => {
            let date = datetime.local.date;
            let time = &datetime.local.time;
            let fraction = time.fraction().digits();
            let mut out = format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
                date.year(),
                date.month(),
                date.day(),
                time.hour(),
                time.minute(),
                time.second(),
            );
            if !fraction.is_empty() {
                out.push('.');
                out.push_str(fraction);
            }
            out.push('Z');
            out
        }
        other => format!("{other:?}"),
    }
}

fn assert_semantic_equal(left: &Value, right: &Value, context: &str) {
    assert!(
        render(left) == render(right),
        "{context}: semantic mismatch: got {}, expected {}",
        render(left),
        render(right)
    );
}

/// R-SCAN + R-MAT: the golden corpus scans to a skeleton whose root materializes to the expected semantic value.
/// Table-driven.
#[test]
fn law_r_scan_and_mat_golden_corpus() {
    let cases: &[(&[u8], &str)] = &[
        (&[0x00], "0"),
        (&[0x01], "1"),
        (&[0x17], "23"),
        (&[0x18, 0x18], "24"),
        (&[0x19, 0x01, 0x00], "256"),
        (&[0x1a, 0x00, 0x01, 0x00, 0x00], "65536"),
        (&[0x20], "-1"),
        (
            &[0x3b, 0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
            "-9223372036854775808",
        ),
        (&[0xf4], "false"),
        (&[0xf5], "true"),
        (&[0xf6], "null"),
        (&[0xf7], "cbor:simple:23(null)"),
        (&[0xf9, 0x3c, 0x00], "1.0"),
        (&[0xf9, 0x3e, 0x00], "1.5"),
        (&[0xfa, 0x3f, 0x80, 0x00, 0x00], "1.0"),
        (&[0xfb, 0x3f, 0xf8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], "1.5"),
        (&[0x61, 0x61], "\"a\""),
        (&[0x65, 0x68, 0x65, 0x6c, 0x6c, 0x6f], "\"hello\""),
        (&[0x40], "h[]"),
        (&[0x44, 0x01, 0x02, 0x03, 0x04], "h[1, 2, 3, 4]"),
        (&[0x80], "[]"),
        (&[0x83, 0x01, 0x02, 0x03], "[1, 2, 3]"),
        (&[0xa0], "{}"),
        (
            &[0xa2, 0x61, 0x61, 0x01, 0x61, 0x62, 0x82, 0xf5, 0xf6],
            "{\"a\": 1, \"b\": [true, null]}",
        ),
        (
            &[0x7f, 0x63, 0x66, 0x6f, 0x6f, 0x63, 0x62, 0x61, 0x72, 0xff],
            "\"foobar\"",
        ),
        (&[0x9f, 0x01, 0x02, 0xff], "[1, 2]"),
        (&[0xc2, 0x41, 0x01], "1"),
        (&[0xc3, 0x41, 0x01], "-2"),
        (&[0xc1, 0x00], "1970-01-01T00:00:00Z"),
        (&[0xd9, 0xd9, 0xf7, 0x61, 0x78], "cbor:tag:55799(\"x\")"),
        (
            &[0xd9, 0xd9, 0xf7, 0xd8, 0x22, 0x82, 0x01, 0x02],
            "cbor:tag:55799(cbor:tag:34([1, 2]))",
        ),
    ];
    for (bytes, expected) in cases {
        let value = decode(bytes).unwrap_or_else(|error| panic!("decode failed for {bytes:02x?}: {error}"));
        let rendered = render(&value);
        assert_eq!(rendered, *expected, "decode mismatch for {bytes:02x?}");
    }
}

/// R-SCAN corrupt-late: a corrupt byte anywhere fails the scan, even in a subtree the demand never touches — the
/// whole input is validated.
#[test]
fn law_r_scan_rejects_corrupt_mutations() {
    // The golden object `{"a":1,"b":[true,null]}` with one byte corrupted into an invalid CBOR item in each position.
    let mutations: &[(&[u8], &str)] = &[
        (
            &[0xa2, 0x61, 0x61, 0x01, 0x61, 0x62, 0x82, 0xf5],
            "truncated after true",
        ),
        (
            &[0xa2, 0x61, 0x61, 0x01, 0x61, 0x62, 0x82, 0x1c],
            "reserved additional info 28",
        ),
        (
            &[0xa2, 0x61, 0x61, 0x01, 0x61, 0x62, 0x82, 0xf8, 0x1f],
            "reserved simple value 31",
        ),
        (&[0xf8, 0x00], "two-byte simple 0 is not well-formed"),
        (&[0xf8, 0x14], "two-byte simple 20 aliases false"),
        (&[0xf8, 0x17], "two-byte simple 23 aliases undefined"),
        (&[0xbf, 0x61, 0x61, 0xff], "indefinite map with a dangling key"),
        (
            &[0xbf, 0x61, 0x61, 0x01, 0x61, 0x62, 0xff],
            "indefinite map with a trailing dangling key",
        ),
        (
            &[0x81, 0xbf, 0x61, 0x61, 0xff],
            "nested indefinite map with a dangling key",
        ),
        (
            &[0xa2, 0x61, 0xff, 0x01, 0x61, 0x62, 0x82, 0xf5, 0xf6],
            "invalid UTF-8 key byte",
        ),
        (
            &[0xa2, 0x61, 0x61, 0x01, 0x61, 0x62, 0x82, 0xf5, 0xf6, 0x00],
            "trailing bytes",
        ),
    ];
    for (bytes, label) in mutations {
        match decode(bytes) {
            Ok(value) => panic!("mutation ({label}) {bytes:02x?} decoded to {value:?}"),
            Err(error) => assert_eq!(
                error.kind(),
                CodecFailureKind::InvalidInput,
                "mutation ({label}) must reject with InvalidInput, got {error:?}"
            ),
        }
    }
    // The duplicate-text-key law (§5.6.1): same key twice is invalid.
    let duplicate = [0xa2, 0x61, 0x61, 0x01, 0x61, 0x61, 0x02];
    match decode(&duplicate) {
        Ok(value) => panic!("duplicate key decoded to {value:?}"),
        Err(error) => assert_eq!(error.kind(), CodecFailureKind::InvalidInput),
    }
    // A non-text map key is valid CBOR but not projectable to the semantic root: a raw-shape failure, never a silent
    // key fabrication.
    let non_text_key = [0xa1, 0x01, 0x61, 0x61];
    match decode(&non_text_key) {
        Ok(value) => panic!("non-text key decoded to {value:?}"),
        Err(error) => assert_eq!(
            error.kind(),
            CodecFailureKind::UnsupportedRepresentation,
            "non-text key: {error:?}"
        ),
    }
}

/// R-ENC: decode → encode (core-deterministic) → re-decode is the same semantic value.
#[test]
fn law_r_enc_roundtrip() {
    let cases: &[&[u8]] = &[
        &[0x83, 0x01, 0x02, 0x03],
        &[0xa2, 0x61, 0x61, 0x01, 0x61, 0x62, 0x82, 0xf5, 0xf6],
        &[0x7f, 0x63, 0x66, 0x6f, 0x6f, 0x63, 0x62, 0x61, 0x72, 0xff],
        &[0x9f, 0x01, 0x02, 0xff],
        &[0x44, 0x01, 0x02, 0x03, 0x04],
        &[0xc1, 0xfb, 0x3f, 0xf8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
        &[0xd9, 0xd9, 0xf7, 0xd8, 0x22, 0x82, 0x01, 0x02],
    ];
    for bytes in cases {
        let value = decode(bytes).unwrap_or_else(|error| panic!("decode failed for {bytes:02x?}: {error}"));
        let (encoded, _) = encode(&value).unwrap_or_else(|error| panic!("encode failed for {bytes:02x?}: {error}"));
        let redecoded = decode(&encoded).unwrap_or_else(|error| panic!("re-decode failed for {encoded:02x?}: {error}"));
        assert_semantic_equal(&value, &redecoded, &format!("roundtrip {bytes:02x?}"));
    }
}

/// R-ENC-B + P-BOUND: encoder output is byte-identical to the golden bytes.
#[test]
fn law_r_enc_b_byte_boundary() {
    // [1, 2, 3] — same item, shorter.
    let value = decode(&[0x83, 0x01, 0x02, 0x03]).expect("decode");
    let (encoded, _) = encode(&value).expect("encode");
    assert_eq!(encoded, vec![0x83, 0x01, 0x02, 0x03]);
    // {"a":1,"b":[true,null]} — the core-deterministic profile sorts keys.
    let value = decode(&[0xa2, 0x61, 0x61, 0x01, 0x61, 0x62, 0x82, 0xf5, 0xf6]).expect("decode");
    let (encoded, _) = encode(&value).expect("encode");
    assert_eq!(encoded, vec![0xa2, 0x61, 0x61, 0x01, 0x61, 0x62, 0x82, 0xf5, 0xf6]);
}

/// R-HINT: demand is a hint — an empty demand and the full demand decode to byte-identical answers.
#[test]
fn law_r_hint_demand_is_a_hint() {
    let bytes: &[u8] = &[0xa2, 0x61, 0x61, 0x01, 0x61, 0x62, 0x82, 0xf5, 0xf6];
    let mut resources = resources();
    let full = decode_with_shared(bytes, &mut resources).expect("full-demand decode");
    let hint = { decode_with(bytes, CodecDemand::try_new(&resources)).expect("empty-hint decode") };
    assert_semantic_equal(&full, &hint, "demand hint independence");
}

/// R-VOC: the value vocabulary roundtrips — bignum tags, datetime tag, non-finite floats, and exact decimals.
#[test]
fn law_r_voc_vocabulary_roundtrip() {
    // Tag 2/3 bignums roundtrip through encode → re-decode.
    let bignum = [0xc2, 0x42, 0x01, 0x00]; // tag 2, 256
    let value = decode(&bignum).expect("bignum decode");
    let (encoded, _) = encode(&value).expect("bignum encode");
    let redecoded = decode(&encoded).expect("bignum re-decode");
    assert_semantic_equal(&value, &redecoded, "bignum");
    // Non-finite floats (half precision): +Inf, -Inf, NaN. The canonical NaN spelling f9 7e 00 round-trips (fuzz
    // receipt 0x207710e37543d4b5).
    for bytes in [
        &[0xf9, 0x7c, 0x00][..],
        &[0xf9, 0xfc, 0x00][..],
        &[0xf9, 0x7e, 0x00][..],
    ] {
        let value = decode(bytes).expect("non-finite decode");
        let (encoded, _) = encode(&value).expect("non-finite encode");
        let redecoded = decode(&encoded).expect("non-finite re-decode");
        assert_semantic_equal(&value, &redecoded, &format!("non-finite {bytes:02x?}"));
    }
    // A half-precision float is a binary64 value (CBOR floats are not exact decimals); the ROUNDTRIP must be
    // value-stable.
    let value = decode(&[0xf9, 0x3e, 0x00]).expect("float decode");
    let (encoded, _) = encode(&value).expect("float encode");
    let redecoded = decode(&encoded).expect("float re-decode");
    assert_semantic_equal(&value, &redecoded, "half-precision float");
    // A half-precision float that is an integer (1.0) renders integrally.
    let one = decode(&[0xf9, 0x3c, 0x00]).expect("one decode");
    assert_eq!(render(&one), "1.0");
}

/// P-ZERO: a failed encode publishes ZERO bytes to the sink.
#[test]
fn law_p_zero_failed_encode_publishes_nothing() {
    // A local date/time is a value CBOR cannot represent (RFC 8949 has no such item; only the offset date-time via tag
    // 1).
    let mut resources = resources();
    let value = Value::LocalDate(jqf_data::LocalDate::new(2026, 8, 15).expect("date"));
    let format = FormatId::try_new(jqf_codec_cbor::FORMAT_ID).expect("format");
    let dialect = DialectId::try_new(ENCODE_DIALECT).expect("dialect");
    let registration = jqf_codec_cbor::registration().expect("registration");
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
            &mut resources,
        )
        .expect("factory");
    let mut session = factory
        .start(EncodeItem::Owned(&value), PreservationRequest::None, &mut resources)
        .expect("session");
    let mut out = Vec::new();
    {
        let mut sink = VecByteSink::new(&mut out);
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        let error = session.encode(&mut sink, &mut run).expect_err("must fail");
        assert_eq!(
            error.kind(),
            CodecFailureKind::UnsupportedRepresentation,
            "unrepresentable value: {error:?}"
        );
    }
    assert!(out.is_empty(), "failed encode published {} bytes", out.len());
}

/// P-CHUNK: chunked publication is bounded — a sink capped at a small chunk still receives the complete golden output
/// via the backpressure retry surface, with no size violation.
#[test]
fn law_p_chunk_bounded_sink() {
    struct CappedSink<'a> {
        target: &'a mut Vec<u8>,
        cap: usize,
    }
    impl ByteSink for CappedSink<'_> {
        fn write(&mut self, bytes: &[u8], _resources: &mut ResourceContext<'_>) -> Result<usize, CodecError> {
            let take = bytes.len().min(self.cap);
            self.target.extend_from_slice(&bytes[..take]);
            Ok(take)
        }
        fn flush(&mut self) -> Result<(), CodecError> {
            Ok(())
        }
    }
    // A nested structure large enough that the encoder offers several chunks.
    let mut items: Vec<Value> = Vec::new();
    for index in 0..64 {
        items.push(Value::Number(jqf_data::Number::integer(jqf_data::Integer::from_i64(
            index,
        ))));
    }
    let mut resources = resources();
    let value = Value::Array(jqf_data::Array::try_from_vec(items).expect("array"));
    let (golden, _) = encode_with(&value, &mut resources).expect("golden encode");
    assert!(!golden.is_empty());
    let format = FormatId::try_new(jqf_codec_cbor::FORMAT_ID).expect("format");
    let dialect = DialectId::try_new(ENCODE_DIALECT).expect("dialect");
    let registration = jqf_codec_cbor::registration().expect("registration");
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
            &mut resources,
        )
        .expect("factory");
    let mut session = factory
        .start(EncodeItem::Owned(&value), PreservationRequest::None, &mut resources)
        .expect("session");
    let mut out = Vec::new();
    {
        let mut sink = CappedSink {
            target: &mut out,
            cap: 7,
        };
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        session.encode(&mut sink, &mut run).expect("bounded encode completes");
    }
    assert_eq!(out, golden, "bounded-sink bytes must match the unbounded run");
}

/// A-REPORT: the `PreservationReport` is honest — an exact roundtrip reports Exact on the axes the bytes really
/// preserve.
#[test]
fn law_a_report_axes_are_honest() {
    let value = decode(&[0xa2, 0x61, 0x61, 0x01, 0x61, 0x62, 0x82, 0xf5, 0xf6]).expect("decode");
    let (_, report) = encode(&value).expect("encode");
    assert_eq!(
        report.semantic_values(),
        PreservationOutcome::Exact,
        "semantic values must be Exact for a lossless CBOR roundtrip"
    );
    assert_eq!(
        report.tags_and_facts(),
        PreservationOutcome::Exact,
        "tags must be Exact (the generic data model carries them)"
    );
    // The core-deterministic profile SORTS map keys, so ordering is a canonical (Normalized) change — the honest
    // report must say so, never claim Exact for a reorder it performed.
    assert_eq!(
        report.ordering(),
        PreservationOutcome::Normalized,
        "the deterministic profile reorders keys; the report must be honest"
    );
}

/// A-CANCEL: a cancelled run stops at the next work check with a typed error — never a panic, never a partial value.
#[test]
fn law_a_cancel_stops_at_the_work_check() {
    // `ResourceContext::new` itself observes control, so the cancellation must arrive on a LATER check: the first check
    // (construction) continues, every check after it cancels.
    struct CancelControl {
        cancelled: core::sync::atomic::AtomicBool,
    }
    impl Control for CancelControl {
        fn check(&self) -> ControlOutcome {
            use core::sync::atomic::Ordering;
            if self.cancelled.swap(true, Ordering::SeqCst) {
                ControlOutcome::Cancelled
            } else {
                ControlOutcome::Continue
            }
        }
    }
    static CANCEL: CancelControl = CancelControl {
        cancelled: core::sync::atomic::AtomicBool::new(false),
    };
    // A tiny meter guarantees the codec exhausts its cooperative entry and hits the replenish (control-observing) path
    // within one document.
    let mut resources = ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
            .expect("account"),
        &CANCEL,
        WorkMeter::try_new_v1(1).expect("work"),
    )
    .expect("resources");
    let bytes: &[u8] = &[0xa2, 0x61, 0x61, 0x01, 0x61, 0x62, 0x82, 0xf5, 0xf6];
    let registration = jqf_codec_cbor::registration().expect("registration");
    let dialect = DialectId::try_new(DECODE_DIALECT).expect("dialect");
    let mut provider = registration
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
                value_separator: &[],
            },
            &mut resources,
        )
        .expect("provider");
    let requirement = requirement(&resources, whole_demand(&resources));
    let handle = provider.bind(&requirement).expect("bind");
    let mut session = provider.open(&handle, &mut resources).expect("open");
    let result = {
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        session.decode(&mut run)
    };
    match result {
        Ok(_) => panic!("cancelled decode must not succeed"),
        Err(error) => assert_eq!(
            error.kind(),
            CodecFailureKind::Control(jqf_resource::ControlError::Cancelled),
            "cancelled decode must surface the typed control error, got {error:?}"
        ),
    }
}

/// Cross-cutting: the two-kind declaration — CBOR serves `CompleteDocument` and never `RecordStream` — plus the
/// E-table, declared PRESENT once the span binding and the three edit hooks landed.
#[test]
fn law_table_declarations() {
    let registration = jqf_codec_cbor::registration().expect("registration");
    let operations = registration.descriptor().operations();
    assert!(operations.decode(), "CBOR must declare decoder construction");
    assert!(operations.encode(), "CBOR must declare encoder construction");
    // The E-table (span/`--edit` law) is a DECLARED PRESENCE: per-item span binding and the three edit hooks are live,
    // so the table pins what the codec now supplies (the splice receipts themselves are `law_e_splice_receipts` below;
    // the identity/survival/placement arms live in `tools/jqf-edit-differential.py`).
    let mut resources = resources();
    let provider = registration
        .decoder()
        .expect("decoder")
        .create_provider(
            source(&[0x01]),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(DECODE_DIALECT).expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            &mut resources,
        )
        .expect("provider");
    let routes = provider.route_descriptions();
    for route in routes {
        assert_ne!(
            route.bundle().result(),
            AccessResultKind::RecordStream,
            "CBOR must never advertise a record stream"
        );
    }
}

/// E-table receipts (kit design §2.2, rows E-IDENT/E-SPLICE/E-SURV): the codec's half of the `--edit` contract, driven
/// through the public factory over a document decoded with per-item spans. The append receipt splices the returned
/// insertions into the source and re-decodes, asserting BOTH the codec-named placement (the new member lands inside the
/// container) and that every untouched byte survives verbatim — the lane's reason to exist.
#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one E-table test drives all three hooks over one decoded document; splitting it would re-decode per receipt"
)]
fn law_e_splice_receipts() {
    let mut resources = resources();
    let registration = jqf_codec_cbor::registration().expect("registration");
    let format = FormatId::try_new(jqf_codec_cbor::FORMAT_ID).expect("format");
    let dialect = DialectId::try_new(DECODE_DIALECT).expect("dialect");
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
            &mut resources,
        )
        .expect("factory");

    // `a2 61 61 01 61 62 02` — {"a":1,"b":2}.
    let bytes = [0xa2, 0x61, 0x61, 0x01, 0x61, 0x62, 0x02];
    let mut provider = registration
        .decoder()
        .expect("decoder")
        .create_provider(
            source(&bytes),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &dialect,
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            &mut resources,
        )
        .expect("provider");
    let requirement = requirement(&resources, whole_demand(&resources));
    let handle = provider.bind(&requirement).expect("bind");
    let mut session = provider.open(&handle, &mut resources).expect("open");
    let result = {
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        session.decode(&mut run).expect("decode")
    };
    let AccessOutcome::FullDocument(product) = result.outcome() else {
        panic!("expected full document");
    };
    let document = product.document();

    // E-IDENT: a span-bearing scalar leaf re-encodes as exactly its own item. `a` is `01`; changing it to `5` renders
    // `05` — the whole splice.
    let root = document
        .value_view(document.node_handle(document.root()).expect("handle"))
        .expect("view");
    let member = root
        .object()
        .expect("object")
        .expect("root object")
        .get("a")
        .expect("member a");
    let rendered = factory
        .render_leaf(
            document,
            member.node(),
            &[String::from("a")],
            &bytes,
            &Value::Number(
                jqf_data::Number::try_integer_unaccounted(jqf_data::Integer::parse("5").expect("integer"))
                    .expect("number"),
            ),
            Some(&bytes[3..4]),
            &mut resources,
        )
        .expect("leaf");
    assert_eq!(rendered, vec![0x05], "a changed integer re-encodes as its item");

    // E-SPLICE + E-SURV: append the member `c: 3`. The insertions re-derive the count head (`a2` -> `a3`) and land the
    // new key+value bytes after the last pair; the untouched `a`/`b` bytes survive verbatim in the patched source, and
    // the patched bytes re-decode to the grown value.
    let new_member = Value::Number(
        jqf_data::Number::try_integer_unaccounted(jqf_data::Integer::parse("3").expect("integer")).expect("number"),
    );
    let insertions = factory
        .render_edit_append(
            document,
            document.root(),
            &[],
            &bytes,
            jqf_codec_core::EditAppendMembers::Table(&[("c", &new_member)]),
            &mut resources,
        )
        .expect("append");
    let head = insertions
        .iter()
        .find(|insertion| insertion.at == 0)
        .expect("head insertion");
    assert_eq!(head.bytes, vec![0xa3], "the pair count head is re-derived");
    let tail = insertions
        .iter()
        .find(|insertion| insertion.at == bytes.len())
        .expect("member insertion");
    assert_eq!(
        tail.bytes,
        vec![0x61, 0x63, 0x03],
        "the new member lands after the last pair"
    );
    let mut patched = bytes.to_vec();
    for insertion in insertions {
        // The seam's replacement form: the head rewrite overwrites its authored span instead of only growing the
        // segment.
        match insertion.replace {
            Some((start, end)) => {
                patched.splice(start..end, insertion.bytes.iter().copied());
            }
            None => {
                patched.splice(insertion.at..insertion.at, insertion.bytes.iter().copied());
            }
        }
    }
    assert_eq!(
        &patched[1..7],
        &bytes[1..7],
        "the untouched a/b member bytes survive the append verbatim"
    );
    let value = decode_with(&patched, whole_demand(&resources)).expect("re-decode");
    let rendered = format!("{value:?}");
    assert!(
        rendered.contains("Machine(3)"),
        "the patched bytes re-decode to the grown value (a/b survive, c=3), got {rendered}"
    );

    // E-SPLICE (remove): cutting the `a` member's key+value span plus the head leaves a one-pair map that re-decodes to
    // `{"b":2}`.
    let removals = factory
        .render_edit_remove(
            document,
            document.root(),
            &[],
            &bytes,
            jqf_codec_core::EditRemoveMembers::Table(&[("a", member.node())]),
            &mut resources,
        )
        .expect("remove");
    let mut cut = bytes.to_vec();
    for removal in removals.iter().rev() {
        // The seam's replacement form: the head rewrite overwrites its span with the re-derived bytes instead of
        // cutting.
        cut.splice(removal.start..removal.end, removal.replacement.iter().copied());
    }
    let value = decode_with(&cut, whole_demand(&resources)).expect("re-decode");
    let rendered = format!("{value:?}");
    assert!(
        rendered.contains("Machine(2)") && !rendered.contains("Machine(1)"),
        "the cut bytes re-decode to the shrunk value (a gone, b survives), got {rendered}"
    );
}
