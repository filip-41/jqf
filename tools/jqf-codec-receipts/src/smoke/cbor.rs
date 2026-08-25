//! CBOR (RFC 8949) codec receipt battery.
//!
//! Pins the CBOR codec's surface as of the whole-route stage: the
//! registration's dialect set (the generic input dialect plus the four output
//! profiles, with the well-formed identity reserved and NOT advertised), the
//! advertised route inventory (slot 0 Whole plus slot 1 Exact/Located —
//! the two-slot ladder), and a
//! whole-route decode corpus covering the generic-data-model
//! grammar: scalars, definite and indefinite containers, §5.6.1 key
//! uniqueness, recognized-tag projection (tags 0-5), and the tag-layer chain
//! materialization (nested `Value::Tagged`).

use crate::drive::{resources, source, whole_requirement};
use jqf_codec_core::{
    AccessFootprintKind, AccessOutcome, AccessResultKind, CodecRunContext, DecodeRequest, DiagnosticPolicy,
    ValidationMode,
};
use jqf_data::{DialectId, Value};
use jqf_sdk::ItemSink;

/// Drives the CBOR whole-route provider to one materialized root value.
fn decode(bytes: &[u8]) -> Result<Value, String> {
    let mut resources = resources();
    let registration = jqf_codec_cbor::registration().map_err(|error| format!("{error:?}"))?;
    let mut provider = registration
        .decoder()
        .expect("cbor decoder factory")
        .create_provider(
            source(bytes),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(jqf_codec_cbor::CBOR_PREFERRED_DIALECT_ID).expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            &mut resources,
        )
        .map_err(|error| format!("cbor provider: {error:?}"))?;
    let requirement = whole_requirement(&resources);
    let handle = provider
        .bind(&requirement)
        .map_err(|error| format!("bind: {error:?}"))?;
    let mut session = provider
        .open(&handle, &mut resources)
        .map_err(|error| format!("open: {error:?}"))?;
    {
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        let result = session.decode(&mut run).map_err(|error| format!("decode: {error:?}"))?;
        let AccessOutcome::FullDocument(product) = result.outcome() else {
            return Err("expected full document".into());
        };
        product
            .document()
            .materialize_root(&mut resources)
            .map_err(|error| format!("materialize: {error:?}"))
    }
}

/// The kind of a reject corpus row: the exact codec failure family expected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RejectKind {
    InvalidInput,
    UnsupportedRepresentation,
}

fn expect_reject(bytes: &[u8], kind: RejectKind) -> Result<(), String> {
    match decode(bytes) {
        Ok(_) => Err(format!("expected reject for {bytes:02x?}")),
        Err(error) if error.contains("InvalidInput") && kind == RejectKind::InvalidInput => Ok(()),
        Err(error) if error.contains("UnsupportedRepresentation") && kind == RejectKind::UnsupportedRepresentation => {
            Ok(())
        }
        Err(other) => Err(format!("expected {kind:?} for {bytes:02x?}, got {other}")),
    }
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
        // `as_slice` over `as_ref`: winnow (via the harness's pinned `toml`
        // oracle dep) implements `AsRef` for `[u8]`, so `as_ref` is ambiguous
        // once the toml differential references that crate (rustc loads the
        // dependency's metadata lazily). Same bytes, inherent method.
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
        V::Tagged { tag, payload } => {
            format!("{}({})", tag.as_str(), render(payload))
        }
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

/// A sink that collects every published byte and counts items, for the
/// cbor-seq sequence rows' byte-identity accounting.
struct ByteSink {
    bytes: Vec<u8>,
    items: u64,
}

impl ItemSink for ByteSink {
    type Error = &'static str;

    fn begin_item(&mut self, _index: u64) -> Result<(), Self::Error> {
        self.items += 1;
        Ok(())
    }

    fn write(&mut self, bytes: &[u8]) -> Result<usize, Self::Error> {
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn finish_item(&mut self, _index: u64, _report: jqf_sdk::EncodedItemReport) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// The three named cbor-seq receipts (plan 138 S1), driven through the SDK's
/// real adjacent-value drive over the registered cbor-seq identities:
///
/// - **round-trip**: N items in → N items out, byte-identical (an RFC 8742
///   sequence is concatenated items, so the encode half writes no framing
///   bytes at all);
/// - **zero-item empty input** (D5): empty input is a zero-item success;
/// - **separator canary** (D7): the two bytes `20 20` decode as TWO `-1`
///   items, never one — the drive's value-separator set is empty for
///   cbor-seq, so `0x20` is a complete item, not insignificant whitespace.
fn sequence_rows() -> Result<(), String> {
    fn run(input: &[u8]) -> Result<(Vec<u8>, u64), String> {
        let mut resources = resources();
        let cbor = jqf_codec_cbor::registration().map_err(|e| format!("{e:?}"))?;
        let cbor_seq = jqf_codec_cbor::seq::registration().map_err(|e| format!("{e:?}"))?;
        let registrations = [&cbor, &cbor_seq];
        let catalog = jqf_sdk::CodecCatalog::new(&registrations);
        let format = jqf_data::FormatId::try_new(jqf_codec_cbor::seq::FORMAT_ID).map_err(|e| e.to_string())?;
        let dialect =
            jqf_data::DialectId::try_new(jqf_codec_cbor::seq::RFC8742_GENERIC_DIALECT_ID).map_err(|e| e.to_string())?;
        let output_format = jqf_data::FormatId::try_new(jqf_codec_cbor::seq::FORMAT_ID).map_err(|e| e.to_string())?;
        let output_dialect =
            jqf_data::DialectId::try_new(jqf_codec_cbor::seq::JQF_DIALECT_ID).map_err(|e| e.to_string())?;
        let policy = jqf_engine::CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
        let program =
            jqf_engine::try_compile_program(".", policy, &resources).map_err(|error| format!("compile: {error}"))?;
        let requirement = program
            .try_requirement(&resources)
            .map_err(|error| format!("requirement: {:?}", error.kind()))?;
        let src = source(input);
        let policy_dialect = DialectId::try_new(jqf_codec_cbor::seq::RFC8742_GENERIC_DIALECT_ID).expect("dialect");
        let encode_options = jqf_codec_cbor::seq::CborSeqEncodeOptions::default();
        let mut sink = ByteSink {
            bytes: Vec::new(),
            items: 0,
        };
        let request = jqf_sdk::Request::new(&program, jqf_sdk::Input::Whole(input))
            .with_catalog(catalog)
            .with_source(src)
            .with_format(format, dialect)
            .with_output_format(output_format, output_dialect)
            .with_policy(jqf_sdk::PipelinePolicy {
                decode: DecodeRequest {
                    validation: ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect: &policy_dialect,
                    options: None,
                    // cbor-seq's whole contract: the adjacent opt-in ON and
                    // the value-separator set EMPTY (the D7 law).
                    allow_adjacent_values: true,
                    value_separator: &[],
                },
                encode_diagnostics: DiagnosticPolicy::ErrorsOnly,
                preservation: jqf_codec_core::PreservationRequest::None,
                encode_options: Some(&encode_options as &(dyn core::any::Any + Send + Sync)),
                cooperative_credits: 4_096,
                split: None,

                max_iterations: None,
            })
            .with_framing(jqf_sdk::FacadeFraming::item_suffix(b""))
            .with_resources(&mut resources)
            .with_requirement(&requirement);
        let outcome = jqf_sdk::execute(request, &mut sink)
            .map_err(|error| format!("sequence: {:?}", error.pipeline_failure()))?;
        if !matches!(outcome, jqf_sdk::Outcome::Served(_)) {
            return Err(format!("cbor-seq sequence did not serve: {outcome:?}"));
        }
        Ok((sink.bytes, sink.items))
    }

    // Round-trip: `1` (0x01), `-1` (0x20), `2` (0x02) — three items in,
    // three items out, byte-identical (no framing bytes to add or strip).
    let items_in: &[u8] = &[0x01, 0x20, 0x02];
    let (bytes, items) = run(items_in)?;
    if items != 3 || bytes != items_in {
        return Err(format!(
            "cbor-seq round-trip broke: items={items} bytes={bytes:02x?} (expected {items_in:02x?})"
        ));
    }
    // Zero-item empty input (D5): no items, no bytes, a successful run.
    let (bytes, items) = run(&[])?;
    if items != 0 || !bytes.is_empty() {
        return Err("cbor-seq empty input must be a zero-item success".into());
    }
    // The separator canary (D7): `20 20` is TWO `-1` items, never one. This
    // is also the gates-teeth probe — today's code (before this wave) drops
    // the first `0x20` as whitespace and decodes one item.
    let (bytes, items) = run(&[0x20, 0x20])?;
    if items != 2 || bytes != [0x20, 0x20] {
        return Err(format!(
            "the separator canary failed: items={items} bytes={bytes:02x?} (expected two 0x20)"
        ));
    }
    Ok(())
}

/// Pins the registration surface: five dialects, generic advertised and the
/// well-formed identity reserved.
fn registration_surface() -> Result<(), String> {
    let registration = jqf_codec_cbor::registration().map_err(|error| format!("{error:?}"))?;
    let descriptor = registration.descriptor();
    if descriptor.format().as_str() != "cbor" {
        return Err(format!("unexpected format {}", descriptor.format().as_str()));
    }
    let dialects = descriptor.dialects();
    let expected = [
        "cbor.rfc8949-generic@1",
        "cbor.source@1",
        "cbor.preferred@1",
        "cbor.core-deterministic@1",
        "cbor.length-first@1",
    ];
    if dialects.len() != expected.len()
        || dialects
            .iter()
            .zip(expected)
            .any(|(left, right)| left.as_str() != right)
    {
        return Err(format!(
            "unexpected CBOR dialect set: {}",
            dialects
                .iter()
                .map(|dialect| dialect.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if dialects
        .iter()
        .any(|dialect| dialect.as_str() == "cbor.rfc8949-well-formed@1")
    {
        return Err("the reserved well-formed dialect must not be advertised".into());
    }
    let _ = DialectId::try_new("cbor.rfc8949-generic@1").map_err(|error| format!("{error:?}"))?;
    Ok(())
}

/// Pins the route inventory: the two-route table at slots 0..1 — `Whole`/
/// `CompleteDocument`, `Exact`/`Located`.
fn route_inventory() -> Result<(), String> {
    let mut resources = resources();
    let registration = jqf_codec_cbor::registration().map_err(|error| format!("{error:?}"))?;
    let provider = registration
        .decoder()
        .expect("cbor decoder factory")
        .create_provider(
            source(b"\x01"),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(jqf_codec_cbor::CBOR_PREFERRED_DIALECT_ID).expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            &mut resources,
        )
        .map_err(|error| format!("provider: {:?}", error.kind()))?;
    let routes = provider.route_descriptions();
    let expected = [AccessResultKind::CompleteDocument, AccessResultKind::Located];
    if routes.len() != expected.len()
        || routes.iter().zip(expected).any(|(route, result)| {
            route.slot().get() != u32::try_from(route_index(result)).unwrap_or(u32::MAX)
                || route.bundle().result() != result
        })
    {
        return Err(format!(
            "CBOR advertised {} routes ({}); expected the two-route table",
            routes.len(),
            routes
                .iter()
                .map(|route| route.slot().get())
                .collect::<Vec<_>>()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    if routes[0].bundle().footprint() != AccessFootprintKind::Whole {
        return Err("CBOR slot 0 is not Whole".into());
    }
    for route in &routes[1..] {
        if route.bundle().footprint() != AccessFootprintKind::Exact {
            return Err(format!("CBOR slot {} is not Exact", route.slot().get()));
        }
    }
    Ok(())
}

fn route_index(result: AccessResultKind) -> usize {
    match result {
        AccessResultKind::CompleteDocument => 0,
        AccessResultKind::Located => 1,
        // The only remaining result kind is the record stream, which CBOR
        // never advertises — a future
        // result kind is a smoke-inventory change.
        AccessResultKind::RecordStream => usize::MAX,
    }
}

/// The whole-route decode corpus.
fn decode_corpus() -> Result<(), String> {
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
        (
            &[0xc1, 0xfb, 0x3f, 0xf8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
            "1970-01-01T00:00:01.5Z",
        ),
        (&[0xd9, 0xd9, 0xf7, 0x61, 0x78], "cbor:tag:55799(\"x\")"),
        (
            &[0xd9, 0xd9, 0xf7, 0xd8, 0x22, 0x82, 0x01, 0x02],
            "cbor:tag:55799(cbor:tag:34([1, 2]))",
        ),
    ];
    for (bytes, expected) in cases {
        let value = decode(bytes).map_err(|error| format!("decode failed for {bytes:02x?}: {error}"))?;
        let rendered = render(&value);
        if rendered != *expected {
            return Err(format!(
                "decode mismatch for {bytes:02x?}: got {rendered:?}, expected {expected:?}"
            ));
        }
    }
    Ok(())
}

/// The generic-validity reject corpus.
fn reject_corpus() -> Result<(), String> {
    // Duplicate text keys per §5.6.1.
    expect_reject(&[0xa2, 0x61, 0x61, 0x01, 0x61, 0x61, 0x02], RejectKind::InvalidInput)?;
    // Non-text map key: valid CBOR, not projectable for the semantic root.
    expect_reject(&[0xa1, 0x01, 0x61, 0x61], RejectKind::UnsupportedRepresentation)?;
    // Invalid UTF-8 in a text string.
    expect_reject(&[0x61, 0xff], RejectKind::InvalidInput)?;
    // Trailing bytes after the single top-level item.
    expect_reject(&[0x81, 0x01, 0x00], RejectKind::InvalidInput)?;
    // Reserved additional-information value 28.
    expect_reject(&[0x1c], RejectKind::InvalidInput)?;
    // Reserved simple value 31.
    expect_reject(&[0xf8, 0x1f], RejectKind::InvalidInput)?;
    // Tag 1 with a non-integer/float payload (a tag) is a raw-shape failure.
    expect_reject(&[0xc1, 0xc2, 0x41, 0x01], RejectKind::InvalidInput)?;
    // Tag 2 whose payload is not a byte string.
    expect_reject(&[0xc2, 0x61, 0x78], RejectKind::InvalidInput)?;
    Ok(())
}

pub fn run() -> Result<(), String> {
    let results = [
        ("registration surface", registration_surface()),
        ("route inventory", route_inventory()),
        ("sequence rows", sequence_rows()),
        ("decode corpus", decode_corpus()),
        ("reject corpus", reject_corpus()),
    ];
    let mut failures = 0;
    for (label, result) in results {
        match result {
            Ok(()) => println!("cbor-smoke: {label}: ok"),
            Err(error) => {
                failures += 1;
                println!("cbor-smoke: {label}: FAIL: {error}");
            }
        }
    }
    if failures != 0 {
        return Err(format!("{failures} receipt(s) failed"));
    }
    println!("cbor-smoke: all receipts pass");
    Ok(())
}
