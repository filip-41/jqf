//! JSON5 codec receipt battery (the JSON5 vertical's gate, plan 135
//! S2/S3 + lane 145 E3).
//!
//! Pins the laws the codec must hold: the registration identity (format,
//! dialect, extension), JSON ⊂ JSON5 (every strict document decodes
//! byte-identically — the conformance corpus's first stand-in, 135 S4),
//! the grammar arms (unquoted keys, single quotes, hex, leading/trailing
//! decimal points, `Infinity`/`NaN`, comments, trailing commas), the
//! `json5.comment@1` leading-comment facts (D11), and the encoder's
//! comment re-emission.

use crate::drive::{resources, resume, source, whole_requirement};
use jqf_codec_core::{
    AccessOutcome, CodecFailureKind, CodecRunContext, DecodeRequest, DiagnosticPolicy, EncodeItem, EncodeRequest,
    FactIntent, PreservationRequest, ValidationMode,
};
use jqf_data::{DialectId, FormatId, LocalOwnerRef, ReaderPoll};

/// Decodes one STRICT JSON document through the strict codec's
/// whole-document route — the conformance oracle every JSON5 decode is
/// compared against.
fn strict_json_value(bytes: &[u8]) -> Result<jqf_data::Value, CodecFailureKind> {
    let registration = jqf_codec_json::registration().map_err(|_| CodecFailureKind::InternalContractViolation {
        contract: "json registration",
    })?;
    whole_value_with(&registration, jqf_codec_json::RFC8259_DIALECT_ID, bytes)
}

/// Decodes one JSON5 document through the whole-document route, returning
/// the materialized root value or the failure kind.
fn whole_value(bytes: &[u8]) -> Result<jqf_data::Value, CodecFailureKind> {
    let registration =
        jqf_codec_json::json5::registration().map_err(|_| CodecFailureKind::InternalContractViolation {
            contract: "json5 registration",
        })?;
    whole_value_with(&registration, jqf_codec_json::json5::DOCUMENT_DIALECT_ID, bytes)
}

fn whole_value_with(
    registration: &jqf_codec_core::CodecRegistration<'static>,
    dialect: &str,
    bytes: &[u8],
) -> Result<jqf_data::Value, CodecFailureKind> {
    let mut resources = resources();
    let dialect = DialectId::try_new(dialect).expect("dialect");
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
                value_separator: jqf_codec_json::VALUE_SEPARATORS,
            },
            &mut resources,
        )
        .map_err(|error| error.kind())?;
    let requirement = whole_requirement(&resources);
    let handle = provider
        .bind(&requirement)
        .map_err(|_| CodecFailureKind::InternalContractViolation { contract: "json5 bind" })?;
    let mut session = provider.open(&handle, &mut resources).map_err(|error| error.kind())?;
    let result = {
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        session.decode(&mut run).map_err(|error| error.kind())?
    };
    let (outcome, _) = result.into_parts();
    let AccessOutcome::FullDocument(product) = outcome else {
        return Err(CodecFailureKind::InternalContractViolation {
            contract: "json5 whole route",
        });
    };
    product
        .document()
        .materialize_root(&mut resources)
        .map_err(|_| CodecFailureKind::InternalContractViolation {
            contract: "json5 materialize",
        })
}

/// Encodes one decoded product through the JSON5 encoder, returning the
/// complete bytes.
fn encode_whole(
    product: &jqf_codec_core::DocumentProduct<'_>,
    resources: &mut jqf_resource::ResourceContext<'_>,
) -> Result<Vec<u8>, CodecFailureKind> {
    let registration =
        jqf_codec_json::json5::registration().map_err(|_| CodecFailureKind::InternalContractViolation {
            contract: "json5 registration",
        })?;
    let factory = registration
        .encoder()
        .expect("encoder")
        .create_factory(
            EncodeRequest {
                format: &FormatId::try_new(jqf_codec_json::json5::FORMAT_ID).expect("format"),
                dialect: &DialectId::try_new(jqf_codec_json::json5::JQF_DIALECT_ID).expect("dialect"),
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                preservation: PreservationRequest::Report,
                options: Some(&jqf_codec_json::JsonEncodeOptions::default()),
            },
            resources,
        )
        .map_err(|error| error.kind())?;
    let item = EncodeItem::try_located(product, product.document().root_handle()).expect("located");
    let mut session = factory
        .start(item, PreservationRequest::Report, resources)
        .map_err(|error| error.kind())?;
    let mut out = Vec::new();
    {
        let mut sink = jqf_codec_core::VecByteSink::new(&mut out);
        let mut run = jqf_codec_core::CodecRunContext::new(resources);
        run.set_cooperative_credits(4_096);
        session.encode(&mut sink, &mut run).map_err(|error| error.kind())?;
    }
    Ok(out)
}

pub fn run() -> Result<(), String> {
    // Registration validity.
    let registration =
        jqf_codec_json::json5::registration().map_err(|error| format!("invalid JSON5 registration: {error:?}"))?;
    let descriptor = registration.descriptor();
    if descriptor.format().as_str() != jqf_codec_json::json5::FORMAT_ID {
        return Err("JSON5 registration names the wrong format".into());
    }
    if descriptor.dialects().len() != 3 {
        return Err(format!(
            "JSON5 registration must carry the 1 input + 2 output dialects, found {}",
            descriptor.dialects().len()
        ));
    }
    if !descriptor.extensions().contains(&"json5") {
        return Err("JSON5 registration must declare the .json5 extension".into());
    }

    // JSON ⊂ JSON5: every strict document decodes to the same value the
    // strict json codec produces (135 S4's first stand-in — the
    // byte-identity half is the compat corpus's job).
    for bytes in [
        &b"null"[..],
        b"true",
        b"123",
        b"1.5e3",
        b"\"a\\n\\u0041\"",
        b"[]",
        b"[1,2,{\"a\":[]}]",
        b"{\"a\":1,\"b\":[true,false,null]}",
    ] {
        let strict_value =
            strict_json_value(bytes).map_err(|kind| format!("strict json rejected {bytes:?}: {kind:?}"))?;
        let value = whole_value(bytes).map_err(|kind| format!("strict document rejected under json5: {kind:?}"))?;
        if format!("{value:?}") != format!("{strict_value:?}") {
            return Err(format!("strict document under json5 diverges: {value:?}"));
        }
    }

    // The JSON5 grammar arms (plan 135 D7/D4): every extension spelling
    // decodes, and the value matches the strict spelling of the same
    // document.
    for (extended, strict) in [
        (&b"{a: 'x', b: 2}"[..], &b"{\"a\":\"x\",\"b\":2}"[..]),
        (b"0x10", b"16"),
        (b"-0xff", b"-255"),
        (b".5", b"0.5"),
        (b"+7", b"7"),
        (b"{\n  // lead\n  a: 1, // inline\n  b: 2,\n}\n", b"{\"a\":1,\"b\":2}"),
        (b"'\\x41'", b"\"A\""),
        (b"'it\\'s'", b"\"it's\""),
        (b"'a\\\nb'", b"\"ab\""),
    ] {
        let value = whole_value(extended).map_err(|kind| format!("json5 spelling rejected: {kind:?}"))?;
        let strict_value =
            strict_json_value(strict).map_err(|kind| format!("strict oracle rejected {strict:?}: {kind:?}"))?;
        if format!("{value:?}") != format!("{strict_value:?}") {
            return Err(format!("json5 {extended:?} diverges from {strict:?}: {value:?}"));
        }
    }

    // `Infinity`/`NaN` decode to the pinned non-finite numbers.
    for spelling in [&b"Infinity"[..], b"-Infinity", b"NaN", b"-NaN"] {
        let value = whole_value(spelling).map_err(|kind| format!("non-finite spelling rejected: {kind:?}"))?;
        if !format!("{value:?}").contains("Float") {
            return Err(format!("{spelling:?} must decode to a Float: {value:?}"));
        }
    }

    // The comment grammar: line and block comments accepted, unterminated
    // block comment rejected.
    for bytes in [
        &b"{\"a\": 1 // line comment\n}"[..],
        b"{\"a\": /* block */ 1}",
        b"{\"a\": 1, /* trailing */}",
        b"// leading\n{\"a\": 1}",
    ] {
        whole_value(bytes).map_err(|kind| format!("comment document rejected: {kind:?}"))?;
    }
    if whole_value(b"{\"a\": 1 /* never closes}").is_ok() {
        return Err("an unterminated block comment must be rejected".into());
    }

    // Trailing commas are JSON5 grammar (accepted).
    if whole_value(b"{\"a\": 1,}").is_err() {
        return Err("json5 must accept a trailing comma".into());
    }

    // Comment facts (plan 135 D11): the leading comment attaches to the
    // VALUE node of the member it precedes, under the `json5.comment@1`
    // role, and the encoder re-emits it (T6 round-trip).
    {
        let mut resources = resources();
        let bytes = b"{\n  // name comment\n  name: 'ada',\n}\n";
        let registration = jqf_codec_json::json5::registration().map_err(|error| format!("{error:?}"))?;
        let dialect = DialectId::try_new(jqf_codec_json::json5::DOCUMENT_DIALECT_ID).expect("dialect");
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
                    value_separator: jqf_codec_json::VALUE_SEPARATORS,
                },
                &mut resources,
            )
            .map_err(|error| format!("{error:?}"))?;
        let requirement = whole_requirement(&resources).with_fact_intent(FactIntent::Preserve);
        let handle = provider.bind(&requirement).map_err(|error| format!("{error:?}"))?;
        let mut session = provider
            .open(&handle, &mut resources)
            .map_err(|error| format!("{error:?}"))?;
        let result = {
            let mut run = CodecRunContext::new(&mut resources);
            run.set_cooperative_credits(4_096);
            session.decode(&mut run).map_err(|error| format!("{error:?}"))?
        };
        let (outcome, _) = result.into_parts();
        let AccessOutcome::FullDocument(product) = outcome else {
            return Err("json5 whole route".into());
        };
        let document = product.document();
        let limit = jqf_data::BatchLimit::new(usize::MAX).expect("limit");
        let mut reader = document.fact_reader(&mut resources).expect("reader");
        let mut found = Vec::new();
        loop {
            match reader.poll_batch(limit, &mut resources).expect("poll") {
                ReaderPoll::Batch(batch) => {
                    for fact in batch.iter() {
                        let LocalOwnerRef::Node(node) = fact.owner() else {
                            continue;
                        };
                        if fact.role().as_str() != "json5.comment@1" {
                            continue;
                        }
                        let jqf_data::FactPayloadView::List(texts) = fact.payload() else {
                            continue;
                        };
                        let lines: Vec<String> = texts
                            .iter()
                            .filter_map(|entry| match entry {
                                jqf_data::FactPayloadView::Text(text) => Some(String::from(text)),
                                _ => None,
                            })
                            .collect();
                        found.push((node, lines));
                    }
                }
                ReaderPoll::Pending => {
                    // Work credits are not replenished by the bare read loop;
                    // a Pending poll must resume or it spins forever (the
                    // jsonc-smoke hang, mirrored here).
                    resume(&mut resources);
                }
                ReaderPoll::End(_) => break,
            }
        }
        if found.len() != 1 {
            return Err(format!("expected one comment fact, found {found:?}"));
        }
        if found[0].1 != vec![String::from("name comment")] {
            return Err(format!("member comment text wrong: {:?}", found[0].1));
        }
        if found[0].0 == document.root() {
            return Err("the member comment must not attach to the root".into());
        }

        // The encoder re-emits the comment facts (T6 round-trip).
        let bytes = encode_whole(&product, &mut resources).map_err(|kind| format!("{kind:?}"))?;
        let text = String::from_utf8_lossy(&bytes);
        if !text.contains("// name comment") {
            return Err(format!("encoder dropped the comment: {text}"));
        }
        if !text.contains("\"name\"") {
            return Err(format!("encoder dropped the member: {text}"));
        }
    }

    Ok(())
}
