//! TOML codec receipt battery (the first non-JSON codec vertical).

use crate::drive::{decode_session, exact_requirement, limited_resources, resources, source, whole_requirement};
use jqf_codec_core::{
    AccessOutcome, AccessResultKind, CodecError, CodecFailureKind, CodecRunContext, DecodeRequest, DiagnosticPolicy,
    EncodeItem, EncodeRequest, ExactSelectionRecord, ValidationMode,
};
use jqf_data::{DialectId, FormatId, Value, ValueKind};
use jqf_resource::ResourceContext;

/// A realistic Cargo.toml-shaped fixture: mixed statement kinds, each with a
/// trailing comment, exercising F1's fix across headers, arrays-of-tables,
/// inline tables, and every scalar kind in one document (not just isolated
/// one-line cases).
const CARGO_TOML_SHAPED_FIXTURE: &str = "\
[package] # crate metadata
name = \"jqf\" # crate name
version = \"0.0.0\" # semver
edition = \"2024\" # rust edition
publish = false # never publish

[dependencies] # runtime deps
jqf-data = { path = \"../jqf-data\" } # workspace path dep
serde = \"1\" # crates.io dep

[[bin]] # one binary target
name = \"jqf\" # binary name
path = \"src/main.rs\" # entry point

[features] # cargo features
default = [\"std\"] # default feature set
";

/// Decodes via the WHOLE-DOCUMENT route under a caller-owned context and
/// returns the kind of the outcome (or the failure kind).
fn whole_outcome(
    bytes: &[u8],
    resources: &mut ResourceContext<'_>,
) -> Result<jqf_codec_core::CodecFailureKind, String> {
    let registration = jqf_codec_toml::registration_1_0().map_err(|error| format!("{error:?}"))?;
    let mut provider = registration
        .decoder()
        .expect("decoder")
        .create_provider(
            source(bytes),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(jqf_codec_toml::TOML_1_0_DIALECT_ID).expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            resources,
        )
        .map_err(|error| format!("provider: {:?}", error.kind()))?;
    let requirement = whole_requirement(resources);
    let handle = provider
        .bind(&requirement)
        .map_err(|error| format!("bind: {error:?}"))?;
    let mut session = provider
        .open(&handle, resources)
        .map_err(|error| format!("open: {:?}", error.kind()))?;
    {
        let mut run = jqf_codec_core::CodecRunContext::new(resources);
        run.set_cooperative_credits(4_096);
        match session.decode(&mut run) {
            Ok(_) => Ok(jqf_codec_core::CodecFailureKind::InternalContractViolation {
                contract: "expected a nesting rejection, got a result",
            }),
            // The nesting rejection IS the decode error.
            Err(error) => Ok(error.kind()),
        }
    }
}

/// The nesting-depth law receipt: the value grammar and the table path
/// grammar reject at the request's nesting ceiling with the resource error —
/// never a stack overflow (a 1M-deep array used to abort) and never a hang
/// (a deep table chain used to never finish).
fn assert_resource_depth() -> Result<(), String> {
    let is_nesting_limit = |kind: &jqf_codec_core::CodecFailureKind| {
        matches!(
            kind,
            jqf_codec_core::CodecFailureKind::Resource(jqf_resource::ResourceError::LimitExceeded {
                limit_kind: jqf_resource::ResourceLimit::NestingDepth,
                ..
            })
        )
    };
    let mut resources = limited_resources(1);
    // A 2-deep array and a 2-part header both exceed a ceiling of 1.
    let kind = whole_outcome(b"a = [[1]]\n", &mut resources)?;
    if !is_nesting_limit(&kind) {
        return Err(format!("deep array was not rejected as nesting: {kind:?}"));
    }
    let mut resources = limited_resources(1);
    let kind = whole_outcome(b"[a.b]\nx = 1\n", &mut resources)?;
    if !is_nesting_limit(&kind) {
        return Err(format!("deep header was not rejected as nesting: {kind:?}"));
    }
    // A 1-deep document still decodes under the same ceiling.
    let mut resources = limited_resources(1);
    let kind = whole_outcome(b"a = 1\n", &mut resources)?;
    if !matches!(
        kind,
        jqf_codec_core::CodecFailureKind::InternalContractViolation {
            contract: "expected a nesting rejection, got a result",
        }
    ) {
        return Err("a shallow document failed under a ceiling of 1".into());
    }
    // The scoped route shares the law.
    let mut resources = limited_resources(1);
    let located = run_located_in(b"a = [[1]]\n", &["a"], None, None, &mut resources);
    match located {
        Err(error) if error.contains("NestingDepth") => {}
        other => {
            return Err(format!("scoped nesting must be NestingDepth, got {other:?}"));
        }
    }
    Ok(())
}

/// Drives one scoped or shallow decode, returning the located outcome, the
/// physical route receipt, and the bound slot.
/// The raw-error twin of [`run_located`]: the poll error itself, so the drift
/// fence can compare codes and offsets with the whole parser's.
fn run_located_raw<'bytes>(
    bytes: &'bytes [u8],
    members: &[&str],
    index: Option<i64>,
    range: Option<(Option<i64>, Option<i64>)>,
) -> Result<jqf_codec_core::LocatedOutcome<'bytes>, jqf_codec_core::CodecError> {
    let mut resources = resources();
    let registration = jqf_codec_toml::registration_1_0().expect("registration");
    let mut provider = registration
        .decoder()
        .expect("decoder")
        .create_provider(
            source(bytes),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(jqf_codec_toml::TOML_1_0_DIALECT_ID).expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            &mut resources,
        )
        .expect("provider");
    let requirement = exact_requirement(&resources, members, index, range);
    let handle = provider.bind(&requirement).expect("bind");
    let mut session = provider.open(&handle, &mut resources).expect("open");
    let result = decode_session(&mut session, &mut resources)?;
    let AccessOutcome::Located(outcome) = result.outcome() else {
        return Err(jqf_codec_core::CodecError::new(
            jqf_codec_core::CodecFailureKind::InternalContractViolation {
                contract: "located decode was not a Located outcome",
            },
        ));
    };
    let owned = outcome
        .product()
        .try_clone()
        .map_err(|_error| jqf_codec_core::CodecError::new(jqf_codec_core::CodecFailureKind::Overflow))?;
    jqf_codec_core::LocatedOutcome::try_new(&owned, outcome.result().clone())
}

fn run_located<'bytes>(
    bytes: &'bytes [u8],
    members: &[&str],
    index: Option<i64>,
    range: Option<(Option<i64>, Option<i64>)>,
) -> Result<
    (
        jqf_codec_core::LocatedOutcome<'bytes>,
        jqf_codec_core::PhysicalRouteId,
        u32,
    ),
    String,
> {
    let mut resources = resources();
    run_located_in(bytes, members, index, range, &mut resources)
}

/// `run_located` over a caller-owned context (the `resource_depth` receipt
/// runs the scoped route under a reduced nesting ceiling).
fn run_located_in<'bytes>(
    bytes: &'bytes [u8],
    members: &[&str],
    index: Option<i64>,
    range: Option<(Option<i64>, Option<i64>)>,
    resources: &mut ResourceContext<'static>,
) -> Result<
    (
        jqf_codec_core::LocatedOutcome<'bytes>,
        jqf_codec_core::PhysicalRouteId,
        u32,
    ),
    String,
> {
    let registration = jqf_codec_toml::registration_1_0().map_err(|error| format!("{error:?}"))?;
    let mut provider = registration
        .decoder()
        .expect("decoder")
        .create_provider(
            source(bytes),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(jqf_codec_toml::TOML_1_0_DIALECT_ID).expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            resources,
        )
        .map_err(|error| format!("provider: {:?}", error.kind()))?;
    let requirement = exact_requirement(resources, members, index, range);
    let handle = provider
        .bind(&requirement)
        .map_err(|error| format!("bind: {error:?}"))?;
    let mut session = provider
        .open(&handle, resources)
        .map_err(|error| format!("open: {:?}", error.kind()))?;
    let receipt = session
        .physical_route_receipt()
        .ok_or_else(|| "located session without a physical receipt".to_owned())?;
    let result = decode_session(&mut session, resources).map_err(|error| format!("decode: {:?}", error.kind()))?;
    let AccessOutcome::Located(outcome) = result.outcome() else {
        return Err("located decode was not a Located outcome".into());
    };
    let owned = outcome
        .product()
        .try_clone()
        .map_err(|error| format!("clone located product: {error:?}"))?;
    let outcome = jqf_codec_core::LocatedOutcome::try_new(&owned, outcome.result().clone())
        .map_err(|error| format!("clone located outcome: {error:?}"))?;
    Ok((outcome, receipt.route(), receipt.slot().get()))
}

/// The whole-document route's answer for one member path (plus an optional
/// trailing index) over a materialized document: the value, or the negative
/// observation the tree navigator reports — the missing step, or the kind
/// mismatch at a step. The dotted-parity receipt's whole-document arm.
#[derive(Debug)]
enum Navigated<'a> {
    Value(&'a Value),
    Missing { step: usize },
    Mismatch { step: usize, kind: ValueKind },
}

fn navigate_whole<'a>(value: &'a Value, members: &[&str], index: Option<i64>) -> Navigated<'a> {
    let mut current = value;
    for (step, member) in members.iter().enumerate() {
        match current {
            Value::Object(object) => match object.get(member) {
                Some(next) => current = next,
                None => return Navigated::Missing { step },
            },
            other => {
                return Navigated::Mismatch {
                    step,
                    kind: other.kind(),
                };
            }
        }
    }
    if let Some(index) = index {
        return match current {
            Value::Array(array) => {
                let len = array.len();
                let position = if index < 0 {
                    usize::try_from(index.unsigned_abs())
                        .ok()
                        .and_then(|from_end| len.checked_sub(from_end))
                } else {
                    usize::try_from(index).ok().filter(|position| *position < len)
                };
                match position.and_then(|position| array.get(position)) {
                    Some(item) => Navigated::Value(item),
                    None => Navigated::Missing { step: members.len() },
                }
            }
            other => Navigated::Mismatch {
                step: members.len(),
                kind: other.kind(),
            },
        };
    }
    Navigated::Value(current)
}

fn decode(bytes: &[u8]) -> Result<Value, CodecError> {
    let mut resources = resources();
    let registration = jqf_codec_toml::registration_1_0().expect("registration");
    let mut provider = registration.decoder().expect("decoder").create_provider(
        source(bytes),
        DecodeRequest {
            validation: ValidationMode::Strict,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            dialect: &DialectId::try_new(jqf_codec_toml::TOML_1_0_DIALECT_ID).expect("dialect"),
            options: None,
            allow_adjacent_values: false,
            value_separator: &[],
        },
        &mut resources,
    )?;
    let requirement = whole_requirement(&resources);
    let handle = provider.bind(&requirement).expect("bind");
    let mut session = provider.open(&handle, &mut resources)?;
    let result = decode_session(&mut session, &mut resources)?;
    let AccessOutcome::FullDocument(product) = result.outcome() else {
        panic!("expected full document");
    };
    product.document().materialize_root(&mut resources).map_err(|_| {
        CodecError::new(CodecFailureKind::InternalContractViolation {
            contract: "materialize TOML root",
        })
    })
}

fn encode(value: &Value, resources: &mut ResourceContext<'_>) -> Result<Vec<u8>, CodecError> {
    let format = FormatId::try_new(jqf_codec_toml::FORMAT_ID).expect("format");
    let dialect = DialectId::try_new(jqf_codec_toml::TOML_JQF_1_0_DIALECT_ID).expect("dialect");
    let registration = jqf_codec_toml::registration_1_0().expect("registration");
    let factory = registration.encoder().expect("encoder").create_factory(
        EncodeRequest {
            format: &format,
            dialect: &dialect,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            preservation: jqf_codec_core::PreservationRequest::None,
            options: None,
        },
        resources,
    )?;
    let mut session = factory
        .start(
            EncodeItem::Owned(value),
            jqf_codec_core::PreservationRequest::None,
            resources,
        )
        .expect("session");
    let mut out = Vec::new();
    {
        let mut sink = jqf_codec_core::VecByteSink::new(&mut out);
        let mut run = CodecRunContext::new(resources);
        run.set_cooperative_credits(4_096);
        session.encode(&mut sink, &mut run)?;
    }
    Ok(out)
}

fn roundtrip(input: &str) -> Result<String, CodecError> {
    let value = decode(input.as_bytes())?;
    let mut resources = resources();
    let bytes = encode(&value, &mut resources)?;
    Ok(String::from_utf8(bytes).expect("UTF-8 output"))
}

#[allow(
    clippy::too_many_lines,
    reason = "one deterministic receipt battery; splitting it would scatter the pinned inventory"
)]
pub fn run() -> Result<(), String> {
    // Registration validity.
    let registration =
        jqf_codec_toml::registration_1_0().map_err(|error| format!("invalid TOML 1.0 registration: {error:?}"))?;
    let descriptor = registration.descriptor();
    if descriptor.format().as_str() != jqf_codec_toml::FORMAT_ID {
        return Err("TOML 1.0 registration names the wrong format".into());
    }
    let dialects: Vec<&str> = descriptor.dialects().iter().map(|d| d.as_str()).collect();
    if dialects
        != [
            jqf_codec_toml::TOML_1_0_DIALECT_ID,
            jqf_codec_toml::TOML_JQF_1_0_DIALECT_ID,
        ]
    {
        return Err(format!("TOML 1.0 dialects drifted: {dialects:?}"));
    }
    let ops = descriptor.operations();
    if !ops.decode() || !ops.encode() || !ops.validate_tags() {
        return Err("TOML must declare decode+encode+validate-tags".into());
    }
    if registration.tag_validator().is_none() {
        return Err("TOML tag validator missing".into());
    }
    let registration_1_1 =
        jqf_codec_toml::registration_1_1().map_err(|error| format!("invalid TOML 1.1 registration: {error:?}"))?;
    let dialects_1_1: Vec<&str> = registration_1_1
        .descriptor()
        .dialects()
        .iter()
        .map(|d| d.as_str())
        .collect();
    if dialects_1_1
        != [
            jqf_codec_toml::TOML_1_1_DIALECT_ID,
            jqf_codec_toml::TOML_JQF_1_1_DIALECT_ID,
        ]
    {
        return Err(format!("TOML 1.1 dialects drifted: {dialects_1_1:?}"));
    }

    // Provider route inventory: TWO access routes — slot 0
    // Whole/CompleteDocument, slot 1
    // Exact/Located — with an
    // empty support profile (no authoritative attribute absence — TOML has no
    // attributes).
    let mut route_resources = resources();
    let provider = registration
        .decoder()
        .expect("decoder")
        .create_provider(
            source(b"a = 1\n"),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(jqf_codec_toml::TOML_1_0_DIALECT_ID).expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            &mut route_resources,
        )
        .map_err(|error| format!("provider: {:?}", error.kind()))?;
    let routes = provider.route_descriptions();
    if routes.len() != 2 || routes[0].slot().get() != 0 || routes[1].slot().get() != 1 {
        return Err(format!(
            "TOML did not advertise exactly two access routes at slots 0..1 (got {}: {:?})",
            routes.len(),
            routes.iter().map(|route| route.slot().get()).collect::<Vec<_>>()
        ));
    }
    if routes[0].bundle().footprint() != jqf_codec_core::AccessFootprintKind::Whole
        || routes[0].bundle().result() != AccessResultKind::CompleteDocument
    {
        return Err("TOML slot 0 is not Whole/CompleteDocument".into());
    }
    if routes[1].bundle().footprint() != jqf_codec_core::AccessFootprintKind::Exact
        || routes[1].bundle().result() != AccessResultKind::Located
    {
        return Err("TOML slot 1 is not Exact/Located".into());
    }
    if provider.supports_attribute_absence() {
        return Err("TOML must not advertise attribute support".into());
    }

    // Accept/reject corpus (deterministic table).
    let mut accepted = 0u32;
    let mut rejected = 0u32;
    for (bytes, should_accept) in [
        (b"".as_slice(), true),
        (b"a = 1\n".as_slice(), true),
        (b"a = \"x\"\n".as_slice(), true),
        (b"a = 0x1F\n".as_slice(), true),
        (b"a = 1_000_000\n".as_slice(), true),
        (b"a = [1, 2, 3]\n".as_slice(), true),
        (b"a = { x = 1, y = 2 }\n".as_slice(), true),
        (b"[table]\nx = 1\n".as_slice(), true),
        (b"[[a]]\nx = 1\n[[a]]\ny = 2\n".as_slice(), true),
        (b"a.b = 1\n".as_slice(), true),
        (b"a = 1979-05-27\n".as_slice(), true),
        (b"a = 07:32:00\n".as_slice(), true),
        (b"a = 1979-05-27T07:32:00Z\n".as_slice(), true),
        (b"a = inf\n".as_slice(), true),
        (b"a = -nan\n".as_slice(), true),
        (b"# comment\n".as_slice(), true),
        // F1: a trailing comment after a value is legal TOML for every value
        // type (`require_statement_end` must skip it, not just a bare newline).
        (b"a = 1 # c\n".as_slice(), true),
        (b"a = 1.5 # c\n".as_slice(), true),
        (b"a = \"x\" # c\n".as_slice(), true),
        (b"a = true # c\n".as_slice(), true),
        (b"a = [1, 2, 3] # c\n".as_slice(), true),
        (b"a = { x = 1 } # c\n".as_slice(), true),
        (b"a = 1979-05-27 # c\n".as_slice(), true),
        (b"[table] # c\nx = 1\n".as_slice(), true),
        (CARGO_TOML_SHAPED_FIXTURE.as_bytes(), true),
        // F2 bullet 1: underscores next to a hex letter, not just a digit.
        (b"a = 0xff_ff\n".as_slice(), true),
        (b"a = 0xDEAD_BEEF\n".as_slice(), true),
        // F2 bullet 2: dotted keys inside an inline table (the spec's own
        // example).
        (b"animal = { type.name = \"pug\" }\n".as_slice(), true),
        (b"a = 1\na = 2\n".as_slice(), false),
        (b"[a]\nx = 1\n[a]\ny = 2\n".as_slice(), false),
        (b"a = 1 garbage\n".as_slice(), false),
        (b"a = \n".as_slice(), false),
        (b"a = \"unterminated\n".as_slice(), false),
        (b"[a]\nx = 1\n[[a]]\ny = 2\n".as_slice(), false),
    ] {
        let result = decode(bytes);
        if should_accept {
            result.map_err(|error| {
                format!(
                    "TOML accepted corpus rejected {:?}: {:?}",
                    String::from_utf8_lossy(bytes),
                    error.kind()
                )
            })?;
            accepted += 1;
        } else {
            if result.is_ok() {
                return Err(format!(
                    "TOML rejected corpus accepted {:?}",
                    String::from_utf8_lossy(bytes)
                ));
            }
            rejected += 1;
        }
    }

    // Encode receipts.
    let simple = roundtrip("a = 1\nb = \"x\"\n").map_err(|error| format!("simple roundtrip: {:?}", error.kind()))?;
    if simple != "a = 1\nb = \"x\"\n" {
        return Err(format!("simple encode mismatch: {simple:?}"));
    }
    let dotted = roundtrip("a.b = 1\n").map_err(|error| format!("dotted roundtrip: {:?}", error.kind()))?;
    if dotted != "[a]\nb = 1\n" {
        return Err(format!("dotted normalization mismatch: {dotted:?}"));
    }
    let empty = roundtrip("# comment only\n").map_err(|error| format!("empty roundtrip: {:?}", error.kind()))?;
    if !empty.is_empty() {
        return Err(format!("empty root must emit zero bytes, got {empty:?}"));
    }
    // Round-trip through the decoder: the deterministic output reparses to the
    // same document.
    let reparsed =
        decode(simple.as_bytes()).map_err(|error| format!("reparse of encoded output: {:?}", error.kind()))?;
    let mut re_encode_resources = resources();
    let re_encoded = encode(&reparsed, &mut re_encode_resources)
        .map_err(|error| format!("re-encode of reparsed: {:?}", error.kind()))?;
    if String::from_utf8(re_encoded).expect("utf8") != simple {
        return Err("encode → decode → encode is not a fixed point".into());
    }

    // Unrepresentable values fail preflight.
    let mut preflight_resources = resources();
    let result = encode(&Value::Null, &mut preflight_resources);
    if result.is_ok() {
        return Err("encoding null must fail preflight".into());
    }

    // F3: an integer outside TOML 1.0.0's signed 64-bit range must decline,
    // not silently truncate or write spec-invalid bytes.
    let mut out_of_range_object =
        jqf_data::Object::try_new().map_err(|error| format!("out-of-range object: {error:?}"))?;
    let huge =
        jqf_data::Integer::parse("9223372036854775808").map_err(|error| format!("parse i64::MAX + 1: {error:?}"))?;
    let key = jqf_data::ObjectKey::try_from_str("n").map_err(|error| format!("out-of-range key: {error:?}"))?;
    out_of_range_object
        .try_insert_unique(key, Value::Number(jqf_data::Number::integer(huge)))
        .map_err(|error| format!("out-of-range insert: {error:?}"))?;
    if encode(&Value::Object(out_of_range_object), &mut preflight_resources).is_ok() {
        return Err("encoding an i64-overflowing integer must fail preflight".into());
    }

    // The source-span zero-copy route (capability roadmap phase 1): verbatim
    // strings, keys, and integers name their source bytes instead of the
    // decoded arena. Bare keys, literal strings, and zero-escape basic strings
    // are source-backed; the escaped string and the non-canonical integer
    // spellings stay on the copying/render paths.
    let mut stats_resources = resources();
    let registration_for_stats = jqf_codec_toml::registration_1_0().map_err(|error| format!("{error:?}"))?;
    let mut provider = registration_for_stats
        .decoder()
        .expect("decoder")
        .create_provider(
            source(b"title = \"TOML\"\ncount = 42\nradix = 0x2A\nescaped = \"a\\nb\"\n"),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(jqf_codec_toml::TOML_1_0_DIALECT_ID).expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            &mut stats_resources,
        )
        .map_err(|error| format!("stats provider: {:?}", error.kind()))?;
    let requirement = whole_requirement(&stats_resources);
    let handle = provider
        .bind(&requirement)
        .map_err(|error| format!("stats bind: {error:?}"))?;
    let mut session = provider
        .open(&handle, &mut stats_resources)
        .map_err(|error| format!("stats open: {:?}", error.kind()))?;
    let stats_result = decode_session(&mut session, &mut stats_resources)
        .map_err(|error| format!("stats decode: {:?}", error.kind()))?;
    let AccessOutcome::FullDocument(stats_product) = stats_result.outcome() else {
        return Err("stats decode was not a full document".into());
    };
    let stats = stats_product
        .document()
        .text_storage_stats()
        .map_err(|error| format!("stats: {error:?}"))?;
    if !stats.trusted_session_source_attachment
        || stats.source_keys != 4
        || stats.source_string_values != 1
        || stats.source_integer_values != 1
        || stats.stored_string_values != 1
        || stats.stored_integer_refs != 2
    {
        return Err(format!(
            "TOML text storage did not take the source-span route: {stats:?}"
        ));
    }

    // The SCOPED route (slot 2, Exact/Located): an exact path materializes
    // only the located subtree, with the identical selection records the
    // whole-decode-then-navigate path publishes.
    let (scoped, route, slot) = run_located(b"[t]\nx = 1\n", &["t", "x"], None, None)?;
    if slot != 1 || route != jqf_codec_toml::SCOPED_PHYSICAL_ROUTE_ID {
        return Err(format!("scoped route receipt mismatch: slot={slot} route={route:?}"));
    }
    let ExactSelectionRecord::Node { .. } = scoped.result() else {
        return Err("scoped selection was not a Node".into());
    };
    let mut scoped_materialize = resources();
    let value = scoped
        .product()
        .document()
        .materialize_root(&mut scoped_materialize)
        .map_err(|_| "materialize scoped value".to_owned())?;
    let ok = matches!(&value, Value::Number(n) if n.to_i64() == Some(1));
    if !ok {
        return Err(format!("scoped materialized the wrong value: {value:?}"));
    }
    // A missing member is a Missing record, not a silent null.
    let (missing, _, _) = run_located(b"[t]\nx = 1\n", &["t", "z"], None, None)?;
    let ExactSelectionRecord::Missing { step_index, .. } = missing.result() else {
        return Err("missing selection was not Missing".into());
    };
    if *step_index != 1 {
        return Err(format!("missing step index was {step_index}, want 1"));
    }
    // A member over a scalar is a TypeMismatch naming the scalar's kind.
    let (mismatch, _, _) = run_located(b"a = 1\n", &["a", "b"], None, None)?;
    let ExactSelectionRecord::TypeMismatch {
        step_index,
        actual_type,
        ..
    } = mismatch.result()
    else {
        return Err("mismatch selection was not TypeMismatch".into());
    };
    if *step_index != 1 || *actual_type != jqf_data::ValueKind::Number {
        return Err(format!("mismatch record wrong: step={step_index} kind={actual_type:?}"));
    }
    // Signed indices resolve against the observed length; an out-of-range
    // position is Missing.
    let (indexed, _, _) = run_located(b"a = [10, 20, 30]\n", &["a"], Some(-1), None)?;
    let ExactSelectionRecord::Node { .. } = indexed.result() else {
        return Err("indexed selection was not a Node".into());
    };
    let mut indexed_materialize = resources();
    let value = indexed
        .product()
        .document()
        .materialize_root(&mut indexed_materialize)
        .map_err(|_| "materialize indexed value".to_owned())?;
    let ok = matches!(&value, Value::Number(n) if n.to_i64() == Some(30));
    if !ok {
        return Err(format!("negative index materialized the wrong value: {value:?}"));
    }
    let (out_of_range, _, _) = run_located(b"a = [10, 20, 30]\n", &["a"], Some(5), None)?;
    let ExactSelectionRecord::Missing { .. } = out_of_range.result() else {
        return Err("out-of-range position was not Missing".into());
    };
    // A trailing range materializes a fresh in-range array (the
    // slice-materialization law).
    let (range, _, _) = run_located(b"a = [10, 20, 30]\n", &["a"], None, Some((Some(1), Some(3))))?;
    let ExactSelectionRecord::Node { .. } = range.result() else {
        return Err("range selection was not a Node".into());
    };
    let mut range_materialize = resources();
    let array = range
        .product()
        .document()
        .materialize_root(&mut range_materialize)
        .map_err(|_| "materialize range".to_owned())?;
    let Value::Array(array) = &array else {
        return Err("range did not materialize an array".into());
    };
    let twenty = |n: &jqf_data::Number| n.to_i64() == Some(20);
    let thirty = |n: &jqf_data::Number| n.to_i64() == Some(30);
    if array.len() != 2
        || !matches!(array.get(0), Some(Value::Number(n)) if twenty(n))
        || !matches!(array.get(1), Some(Value::Number(n)) if thirty(n))
    {
        return Err(format!("range materialized {array:?}, want [20, 30]"));
    }
    // Validate-everything-first: a corrupt byte ANYWHERE fails the scoped
    // route, even when the path never reads it.
    if run_located(b"[t]\nx = 1 garbage\n", &["t", "x"], None, None).is_ok() {
        return Err("scoped route accepted corrupt input".into());
    }

    // The ROUTE-PARITY receipt ( S27): dotted keys inside inline
    // tables. The scoped route must answer byte-identically to the
    // whole-document route for every shape the grammar admits — the law the
    // defect broke (the walk parsed ONE key where the grammar permits a
    // dotted path, so valid input failed with a parse error while the
    // whole-document route answered it). For each fixture the located
    // observation (node value / missing step / mismatch step+kind) must
    // equal the whole-document navigation's, and a node answer must ENCODE
    // to the same bytes.
    let parity_fixtures: &[(&[u8], &[&str], Option<i64>)] = &[
        // The headline case: a full-path match through a dotted key.
        (b"animal = { type.name = \"pug\" }\n", &["animal", "type", "name"], None),
        (b"animal = { type.name = \"pug\" }\n", &["animal", "type"], None),
        // Multi-segment, quoted, and mixed entries.
        (b"a = { b.c.d = 1 }\n", &["a", "b", "c"], None),
        (b"a = { b.c.d = 1 }\n", &["a", "b"], None),
        (b"a = { \"b.c\".d = 1 }\n", &["a", "b.c", "d"], None),
        (b"a = { \"b.c\".d = 1 }\n", &["a", "b.c"], None),
        (b"a = { \"b\\\"c\".d = 1 }\n", &["a", "b\"c"], None),
        (b"a = { \"x y\".d = 1 }\n", &["a", "x y"], None),
        (b"a = { b = 1, c.d = 2 }\n", &["a", "c"], None),
        (b"a = { b = 1, c.d = 2 }\n", &["a", "c", "d"], None),
        (b"a = { b = 1, c.d = 2 }\n", &["a", "b"], None),
        (b"a = { b = 1, c.d = 2 }\n", &["a", "b", "c"], None),
        // Whitespace around the dots and the comma.
        (b"a = { b . c = 1 }\n", &["a", "b", "c"], None),
        (b"a = { b.c = 1 , d.e = 2 }\n", &["a", "d", "e"], None),
        // Multiple entries contribute to one implicit table, in scan order.
        (b"a = { b.c = 1, b.d = 2 }\n", &["a", "b"], None),
        (b"a = { b.c = 1, b.d = 2 }\n", &["a", "b", "d"], None),
        (b"a = { b.c = 1, b.d = { e = 2 } }\n", &["a", "b", "d", "e"], None),
        // Negatives carry the same step and kind as the whole route's.
        (b"a = { b.c = 1 }\n", &["a", "b", "x"], None),
        (b"a = { b.c = 1 }\n", &["a", "x"], None),
        (b"a = { b.c = 1 }\n", &["a", "x", "y"], None),
        (b"a = { b.c = 1 }\n", &["a", "b", "c", "d"], None),
        (b"a = { b.c = 1 }\n", &["a", "b", "c"], Some(0)),
        (b"a = { b.c = 1 }\n", &["a", "b"], Some(0)),
        // Nested containers: the descent through an array element restores
        // the enclosing scan's offset.
        (b"a = { b = [ { c.d = 1 } ] }\n", &["a", "b"], Some(0)),
        (b"a = { b = [ { c.d = 1 } ], e = 2 }\n", &["a", "e"], None),
        (b"a = { b.c = [1, 2] }\n", &["a", "b", "c"], Some(1)),
        // The whole inline table is still reachable as a value.
        (b"a = { b.c = 1 }\n", &["a"], None),
        // The regression rows from the defect brief: the shape the scoped
        // route falsely rejected, plus the three shapes that worked.
        (b"a = { b.c = 1 }\n", &["a", "b"], None),
        (b"a.b = 1\n", &["a", "b"], None),
        (b"a = { b = { c = 1 } }\n", &["a", "b"], None),
        (b"[t]\na.b = 1\n", &["t", "a", "b"], None),
    ];
    let mut parity = 0u32;
    for (bytes, members, index) in parity_fixtures {
        let whole = decode(bytes).map_err(|e| format!("decode parity fixture: {e:?}"))?;
        let expected = navigate_whole(&whole, members, *index);
        let (located, _, _) = run_located(bytes, members, *index, None)?;
        match (expected, located.result()) {
            (Navigated::Value(expected_value), ExactSelectionRecord::Node { .. }) => {
                let mut scoped_materialize = resources();
                let value = located
                    .product()
                    .document()
                    .materialize_root(&mut scoped_materialize)
                    .map_err(|_| "materialize parity scoped value".to_owned())?;
                // The structural comparison is kind-agnostic (scalars,
                // arrays, and objects alike); `Value`'s `Debug` renders the
                // contained data, never addresses.
                if format!("{value:?}") != format!("{expected_value:?}") {
                    return Err(format!(
                        "dotted parity value mismatch for {members:?} on {:?}: \
                         whole={expected_value:?} scoped={value:?}",
                        String::from_utf8_lossy(bytes),
                    ));
                }
            }
            (Navigated::Missing { step }, ExactSelectionRecord::Missing { step_index, .. }) => {
                if *step_index != step {
                    return Err(format!(
                        "dotted parity missing step for {members:?} on {:?}: \
                         whole={step} scoped={step_index}",
                        String::from_utf8_lossy(bytes),
                    ));
                }
            }
            (
                Navigated::Mismatch { step, kind },
                ExactSelectionRecord::TypeMismatch {
                    step_index,
                    actual_type,
                    ..
                },
            ) => {
                if *step_index != step || *actual_type != kind {
                    return Err(format!(
                        "dotted parity mismatch for {members:?} on {:?}: \
                         whole=({step}, {kind:?}) scoped=({step_index}, {actual_type:?})",
                        String::from_utf8_lossy(bytes),
                    ));
                }
            }
            (expected, actual) => {
                return Err(format!(
                    "dotted parity observation mismatch for {members:?} on {:?}: \
                     whole={expected:?} scoped={actual:?}",
                    String::from_utf8_lossy(bytes),
                ));
            }
        }
        parity += 1;
    }

    // The GRAMMAR-DRIFT FENCE: whole-parse and scoped-validate agree on
    // accept/reject + error code + byte position under deterministic byte
    // mutation. The scoped route now opens through the BYTE WALK (scaling
    // plan Lever A), which shares the grammar's lexers and table state but
    // owns the container framing — so the fence's job is bigger than before:
    // it must prove the walk rejects exactly what the parser rejects, with
    // the same code at the same position. The bases cover the walk's own
    // skip surface: strings (escapes, multiline), numbers, temporals, arrays
    // (nested, trailing comma), inline tables (1.0/1.1 separator laws),
    // dotted keys, and array-of-tables, plus the mutation of EVERY byte and
    // a second deterministic pass mutating two bytes.
    let bases: &[&[u8]] = &[
        b"[t]\nx = 1\ny = \"s\"\nz = [1, 2, 3]\n",
        b"esc = \"a\\u0062\\n\\t\"\nml = \"\"\"\nline\n\"\"\"\n",
        b"hex = 0x1F\noct = 0o17\nbig = 1_000_000\nfloat = 1.5e2\ninf = inf\n",
        b"d1 = 1979-05-27\nt1 = 07:32:00\nodt = 1979-05-27T07:32:00Z\n",
        b"point = { x = 1, y = 2, z = { w = \"deep\" } }\narr = [[1, 2], [3]]\ntrail = [1, 2, 3,]\n",
        b"a.b.c = 1\nx.y.z.w = \"deep\"\n",
        b"[[p]]\nname = \"Hammer\"\n[[p]]\nname = \"Nail\"\n[p.extra]\nk = 1\n",
    ];
    let mut drift_state = 0x5eed_u32;
    let mut checked = 0u32;
    for (base_index, base) in bases.iter().enumerate() {
        for _ in 0..512 {
            drift_state = drift_state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let mutation = drift_state;
            let mut bytes = base.to_vec();
            let position = (mutation as usize) % bytes.len();
            bytes[position] = u8::try_from(mutation & 0xff).expect("byte");
            let whole = decode(&bytes).err().map(|error| {
                let diagnostic = error.diagnostic().expect("structured diagnostic");
                (
                    diagnostic.code().name().to_owned(),
                    diagnostic.labels().first().expect("primary label").span().start(),
                )
            });
            let scoped = run_located(&bytes, &["t", "x"], None, None).err().map(|_| ());
            let scoped_reject = scoped.is_some();
            if whole.is_some() != scoped_reject {
                return Err(format!(
                    "grammar drift at base {base_index} mutation {mutation:#x} pos {position}: \
                     whole={:?} scoped_reject={scoped_reject}",
                    whole.as_ref().map(|(code, offset)| (code.clone(), *offset))
                ));
            }
            if let Some((whole_code, whole_offset)) = &whole {
                // Error CODE and POSITION parity is now load-bearing too: the
                // walk shares the lexers (codes/offsets identical by
                // construction there) but owns the container framing, so a
                // walk-side framing mistake must not only reject the same
                // input but at the same byte with the same code.
                let Err(scoped_error) = run_located_raw(&bytes, &["t", "x"], None, None) else {
                    return Err("scoped route accepted what the whole parser rejected".into());
                };
                let diagnostic = scoped_error.diagnostic().expect("scoped diagnostic");
                let scoped_code = diagnostic.code().name();
                let scoped_offset = diagnostic.labels().first().expect("primary label").span().start();
                if scoped_code != whole_code || scoped_offset != *whole_offset {
                    return Err(format!(
                        "grammar drift at base {base_index} mutation {mutation:#x}: \
                         whole=({whole_code}, {whole_offset}) scoped=({scoped_code}, {scoped_offset})"
                    ));
                }
            }
            checked += 1;
        }
    }

    assert_resource_depth()?;
    println!(
        "toml-smoke: accepted={accepted} rejected={rejected} routes=2 encode=true tags=true roundtrips=true source_spans=true scoped=true resource_depth=true drift_fence={checked} dotted_parity={parity}"
    );
    Ok(())
}
