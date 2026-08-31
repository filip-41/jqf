//! YAML codec receipt battery (the YAML vertical's gate).
//!
//! Pins the §4.8 laws the codec must hold: the two-slot route inventory,
//! schema resolution verdicts (failsafe/JSON/core), the number law (exact
//! integers, correctly-rounded floats, fixed NaN bits, the 1.2.2 core
//! int/float productions: leading zeros are integers, underscores are strings),
//! `yaml.key-
//! equivalence@1` duplicate-key behavior, the canonical renderer's byte law,
//! the block dialect's quoting round-trip, target tag validation, and
//! decode→encode→decode round-trip identity.

use crate::drive::{decode_session, exact_requirement, resources, source, whole_requirement};
use jqf_codec_core::{
    AccessAdapter, AccessOutcome, AccessResultKind, CodecFailureKind, CodecRunContext, DecodeRequest, DiagnosticPolicy,
    EncodeItem, EncodeRequest, ExactSelectionRecord, ValidationMode,
};
use jqf_data::{DialectId, FormatId, TagId, Value, ValueKind};

use jqf_codec_yaml::YamlTargetSchema;

/// Decodes one YAML document through the whole-document route, returning the
/// materialized root value or the failure kind.
fn whole_value(bytes: &[u8]) -> Result<Value, CodecFailureKind> {
    let registration = jqf_codec_yaml::registration().map_err(|_e| CodecFailureKind::InternalContractViolation {
        contract: "yaml registration",
    })?;
    whole_value_with(
        &registration,
        &DialectId::try_new(jqf_codec_yaml::YAML_CORE_DIALECT_ID).expect("dialect"),
        bytes,
    )
}

fn whole_value_with(
    registration: &jqf_codec_core::CodecRegistration<'static>,
    dialect: &DialectId,
    bytes: &[u8],
) -> Result<Value, CodecFailureKind> {
    let mut resources = resources();
    let mut provider = registration
        .decoder()
        .expect("decoder")
        .create_provider(
            source(bytes),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect,
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            &mut resources,
        )
        .map_err(|error| error.kind())?;
    let requirement = whole_requirement(&resources);
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

/// Encodes one owned value to YAML under the stream-canonical profile.
fn encode_yaml(value: &Value) -> Result<Vec<u8>, CodecFailureKind> {
    let registration = jqf_codec_yaml::registration().map_err(|_e| CodecFailureKind::InternalContractViolation {
        contract: "yaml registration",
    })?;
    let mut resources = resources();
    let options = YamlTargetSchema::Core;
    let format = FormatId::try_new(jqf_codec_yaml::FORMAT_ID).map_err(|_| CodecFailureKind::RequirementMismatch)?;
    let dialect = DialectId::try_new(jqf_codec_yaml::YAML_STREAM_CANONICAL_DIALECT_ID)
        .map_err(|_| CodecFailureKind::RequirementMismatch)?;
    let request = EncodeRequest {
        format: &format,
        dialect: &dialect,
        diagnostics: DiagnosticPolicy::ErrorsOnly,
        preservation: jqf_codec_core::PreservationRequest::None,
        options: Some(&options as &(dyn core::any::Any + Send + Sync)),
    };
    let factory = registration
        .encoder()
        .expect("encoder")
        .create_factory(request, &mut resources)
        .map_err(|error| error.kind())?;
    let mut session = factory
        .start(
            EncodeItem::Owned(value),
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

/// Block-profile encode of one owned root (the human-readable dialect).
fn encode_yaml_block(value: &Value) -> Result<Vec<u8>, CodecFailureKind> {
    let registration = jqf_codec_yaml::registration().map_err(|_e| CodecFailureKind::InternalContractViolation {
        contract: "yaml registration",
    })?;
    let mut resources = resources();
    let options = YamlTargetSchema::Core;
    let format = FormatId::try_new(jqf_codec_yaml::FORMAT_ID).map_err(|_| CodecFailureKind::RequirementMismatch)?;
    let dialect =
        DialectId::try_new(jqf_codec_yaml::YAML_BLOCK_DIALECT_ID).map_err(|_| CodecFailureKind::RequirementMismatch)?;
    let request = EncodeRequest {
        format: &format,
        dialect: &dialect,
        diagnostics: DiagnosticPolicy::ErrorsOnly,
        preservation: jqf_codec_core::PreservationRequest::None,
        options: Some(&options as &(dyn core::any::Any + Send + Sync)),
    };
    let factory = registration
        .encoder()
        .expect("encoder")
        .create_factory(request, &mut resources)
        .map_err(|error| error.kind())?;
    let mut session = factory
        .start(
            EncodeItem::Owned(value),
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

pub fn run() -> Result<(), String> {
    assert_route_inventory()?;
    assert_exact_member_binds_slot_1()?;
    assert_schema_laws()?;
    assert_number_law()?;
    assert_key_equivalence();
    assert_round_trip()?;
    assert_encode_byte_law()?;
    assert_block_round_trip()?;
    assert_tag_validator()?;
    println!(
        "codec-yaml-smoke: routes=2 exact=true schemas=true numbers=true keys=true duplicates=true roundtrip=true encode=true block-roundtrip=true tags=true receipts=true"
    );
    Ok(())
}

/// The two-slot route inventory (the vertical's slot duty, mirrored in
/// `jqf-sdk-smoke`).
fn assert_route_inventory() -> Result<(), String> {
    let mut resources = resources();
    let registration = jqf_codec_yaml::registration().map_err(|error| format!("{error:?}"))?;
    let provider = registration
        .decoder()
        .expect("decoder")
        .create_provider(
            source(b"a: 1\n"),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(jqf_codec_yaml::YAML_CORE_DIALECT_ID).expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            &mut resources,
        )
        .map_err(|error| format!("provider: {:?}", error.kind()))?;
    let routes = provider.route_descriptions();
    if routes.len() != 2 {
        return Err(format!("expected two routes, got {}", routes.len()));
    }
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
        return Err(format!("route inventory drifted: {kinds:?}"));
    }
    Ok(())
}

/// Slot 1 is Direct Exact: a member path binds Located, adapter none, not the whole-document fallback.
fn assert_exact_member_binds_slot_1() -> Result<(), String> {
    let mut resources = resources();
    let registration = jqf_codec_yaml::registration().map_err(|error| format!("{error:?}"))?;
    let mut provider = registration
        .decoder()
        .expect("decoder")
        .create_provider(
            source(b"a: 1\nb: 2\n"),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(jqf_codec_yaml::YAML_CORE_DIALECT_ID).expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            &mut resources,
        )
        .map_err(|error| format!("provider: {:?}", error.kind()))?;
    let requirement = exact_requirement(&resources, &["a"], None, None);
    let handle = provider
        .bind(&requirement)
        .map_err(|error| format!("exact bind: {error:?}"))?;
    if handle.slot().get() != 1 {
        return Err(format!("YAML Exact bind must use slot 1, got {}", handle.slot().get()));
    }
    if handle.demand_fallback() {
        return Err("YAML Exact bind must not be the whole-document demand fallback".into());
    }
    let mut session = provider
        .open(&handle, &mut resources)
        .map_err(|error| format!("open: {:?}", error.kind()))?;
    let result = decode_session(&mut session, &mut resources).map_err(|error| format!("decode: {:?}", error.kind()))?;
    if result.report().adapter() != AccessAdapter::None {
        return Err(format!(
            "YAML Exact adapter must be None, got {:?}",
            result.report().adapter()
        ));
    }
    let AccessOutcome::Located(located) = result.outcome() else {
        return Err(format!("YAML Exact must be Located, got {:?}", result.outcome()));
    };
    if !matches!(located.result(), ExactSelectionRecord::Node { .. }) {
        return Err(format!("YAML Exact member must be a node, got {:?}", located.result()));
    }
    Ok(())
}

/// Schema laws: core resolution, JSON-schema strictness, failsafe
/// unresolved-tag rejection, quoted-empty-string-is-str.
fn assert_schema_laws() -> Result<(), String> {
    // Core: plain scalars resolve to their categories.
    assert_kind(&decoded(b"42\n")?, ValueKind::Number, "core int")?;
    assert_kind(&decoded(b"3.5\n")?, ValueKind::Number, "core float")?;
    assert_kind(&decoded(b"true\n")?, ValueKind::Bool, "core bool")?;
    assert_kind(&decoded(b"null\n")?, ValueKind::Null, "core null")?;
    assert_kind(&decoded(b"hello\n")?, ValueKind::String, "core string")?;
    // Core empty plain scalar is null.
    assert_kind(&decoded(b"---\n")?, ValueKind::Null, "empty doc null")?;
    // Quoted empty scalar is !!str under every schema.
    assert_kind(&decoded(b"\"\"\n")?, ValueKind::String, "quoted empty str")?;
    // Explicit standard tags resolve.
    let tagged = decoded(b"!!str 123\n")?;
    assert_eq!(tagged.kind(), ValueKind::String, "!!str 123 is a string");
    // The failsafe schema resolves a plain `?` scalar to
    // `!!str` per YAML 1.2 §10.1.2 — `a: 1` decodes as the string "1", not
    // an unresolved-tag block (the declared dialect is unusable otherwise).
    let failsafe_registration =
        jqf_codec_yaml::registration().map_err(|error| format!("failsafe registration: {error:?}"))?;
    let failsafe = whole_value_with(
        &failsafe_registration,
        &DialectId::try_new(jqf_codec_yaml::YAML_FAILSAFE_DIALECT_ID).expect("dialect"),
        b"a: 1\n",
    )
    .map_err(|kind| format!("failsafe decode failed: {kind:?}"))?;
    let Value::Object(members) = &failsafe else {
        return Err("failsafe a: 1 must decode to an object".into());
    };
    let value = members
        .get("a")
        .ok_or_else(|| "failsafe member a missing".to_string())?;
    assert_eq!(value.kind(), ValueKind::String, "failsafe plain scalar is !!str");
    let Value::String(text) = value else {
        return Err("failsafe member a is not a string".into());
    };
    let text: &str = text.as_ref();
    assert_eq!(text, "1", "failsafe a: 1 decodes to the string \"1\"");
    // Local tags stay Value::Tagged with the exact text.
    let money = decoded(b"!money \"10\"\n")?;
    assert_eq!(money.tag().map(jqf_data::TagId::as_str), Some("!money"), "!money tag");
    // Percent triplets are identity characters.
    let percent = decoded(b"!foo%62ar \"x\"\n")?;
    assert_eq!(
        percent.tag().map(jqf_data::TagId::as_str),
        Some("!foo%62ar"),
        "percent triplet preserved"
    );
    // Tags on collections are not discarded.
    let tagged_map = decoded(b"!order {a: 1}\n")?;
    assert_eq!(
        tagged_map.tag().map(jqf_data::TagId::as_str),
        Some("!order"),
        "collection tag retained"
    );
    let tagged_seq = decoded(b"!list\n- 1\n- 2\n")?;
    assert_eq!(
        tagged_seq.tag().map(jqf_data::TagId::as_str),
        Some("!list"),
        "sequence tag retained"
    );
    let json_registration =
        jqf_codec_yaml::registration().map_err(|error| format!("json-schema registration: {error:?}"))?;
    let json_dialect = DialectId::try_new(jqf_codec_yaml::YAML_JSON_DIALECT_ID).expect("dialect");
    if whole_value_with(&json_registration, &json_dialect, b"hello\n").is_ok() {
        return Err("yaml.json@1 must reject an unmatched plain".into());
    }
    let json_null = whole_value_with(&json_registration, &json_dialect, b"null\n")
        .map_err(|kind| format!("yaml.json@1 null: {kind:?}"))?;
    if json_null.kind() != ValueKind::Null {
        return Err(format!("yaml.json@1 null must be Null, got {json_null:?}"));
    }
    Ok(())
}

/// The number law: exact integers, finite float spellings as EXACT decimals
/// (D1 numbers slice), fixed NaN bits.
fn assert_number_law() -> Result<(), String> {
    let big = decoded(b"123456789012345678901234567890\n")?;
    let Value::Number(number) = big else {
        return Err("expected a number".into());
    };
    let integer = number
        .to_integer()
        .ok_or_else(|| "expected an exact integer".to_string())?;
    assert_eq!(
        integer.as_str(),
        "123456789012345678901234567890",
        "arbitrary-precision integer"
    );
    // A finite float spelling decodes as an EXACT decimal, not a binary64:
    // `1.5` and JSON's `1.5` are the same value (D1 decode-unify).
    let finite = decoded(b"1.5\n")?;
    let Value::Number(finite) = finite else {
        return Err("expected a number".into());
    };
    let decimal = finite
        .as_decimal()
        .ok_or_else(|| "1.5 must decode as an exact decimal".to_string())?;
    assert_eq!((decimal.coefficient().as_str(), decimal.scale()), ("15", 1));
    // An out-of-binary64 spelling is still a finite exact decimal, never a
    // range error: `1e400` has a canonical spelling (`1E+400`).
    let huge = decoded(b"1e400\n")?;
    let Value::Number(huge) = huge else {
        return Err("expected a number".into());
    };
    let huge = huge
        .as_decimal()
        .ok_or_else(|| "1e400 must decode as an exact decimal".to_string())?;
    assert_eq!((huge.coefficient().as_str(), huge.scale()), ("1", -400));
    // The binary64 kind survives ONLY for `.inf`/`-.inf`/`.nan`.
    let inf = decoded(b"-.inf\n")?;
    let Value::Number(inf) = inf else {
        return Err("expected a number".into());
    };
    assert_eq!(
        inf.as_float().map(jqf_data::Float::get),
        Some(f64::NEG_INFINITY),
        "signed infinity stays binary64"
    );
    // .nan maps to the fixed positive quiet NaN bits.
    let nan = decoded(b".nan\n")?;
    let Value::Number(nan) = nan else {
        return Err("expected a number".into());
    };
    assert_eq!(
        nan.as_float().map(jqf_data::Float::bits),
        Some(0x7ff8_0000_0000_0000),
        "fixed NaN bits"
    );
    // YAML 1.2.2 core: `[-+]?[0-9]+` is an integer, leading zeros included.
    // Underscores, binary `0b`, and uppercase radix prefixes are strings.
    let leading_zero = decoded(b"007\n")?;
    let Value::Number(leading_zero) = leading_zero else {
        return Err("007 must be a number".into());
    };
    assert_eq!(
        leading_zero.to_integer().map(|i| i.as_str().to_owned()),
        Some("7".into()),
        "007 decodes as the integer 7"
    );
    assert_kind(
        &decoded(b"1_2_3\n")?,
        ValueKind::String,
        "1_2_3 stays a string (core has no underscore separators)",
    )?;
    assert_kind(
        &decoded(b"1_0.5\n")?,
        ValueKind::String,
        "1_0.5 stays a string (core float has no underscores)",
    )?;
    assert_kind(
        &decoded(b"1e1_0\n")?,
        ValueKind::String,
        "1e1_0 stays a string (the exponent production takes digits only)",
    )?;
    assert_kind(
        &decoded(b"0b101\n")?,
        ValueKind::String,
        "0b101 stays a string (core has no binary production)",
    )?;
    assert_kind(
        &decoded(b"0X1F\n")?,
        ValueKind::String,
        "0X1F stays a string (core radix prefix is lowercase)",
    )?;
    let hex = decoded(b"0x1F\n")?;
    let Value::Number(hex) = hex else {
        return Err("0x1F must be a number".into());
    };
    if hex.to_integer().map(|i| i.as_str().to_owned()) != Some("31".into()) {
        return Err(format!("0x1F must decode as integer 31, got {hex:?}"));
    }
    let oct = decoded(b"0o17\n")?;
    let Value::Number(oct) = oct else {
        return Err("0o17 must be a number".into());
    };
    if oct.to_integer().map(|i| i.as_str().to_owned()) != Some("15".into()) {
        return Err(format!("0o17 must decode as integer 15, got {oct:?}"));
    }
    Ok(())
}

/// yaml.key-equivalence@1: duplicate keys rejected through the law.
fn assert_key_equivalence() {
    // Exact text duplicates are rejected.
    assert!(whole_value(b"a: 1\na: 2\n").is_err(), "exact duplicate rejected");
    // `1` and `01` are the same integer key under core — both fail the
    // same way (a non-string key is not coerced), and the duplicate
    // rejection is observable through quoted string keys with equal text.
    assert!(
        whole_value(b"\"a\": 1\n\"a\": 2\n").is_err(),
        "quoted duplicate rejected"
    );
    // int 1 and float 1.0 are DIFFERENT keys (tags differ): a mapping with
    // both is representable only as map-entry topology, which the semantic
    // document does not coerce — the same unrepresentable failure a
    // non-string key always has.
    assert!(
        whole_value(b"1: a\n1.0: b\n").is_err(),
        "non-string keys are never coerced"
    );
}

/// decode -> encode -> decode round-trip identity over representative shapes.
fn assert_round_trip() -> Result<(), String> {
    let cases: [&[u8]; 4] = [
        b"a: 1\nb: [true, null, \"x\"]\n",
        b"server:\n  host: localhost\n  ports:\n    - 80\n    - 443\n",
        b"name: Ada\nage: 37\n",
        b"- 1\n- 2\n- 3\n",
    ];
    for bytes in cases {
        let first = decoded(bytes)?;
        let encoded = encode_yaml(&first).map_err(|kind| format!("encode failed: {kind:?}"))?;
        let second = decoded(&encoded)?;
        if first.kind() != second.kind() || format!("{first:?}") != format!("{second:?}") {
            return Err(format!(
                "round-trip drift: kind {:?} vs {:?}, value {first:?} vs {second:?} for {bytes:?}",
                first.kind(),
                second.kind(),
            ));
        }
    }
    Ok(())
}

/// The canonical renderer's byte law: markers, one tag per node, escapes.
fn assert_encode_byte_law() -> Result<(), String> {
    let value = decoded(b"b: [1, 2]\n")?;
    let encoded = encode_yaml(&value).map_err(|kind| format!("encode failed: {kind:?}"))?;
    let text = String::from_utf8_lossy(&encoded);
    assert!(
        text.starts_with("---\n!!map {"),
        "stream marker and explicit root tag, got: {text:?}"
    );
    assert!(text.ends_with("}\n...\n"), "close and end marker, got: {text:?}");
    assert!(text.contains("!!int \"1\""), "explicit int tag, got: {text:?}");
    assert!(text.contains("!!seq ["), "explicit seq tag, got: {text:?}");
    // No double commas.
    assert!(!text.contains(",,"), "no double commas, got: {text:?}");
    // A string with a quote escapes exactly.
    let quoted = decoded(b"q: \"a\\\"b\"\n")?;
    let encoded = encode_yaml(&quoted).map_err(|kind| format!("encode failed: {kind:?}"))?;
    let text = String::from_utf8_lossy(&encoded);
    assert!(text.contains("\\\""), "quote escaped, got: {text:?}");
    Ok(())
}

/// Decode or a descriptive error string (the receipt helpers' ?-target).
fn decoded(bytes: &[u8]) -> Result<Value, String> {
    whole_value(bytes).map_err(|kind| format!("decode failed: {kind:?}"))
}

/// The block dialect's quoting rule round-trips: a string that looks like a
/// number under the core schema (`007`, `0x1F`, `0o17`) is QUOTED on the
/// way out, so the decoder reads it back as the same string. Spellings the
/// core schema leaves as strings (`0b101`, `1_2_3`) stay plain. The
/// encoder's decision delegates to the decoder's own int/float productions.
fn assert_block_round_trip() -> Result<(), String> {
    for spelling in [
        "0b101", "0B101", "-0b101", "1_2_3", "0_", "007", "1_0.5", "0.5_0", "0x1F", "0o17", "hello", "<<",
    ] {
        let value = Value::try_string(spelling).map_err(|error| format!("string {spelling:?}: {error:?}"))?;
        let encoded = encode_yaml_block(&value).map_err(|kind| format!("block encode of {spelling:?}: {kind:?}"))?;
        let text = String::from_utf8_lossy(&encoded);
        let must_quote = matches!(spelling, "007" | "0x1F" | "0o17" | "<<");
        if must_quote {
            assert!(
                text.contains('"'),
                "{spelling:?} must be quoted to survive the round trip, got: {text:?}"
            );
        } else {
            assert!(!text.contains('"'), "{spelling:?} stays plain, got: {text:?}");
        }
        let round = decoded(&encoded)?;
        let Value::String(shared) = &round else {
            return Err(format!(
                "block round-trip of {spelling:?} returned {round:?} (encoded {text:?})"
            ));
        };
        let text: &str = shared.as_ref();
        if text != spelling {
            return Err(format!(
                "block round-trip of {spelling:?} returned {text:?} (encoded {text:?})"
            ));
        }
    }
    Ok(())
}

/// The target tag validator admits exactly the grammar-valid tag texts. A
/// percent triplet is an IDENTITY character (the codec's own law — the
/// decoder keeps `!foo%62ar` verbatim and the encoder re-emits it), so it
/// must validate; `#` is a valid URI character; a malformed `%` escape and a
/// `,` inside a local tag are refused.
fn assert_tag_validator() -> Result<(), String> {
    let registration = jqf_codec_yaml::registration().map_err(|_| "core registration")?;
    let mut resources = resources();
    let format = FormatId::try_new(jqf_codec_yaml::FORMAT_ID).map_err(|_| "format")?;
    let dialect = DialectId::try_new(jqf_codec_yaml::YAML_STREAM_CANONICAL_DIALECT_ID).map_err(|_| "dialect")?;
    let request = EncodeRequest {
        format: &format,
        dialect: &dialect,
        diagnostics: DiagnosticPolicy::ErrorsOnly,
        preservation: jqf_codec_core::PreservationRequest::None,
        options: None,
    };
    let validator = registration
        .tag_validator()
        .expect("tag validator")
        .create_validator(request, &mut resources)
        .map_err(|error| format!("validator: {error:?}"))?;
    for text in ["!money", "!foo%62ar", "!foo#bar", "tag:yaml.org,2002:str%62"] {
        let tag = TagId::try_new_unaccounted(text).map_err(|_| "tag id")?;
        validator
            .validate(&[&tag], &resources)
            .map_err(|error| format!("{text} must validate, got {error:?}"))?;
    }
    for text in ["!foo%zz", "!foo%6", "!foo,bar"] {
        let tag = TagId::try_new_unaccounted(text).map_err(|_| "tag id")?;
        if validator.validate(&[&tag], &resources).is_ok() {
            return Err(format!("{text} must be refused"));
        }
    }
    Ok(())
}

fn assert_kind(value: &Value, kind: ValueKind, label: &str) -> Result<(), String> {
    if value.kind() == kind {
        Ok(())
    } else {
        Err(format!("{label}: expected {kind:?}, got {:?}", value.kind()))
    }
}
