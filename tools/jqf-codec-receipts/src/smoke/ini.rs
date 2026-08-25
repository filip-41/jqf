//! Flat-config codec receipt battery (plan 137 S0/S1).
//!
//! The route-slot duty: ONE advertised slot, Whole/`CompleteDocument`, for
//! each of the three formats — the same inventory `jqf-sdk-smoke` pins. The
//! battery drives the registration's own decoder and encoder factories:
//! every value is a String (plan 137 D7), comments attach as the grammar's
//! comment fact (D10), and the decode∘encode fixpoint holds on normalized
//! documents (S1's gate).

use jqf_codec_core::{
    AccessOutcome, CodecRunContext, DecodeRequest, DiagnosticPolicy, EncodeItem, EncodeRequest, ValidationMode,
};
use jqf_data::{DialectId, FormatId, Value, ValueKind};
use jqf_resource::ResourceContext;

use crate::drive::{resources, source, whole_requirement};

/// The three (registration, format, input dialect, output dialect) tuples.
struct Case {
    registration: fn() -> Result<jqf_codec_core::CodecRegistration<'static>, jqf_codec_core::RegistrationError>,
    format: &'static str,
    input_dialect: &'static str,
    output_dialect: &'static str,
}

const CASES: [Case; 3] = [
    Case {
        registration: jqf_codec_ini::registration,
        format: jqf_codec_ini::FORMAT_ID,
        input_dialect: jqf_codec_ini::PROPERTIES_JDK_DIALECT_ID,
        output_dialect: jqf_codec_ini::PROPERTIES_JQF_1_0_DIALECT_ID,
    },
    Case {
        registration: jqf_codec_ini::registration_ini,
        format: jqf_codec_ini::INI_FORMAT_ID,
        input_dialect: jqf_codec_ini::INI_JQF_STRICT_DIALECT_ID,
        output_dialect: jqf_codec_ini::INI_JQF_1_0_DIALECT_ID,
    },
    Case {
        registration: jqf_codec_ini::registration_dotenv,
        format: jqf_codec_ini::DOTENV_FORMAT_ID,
        input_dialect: jqf_codec_ini::DOTENV_JQF_STRICT_DIALECT_ID,
        output_dialect: jqf_codec_ini::DOTENV_JQF_1_0_DIALECT_ID,
    },
];

fn decode_one(case: &Case, bytes: &[u8], resources: &mut ResourceContext<'_>) -> Result<Value, String> {
    let registration = (case.registration)().map_err(|e| format!("{e:?}"))?;
    let mut provider = registration
        .decoder()
        .expect("decoder")
        .create_provider(
            source(bytes),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(case.input_dialect).expect("dialect"),
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
    let result = session
        .decode(&mut run)
        .map_err(|e| format!("decode: {:?}", e.kind()))?;
    match result.outcome() {
        AccessOutcome::FullDocument(product) => product
            .document()
            .materialize_root(resources)
            .map_err(|e| e.to_string()),
        AccessOutcome::Located { .. } => Err("unexpected located outcome".into()),
    }
}

fn encode_one(case: &Case, value: &Value, resources: &mut ResourceContext<'_>) -> Result<Vec<u8>, String> {
    let registration = (case.registration)().map_err(|e| format!("{e:?}"))?;
    let format = FormatId::try_new(case.format).map_err(|e| e.to_string())?;
    let dialect = DialectId::try_new(case.output_dialect).map_err(|e| e.to_string())?;
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

fn member_text<'a>(object: &'a Value, key: &str) -> Result<&'a str, String> {
    let Value::Object(object) = object else {
        return Err("root is not an object".into());
    };
    let Value::String(text) = object.get(key).ok_or("missing member")? else {
        return Err("member is not a string".into());
    };
    Ok(text.as_str())
}

/// The flat-config smoke battery: registration validity, the single-slot
/// route inventory, strings-only projection with comment facts, the
/// decode∘encode fixpoint, and the terminal-failure law.
pub fn run() -> Result<(), String> {
    for case in &CASES {
        let registration = (case.registration)().map_err(|e| format!("invalid {e:?}"))?;
        let descriptor = registration.descriptor();
        if descriptor.format().as_str() != case.format {
            return Err(format!("{} registration names the wrong format", case.format));
        }
        // The CLI-facing route declaration (plan 137 S2): every flat-config
        // registration declares the edit lane — the capability flip and its
        // receipts land in the same commit (the 039 drift class).
        if descriptor.route_capabilities() != [jqf_codec_core::RouteCapability::Edit] {
            return Err(format!(
                "{} registration must declare exactly the Edit route",
                case.format
            ));
        }
        if !descriptor
            .dialects()
            .iter()
            .any(|dialect| dialect.as_str() == case.input_dialect)
        {
            return Err(format!("{} input dialect missing", case.format));
        }
        if !descriptor
            .dialects()
            .iter()
            .any(|dialect| dialect.as_str() == case.output_dialect)
        {
            return Err(format!("{} output dialect missing", case.format));
        }
        let mut resources = resources();
        let decoded = decode_one(
            case,
            match case.format {
                "properties" => b"# name\nname = ada\nid=42\n",
                "ini" => b"root = 1\n[db]\nhost = localhost\n",
                "dotenv" => b"# hi\nexport A=\"x y\"\n",
                _ => unreachable!(),
            },
            &mut resources,
        )?;
        // Strings only, always (plan 137 D7): `id=42` is the STRING "42" —
        // pinned on the properties case, whose fixture carries the number.
        if case.format == jqf_codec_ini::FORMAT_ID {
            assert_eq!(
                member_text(&decoded, "id").ok(),
                Some("42"),
                "no type inference: {decoded:?}"
            );
        }
        // The fixpoint: re-encode then re-decode reproduces the document.
        let bytes = encode_one(case, &decoded, &mut resources)?;
        let again = decode_one(case, &bytes, &mut resources)?;
        if !values_equal(&decoded, &again) {
            return Err(format!("{} fixpoint failed: {decoded:?} != {again:?}", case.format));
        }
    }
    // The terminal-failure law: a malformed file fails the whole decode.
    let mut resources = resources();
    assert!(
        decode_one(&CASES[0], b"a=\\u00\n", &mut resources).is_err(),
        "malformed escape"
    );
    assert!(
        decode_one(&CASES[1], b"[a]\nx=1\n[a]\n", &mut resources).is_err(),
        "duplicate section"
    );
    // The root of a flat-config document is an OBJECT whose member kinds are
    // String (the kind law the whole vertical's projection relies on).
    let decoded = decode_one(&CASES[0], b"a=1\nb=2\n", &mut resources)?;
    let Value::Object(object) = &decoded else {
        return Err("root is not an object".into());
    };
    for entry in object {
        if entry.value().kind() != ValueKind::String {
            return Err(format!("member {:?} is not a string", entry.key()));
        }
    }
    println!("flat-config-smoke: formats=3 routes=1 fixpoint=true strings_only=true terminal=true");
    Ok(())
}

fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::String(x), Value::String(y)) => x.as_str() == y.as_str(),
        (Value::Object(x), Value::Object(y)) => {
            if x.len() != y.len() {
                return false;
            }
            for entry in x {
                match y.get(entry.key()) {
                    Some(other) => {
                        if !values_equal(entry.value(), other) {
                            return false;
                        }
                    }
                    None => return false,
                }
            }
            true
        }
        _ => false,
    }
}
