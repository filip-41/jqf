//! JSONC codec receipt battery.
//!
//! Pins the laws the codec must hold: the registration identity (format,
//! dialects, extension), JSONC ⊇ JSON (every strict document decodes
//! byte-identically — the conformance corpus's first stand-in, 135 S4),
//! the comment grammar (accepts `//` and `/* */`, rejects an unterminated
//! comment), the trailing-comma dialect difference (D9), the
//! `jsonc.comment@1` leading-comment facts (D11), and the encoder's
//! comment re-emission and trailing-comma output profiles.

use crate::drive::{resources, resume, source, whole_requirement};
use jqf_codec_core::{
    AccessOutcome, CodecFailureKind, CodecRunContext, DecodeRequest, DiagnosticPolicy, EncodeItem, EncodeRequest,
    FactIntent, PreservationRequest, ValidationMode,
};
use jqf_data::{DialectId, FormatId, LocalOwnerRef, ReaderPoll, Value};

/// Decodes one STRICT JSON document through the strict codec's
/// whole-document route — the conformance oracle every JSONC decode is
/// compared against.
fn strict_json_value(bytes: &[u8]) -> Result<Value, CodecFailureKind> {
    let registration = jqf_codec_json::registration().map_err(|_| CodecFailureKind::InternalContractViolation {
        contract: "json registration",
    })?;
    whole_value_with(&registration, jqf_codec_json::RFC8259_DIALECT_ID, bytes)
}

/// Decodes one JSONC document through the whole-document route, returning
/// the materialized root value or the failure kind.
fn whole_value(bytes: &[u8], dialect: &str) -> Result<Value, CodecFailureKind> {
    let registration =
        jqf_codec_json::jsonc::registration().map_err(|_| CodecFailureKind::InternalContractViolation {
            contract: "jsonc registration",
        })?;
    whole_value_with(&registration, dialect, bytes)
}

fn whole_value_with(
    registration: &jqf_codec_core::CodecRegistration<'static>,
    dialect: &str,
    bytes: &[u8],
) -> Result<Value, CodecFailureKind> {
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
        .map_err(|_| CodecFailureKind::InternalContractViolation { contract: "jsonc bind" })?;
    let mut session = provider.open(&handle, &mut resources).map_err(|error| error.kind())?;
    let result = {
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        session.decode(&mut run).map_err(|error| error.kind())?
    };
    let (outcome, _) = result.into_parts();
    let AccessOutcome::FullDocument(product) = outcome else {
        return Err(CodecFailureKind::InternalContractViolation {
            contract: "jsonc whole route",
        });
    };
    product
        .document()
        .materialize_root(&mut resources)
        .map_err(|_| CodecFailureKind::InternalContractViolation {
            contract: "jsonc materialize",
        })
}

/// Encodes one decoded product through the JSONC encoder with the default
/// (trailing) profile, returning the complete bytes.
fn encode_whole(
    product: &jqf_codec_core::DocumentProduct<'_>,
    resources: &mut jqf_resource::ResourceContext<'_>,
    profile: jqf_codec_json::jsonc::JsoncEncodeProfile,
) -> Result<Vec<u8>, CodecFailureKind> {
    let registration =
        jqf_codec_json::jsonc::registration().map_err(|_| CodecFailureKind::InternalContractViolation {
            contract: "jsonc registration",
        })?;
    let factory = registration
        .encoder()
        .expect("encoder")
        .create_factory(
            EncodeRequest {
                format: &FormatId::try_new(jqf_codec_json::jsonc::FORMAT_ID).expect("format"),
                dialect: &DialectId::try_new(jqf_codec_json::jsonc::TRAILING_JQF_DIALECT_ID).expect("dialect"),
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                preservation: PreservationRequest::Report,
                options: Some(&jqf_codec_json::jsonc::JsoncEncodeOptions {
                    style: jqf_codec_json::JsonEncodeOptions::default(),
                    profile,
                }),
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

/// The leading comment facts attached by a decode, as `(node, texts)`.
fn comment_facts(
    product: &jqf_codec_core::DocumentProduct<'_>,
    resources: &mut jqf_resource::ResourceContext<'_>,
) -> Vec<(jqf_data::NodeId, Vec<String>)> {
    let document = product.document();
    let limit = jqf_data::unbounded_batch_limit();
    let mut reader = document.fact_reader(resources).expect("reader");
    let mut out = Vec::new();
    loop {
        match reader.poll_batch(limit, resources).expect("poll") {
            ReaderPoll::Batch(batch) => {
                for fact in batch.iter() {
                    let LocalOwnerRef::Node(node) = fact.owner() else {
                        continue;
                    };
                    if fact.role().as_str() != "jsonc.comment@1" {
                        continue;
                    }
                    let jqf_data::FactPayloadView::List(texts) = fact.payload() else {
                        continue;
                    };
                    let lines = texts
                        .iter()
                        .filter_map(|entry| match entry {
                            jqf_data::FactPayloadView::Text(text) => Some(String::from(text)),
                            _ => None,
                        })
                        .collect();
                    out.push((node, lines));
                }
            }
            ReaderPoll::Pending => {
                // Work credits are not replenished by the bare read loop; a
                // Pending poll must resume or it spins forever (the E2
                // jsonc-smoke hang, fixed upstream — mirrored here).
                resume(resources);
            }
            ReaderPoll::End(_) => break,
        }
    }
    out
}

pub fn run() -> Result<(), String> {
    // Registration validity.
    let registration =
        jqf_codec_json::jsonc::registration().map_err(|error| format!("invalid JSONC registration: {error:?}"))?;
    let descriptor = registration.descriptor();
    if descriptor.format().as_str() != jqf_codec_json::jsonc::FORMAT_ID {
        return Err("JSONC registration names the wrong format".into());
    }
    if descriptor.dialects().len() != 5 {
        return Err(format!(
            "JSONC registration must carry the 2 input + 3 output dialects, found {}",
            descriptor.dialects().len()
        ));
    }
    if !descriptor.extensions().contains(&"jsonc") {
        return Err("JSONC registration must declare the .jsonc extension".into());
    }

    // JSONC ⊇ JSON: every strict document decodes under BOTH dialects to
    // the same value the strict json codec produces (135 S4's first
    // stand-in — the byte-identity half is the compat corpus's job).
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
        for dialect in [
            jqf_codec_json::jsonc::TRAILING_DIALECT_ID,
            jqf_codec_json::jsonc::DEFAULT_DIALECT_ID,
        ] {
            let value = whole_value(bytes, dialect)
                .map_err(|kind| format!("strict document rejected under {dialect}: {kind:?}"))?;
            if format!("{value:?}") != format!("{strict_value:?}") {
                return Err(format!(
                    "strict document under {dialect} diverges from strict json: {value:?}"
                ));
            }
        }
    }

    // Comment grammar: line and block comments accepted, unterminated block
    // comment rejected.
    let commented_expected = strict_json_value(b"{\"a\":1}").map_err(|kind| format!("{kind:?}"))?;
    for bytes in [
        &b"{\"a\": 1 // line comment\n}"[..],
        b"{\"a\": /* block */ 1}",
        b"{\"a\": 1, /* trailing */}",
        b"// leading\n{\"a\": 1}",
        b"/* multi\nline */ {\"a\": 1}",
    ] {
        let value = whole_value(bytes, jqf_codec_json::jsonc::TRAILING_DIALECT_ID)
            .map_err(|kind| format!("comment document rejected: {kind:?}"))?;
        if format!("{value:?}") != format!("{commented_expected:?}") {
            return Err(format!("comment document diverges: {value:?}"));
        }
    }
    let unterminated = whole_value(
        b"{\"a\": 1 /* never closes}",
        jqf_codec_json::jsonc::TRAILING_DIALECT_ID,
    );
    if unterminated.is_ok() {
        return Err("an unterminated block comment must be rejected".into());
    }

    // The trailing-comma dialect difference (135 D9): accepted under
    // jsonc.trailing@1, rejected under jsonc.default@1.
    let trailing = whole_value(b"{\"a\": 1,}", jqf_codec_json::jsonc::TRAILING_DIALECT_ID);
    if trailing.is_err() {
        return Err("jsonc.trailing@1 must accept a trailing comma".into());
    }
    let default = whole_value(b"{\"a\": 1,}", jqf_codec_json::jsonc::DEFAULT_DIALECT_ID);
    if default.is_ok() {
        return Err("jsonc.default@1 must reject a trailing comma".into());
    }
    if whole_value(b"[1,]", jqf_codec_json::jsonc::TRAILING_DIALECT_ID).is_err() {
        return Err("jsonc.trailing@1 must accept a trailing comma in an array".into());
    }
    if whole_value(b"[1,]", jqf_codec_json::jsonc::DEFAULT_DIALECT_ID).is_ok() {
        return Err("jsonc.default@1 must reject a trailing comma in an array".into());
    }

    // Comment facts (135 D11): the leading comment attaches to the VALUE
    // node of the member it precedes; the document trailer attaches to the
    // root.
    {
        let mut resources = resources();
        let bytes =
            b"{\n  // compiler options\n  \"compilerOptions\": {\n    \"target\": \"ES2020\",\n  },\n  // trailer\n}\n";
        let registration = jqf_codec_json::jsonc::registration().map_err(|error| format!("{error:?}"))?;
        let dialect = DialectId::try_new(jqf_codec_json::jsonc::TRAILING_DIALECT_ID).expect("dialect");
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
            return Err("jsonc whole route".into());
        };
        let facts = comment_facts(&product, &mut resources);
        if facts.len() != 2 {
            return Err(format!(
                "expected two comment facts (member + trailer), found {facts:?}"
            ));
        }
        let (node, lines) = &facts[0];
        if lines != &vec![String::from("compiler options")] {
            return Err(format!("member comment text wrong: {lines:?}"));
        }
        if *node == product.document().root() {
            return Err("the member comment must not attach to the root".into());
        }
        if facts[1].0 != product.document().root() {
            return Err("the trailer comment must attach to the root".into());
        }

        // The encoder re-emits the comment facts (T6 round-trip) and writes
        // trailing commas under the trailing profile.
        let trailing_bytes = encode_whole(
            &product,
            &mut resources,
            jqf_codec_json::jsonc::JsoncEncodeProfile::Trailing,
        )
        .map_err(|kind| format!("{kind:?}"))?;
        let text = String::from_utf8_lossy(&trailing_bytes);
        if !text.contains("// compiler options") {
            return Err(format!("trailing profile dropped the comment: {text}"));
        }
        if !text.contains("\"ES2020\",") {
            return Err(format!("trailing profile dropped the trailing comma: {text}"));
        }
        // The default profile writes no trailing commas but keeps comments.
        let default_bytes = encode_whole(
            &product,
            &mut resources,
            jqf_codec_json::jsonc::JsoncEncodeProfile::Default,
        )
        .map_err(|kind| format!("{kind:?}"))?;
        let text = String::from_utf8_lossy(&default_bytes);
        if !text.contains("// compiler options") {
            return Err("default profile dropped the comment".into());
        }
        if text.contains("\"ES2020\",}") || text.contains("\"ES2020\",\n") {
            return Err(format!("default profile wrote a trailing comma: {text}"));
        }
    }

    Ok(())
}
