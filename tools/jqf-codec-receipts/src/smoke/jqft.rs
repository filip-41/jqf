//! jqft family codec receipt battery (the jqft vertical's gate).
//!
//! Pins the wave-1 core laws: the one-slot route inventory (Whole/
//! `CompleteDocument`) for BOTH formats, core-value round-trip identity,
//! canonical byte law (`%jqft 1` header, two-space layout, `f`-suffixed
//! floats, `0x"…"` bytes, TOML-shaped temporals, `@tag` layers retained),
//! the dedicated reserved-spelling refusals, and the jqfjson strict-JSON
//! one-document-per-source law.

use crate::drive::resources;
use jqf_codec_core::{
    AccessGuarantees, AccessOutcome, AccessRequirement, AccessResultKind, CodecDemand, CodecFailureKind,
    CodecRunContext, DecodeRequest, DiagnosticPolicy, EncodeItem, EncodeRequest, ValidationMode,
};
use jqf_data::{DialectId, FormatId, Value};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

/// Decodes one document through the whole-document route, returning the
/// materialized root value or the failure kind.
fn whole_value(bytes: &[u8], format: &str, _dialect: &str) -> Result<Value, CodecFailureKind> {
    let registration = match format {
        jqf_codec_jqft::FORMAT_ID => jqf_codec_jqft::registration_jqft(),
        jqf_codec_jqft::JQFJSON_FORMAT_ID => jqf_codec_jqft::registration_jqfjson(),
        _ => unreachable!("smoke formats"),
    }
    .map_err(|_e| CodecFailureKind::InternalContractViolation {
        contract: "jqft registration",
    })?;
    let mut resources = resources();
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "smoke.jqft",
        bytes,
        0,
    );
    let mut provider = registration
        .decoder()
        .expect("decoder")
        .create_provider(
            source,
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(jqf_codec_jqft::JQFT_CANONICAL_DIALECT_ID).expect("dialect"),
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
    let AccessOutcome::FullDocument(product) = result.outcome() else {
        return Err(CodecFailureKind::RequirementMismatch);
    };
    product
        .document()
        .materialize_root(&mut resources)
        .map_err(|_| CodecFailureKind::UnsupportedRepresentation)
}

/// Encodes one owned value to canonical bytes.
fn encode_value(value: &Value, format: &str, dialect: &str) -> Result<Vec<u8>, CodecFailureKind> {
    let registration = match format {
        jqf_codec_jqft::FORMAT_ID => jqf_codec_jqft::registration_jqft(),
        jqf_codec_jqft::JQFJSON_FORMAT_ID => jqf_codec_jqft::registration_jqfjson(),
        _ => unreachable!("smoke formats"),
    }
    .map_err(|_e| CodecFailureKind::InternalContractViolation {
        contract: "jqft registration",
    })?;
    let mut resources = resources();
    let request = EncodeRequest {
        format: &FormatId::try_new(format).map_err(|_| CodecFailureKind::RequirementMismatch)?,
        dialect: &DialectId::try_new(dialect).map_err(|_| CodecFailureKind::RequirementMismatch)?,
        diagnostics: DiagnosticPolicy::ErrorsOnly,
        preservation: jqf_codec_core::PreservationRequest::None,
        options: None,
    };
    let factory = registration
        .encoder()
        .expect("encoder")
        .create_factory(request, &mut resources)
        .map_err(|error| error.kind())?;
    let mut session = factory
        .start(
            EncodeItem::owned(value),
            jqf_codec_core::PreservationRequest::None,
            &mut resources,
        )
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
        // `as_slice` over `as_ref`: winnow (via the harness's pinned `toml`
        // oracle dep) implements `AsRef` for `[u8]`, so `as_ref` is ambiguous
        // once the toml differential references that crate. Same bytes.
        V::Bytes(bytes) => format!("h{:?}", bytes.as_slice()),
        V::LocalDate(date) => format!("ld{}", date.year()),
        V::LocalTime(time) => format!("lt{}:{}", time.hour(), time.minute()),
        V::LocalDateTime(datetime) => format!("ldt{}", datetime.date.year()),
        V::OffsetDateTime(datetime) => format!("odt{}", datetime.local.date.year()),
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

fn decoded(bytes: &[u8]) -> Result<Value, String> {
    whole_value(bytes, "jqft", "jqft.document@1")
        .map_err(|kind| format!("decode: {kind:?} for {:?}", String::from_utf8_lossy(bytes)))
}

pub fn run() -> Result<(), String> {
    assert_route_inventory()?;
    assert_jqfjson_route_inventory()?;
    assert_jqfb_route_inventory()?;
    assert_round_trip()?;
    assert_encode_byte_law()?;
    assert_reserved_spellings();
    assert_jqfjson_laws()?;
    println!(
        "codec-jqft-smoke: routes=1 jqfjson_routes=1 jqfb_routes=2 roundtrip=true encode=true reserved=true jqfjson=true receipts=true"
    );
    Ok(())
}

/// The one-slot route inventory (the vertical's slot duty, mirrored in
/// `jqf-sdk-smoke`): Whole/CompleteDocument, and nothing else — every richer
/// demand is served by the core's generic exact adapter over the whole route.
fn assert_route_inventory() -> Result<(), String> {
    let mut resources = resources();
    let registration = jqf_codec_jqft::registration_jqft().map_err(|error| format!("{error:?}"))?;
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(2), SourceKind::Input),
        "routes.jqft",
        b"%jqft 1\na: 1\n",
        0,
    );
    let provider = registration
        .decoder()
        .expect("decoder")
        .create_provider(
            source,
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(jqf_codec_jqft::JQFT_CANONICAL_DIALECT_ID).expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            &mut resources,
        )
        .map_err(|error| format!("provider: {:?}", error.kind()))?;
    let routes = provider.route_descriptions();
    let kinds: Vec<(u32, jqf_codec_core::AccessFootprintKind, AccessResultKind)> = routes
        .iter()
        .map(|route| (route.slot().get(), route.bundle().footprint(), route.bundle().result()))
        .collect();
    let expected = [(
        0,
        jqf_codec_core::AccessFootprintKind::Whole,
        AccessResultKind::CompleteDocument,
    )];
    if kinds != expected {
        return Err(format!("jqft route inventory drifted: {kinds:?}"));
    }
    Ok(())
}

/// The jqfb two-slot route inventory (plan 118 V7b): Whole/CompleteDocument
/// plus the native `Located` scoped route the
/// node-table walk serves. Mirrored in `jqf-sdk-smoke` per the route-slot
/// duty.
fn assert_jqfb_route_inventory() -> Result<(), String> {
    let mut resources = resources();
    let registration = jqf_codec_jqft::registration_jqfb().map_err(|error| format!("{error:?}"))?;
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(4), SourceKind::Input),
        "routes.jqfb",
        &[],
        0,
    );
    let provider = registration
        .decoder()
        .expect("decoder")
        .create_provider(
            source,
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(jqf_codec_jqft::JQFT_CANONICAL_DIALECT_ID).expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            &mut resources,
        )
        .map_err(|error| format!("provider: {:?}", error.kind()))?;
    let routes = provider.route_descriptions();
    let kinds: Vec<(u32, jqf_codec_core::AccessFootprintKind, AccessResultKind)> = routes
        .iter()
        .map(|route| (route.slot().get(), route.bundle().footprint(), route.bundle().result()))
        .collect();
    let expected = [
        (
            0,
            jqf_codec_core::AccessFootprintKind::Whole,
            AccessResultKind::CompleteDocument,
        ),
        (1, jqf_codec_core::AccessFootprintKind::Exact, AccessResultKind::Located),
    ];
    if kinds != expected {
        return Err(format!("jqfb route inventory drifted: {kinds:?}"));
    }
    Ok(())
}

/// The jqfjson one-slot route inventory.
fn assert_jqfjson_route_inventory() -> Result<(), String> {
    let mut resources = resources();
    let registration = jqf_codec_jqft::registration_jqfjson().map_err(|error| format!("{error:?}"))?;
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(3), SourceKind::Input),
        "routes.jqfjson",
        b"{\"a\":1}\n",
        0,
    );
    let provider = registration
        .decoder()
        .expect("decoder")
        .create_provider(
            source,
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(jqf_codec_jqft::JQFT_CANONICAL_DIALECT_ID).expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            &mut resources,
        )
        .map_err(|error| format!("provider: {:?}", error.kind()))?;
    let routes = provider.route_descriptions();
    if routes.len() != 1 || routes[0].slot().get() != 0 {
        return Err(format!("jqfjson route inventory drifted: {:?}", routes.len()));
    }
    Ok(())
}

/// decode -> encode -> decode round-trip identity over the wave-1 core
/// vocabulary: scalars, containers, floats, bytes, temporals, tags.
fn assert_round_trip() -> Result<(), String> {
    let cases: [&[u8]; 14] = [
        b"%jqft 1\nnull\n",
        b"%jqft 1\ntrue\n",
        b"%jqft 1\n42\n",
        b"%jqft 1\n-7\n",
        b"%jqft 1\n1250.00\n",
        b"%jqft 1\n1e3\n",
        b"%jqft 1\n\"hello, \\\"world\\\"\"\n",
        b"%jqft 1\n2.5f\n",
        b"%jqft 1\ninf\n",
        b"%jqft 1\n0x\"9f86d081884c7d65\"\n",
        b"%jqft 1\nb64\"aGVsbG8gd29ybGQ=\"\n",
        b"%jqft 1\n2026-08-02T21:14:00+02:00\n",
        b"%jqft 1\n[1, 2, [true, null]]\n",
        b"%jqft 1\n{name: \"ada\", id: @tag(\"uuid\") \"0198\", tags: [\"a\", \"b\"]}\n",
    ];
    for bytes in cases {
        let first = decoded(bytes)?;
        let encoded =
            encode_value(&first, "jqft", "jqft.canonical@1").map_err(|kind| format!("encode failed: {kind:?}"))?;
        let second = decoded(&encoded)?;
        if render(&first) != render(&second) {
            return Err(format!(
                "jqft round-trip drifted: {:?} -> {}",
                String::from_utf8_lossy(bytes),
                String::from_utf8_lossy(&encoded)
            ));
        }
    }
    Ok(())
}

/// The canonical renderer's byte law: header, layout, suffixes.
fn assert_encode_byte_law() -> Result<(), String> {
    let value = decoded(b"%jqft 1\n{a: 1, b: [true, \"x\"]}\n")?;
    let encoded =
        encode_value(&value, "jqft", "jqft.canonical@1").map_err(|kind| format!("encode failed: {kind:?}"))?;
    let text = String::from_utf8_lossy(&encoded);
    let expected = "%jqft 1\n{\n  a: 1,\n  b: [\n    true,\n    \"x\"\n  ]\n}";
    if text != expected {
        return Err(format!("canonical bytes drifted: got {text:?}"));
    }
    // Floats carry the `f` suffix under the shared ryu formatter (the memo's
    // jqf.float64-ryu@1); non-finite tokens are bare.
    let floats = decoded(b"%jqft 1\n{small: 2.5f, big: 1.5e-3f, huge: inf}\n")?;
    let encoded =
        encode_value(&floats, "jqft", "jqft.canonical@1").map_err(|kind| format!("encode failed: {kind:?}"))?;
    let text = String::from_utf8_lossy(&encoded);
    if !text.contains("2.5f") || !text.contains("0.0015f") || !text.contains("inf") {
        return Err(format!("float suffix law drifted: {text:?}"));
    }
    // The `---` stream separator and the header appear once: the whole
    // route publishes the FIRST document and the trailing `--- {b: 2}` is
    // the next stream item (the step's `document_end` lands at `{b: 2}`'s
    // start), so the decoded value is exactly `{a: 1}`.
    let stream = decoded(b"%jqft 1\n{a: 1}\n---\n{b: 2}\n")?;
    let rendered = render(&stream);
    let expected = render(&decoded(b"%jqft 1\n{a: 1}\n")?);
    if rendered != expected {
        return Err(format!(
            "stream separator law drifted: got {rendered:?}, expected the first document {expected:?}"
        ));
    }
    Ok(())
}

/// Reserved spellings refuse with a dedicated diagnostic (never a generic
/// parse error, never silent bytes); the wave-2 spellings that landed parse.
fn assert_reserved_spellings() {
    // Wave 2 (074): the markup angle form and the bracket-key form are now
    // GRAMMAR — markup parses to its children array (the 073 array model),
    // a bracket key is the non-string-key form.
    assert!(
        whole_value(b"%jqft 1\n<p \"text\">\n", "jqft", "jqft.document@1").is_ok(),
        "markup now parses (074 wave 2)"
    );
    assert!(
        whole_value(b"%jqft 1\n{(\"a\"): 1}\n", "jqft", "jqft.document@1").is_ok(),
        "the bracket key form now parses (074 wave 2)"
    );
    assert!(
        whole_value(b"%jqft 1\n&anchor\n", "jqft", "jqft.document@1").is_err(),
        "anchors are reserved"
    );
    assert!(
        whole_value(b"%jqft 1\n42\n", "jqft", "jqft.document@1").is_ok(),
        "a core value decodes"
    );
}

/// jqfjson: strict JSON, one document per source, canonical compact output.
fn assert_jqfjson_laws() -> Result<(), String> {
    let first = whole_value(b"{\"a\":1,\"b\":[true,null,\"x\"]}\n", "jqfjson", "jqfjson.document@1")
        .map_err(|kind| format!("jqfjson decode: {kind:?}"))?;
    let encoded =
        encode_value(&first, "jqfjson", "jqfjson.canonical@1").map_err(|kind| format!("jqfjson encode: {kind:?}"))?;
    if encoded != b"{\"a\":1,\"b\":[true,null,\"x\"]}" {
        return Err(format!(
            "jqfjson canonical bytes drifted: {:?}",
            String::from_utf8_lossy(&encoded)
        ));
    }
    assert!(
        whole_value(b"{\"a\":1} extra\n", "jqfjson", "jqfjson.document@1").is_err(),
        "trailing content must be rejected"
    );
    assert!(
        whole_value(b"{\"a\":1,}\n", "jqfjson", "jqfjson.document@1").is_err(),
        "jqfjson has no trailing commas"
    );
    // A binary64 float cannot be spelled in plain JSON (jqft can).
    let float =
        whole_value(b"%jqft 1\n1.5f\n", "jqft", "jqft.document@1").map_err(|kind| format!("decode: {kind:?}"))?;
    assert!(
        encode_value(&float, "jqft", "jqft.canonical@1").is_ok(),
        "jqft spells a float"
    );
    Ok(())
}
