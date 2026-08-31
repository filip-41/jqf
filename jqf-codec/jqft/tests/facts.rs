//! Fact tests: the frozen fact names (`.@comment` + the five roles, markup `.@name`/`.@content`/`.@attrs`, the `.&`
//! attribute, tag chains by repetition, bracket keys, provenance/source) actually ATTACH on decode and round-trip
//! through the canonical encoders. Facts live on DOCUMENTS, not on owned values, so every encode here goes through the
//! LOCATED item path — decode to a product, encode the root, re-decode. All loops are bounded.

use jqf_codec_core::{
    AccessFootprint, AccessGuarantees, AccessOutcome, AccessRequirement, CodecDemand, CodecFailureKind,
    CodecRunContext, DecodeRequest, DemandClause, DiagnosticPolicy, EncodeItem, EncodeRequest, ExactPath, FactIntent,
    PreservationRequest, ValidationMode,
};
use jqf_data::{DialectId, FormatId, Value};
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

static CONTROL: ContinueControl = ContinueControl;

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
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "facts.jqft",
        bytes,
        0,
    )
}

fn registration(format: &str) -> jqf_codec_core::CodecRegistration<'static> {
    match format {
        jqf_codec_jqft::FORMAT_ID => jqf_codec_jqft::registration_jqft(),
        jqf_codec_jqft::FORMAT_ID_JQFB => jqf_codec_jqft::registration_jqfb(),
        _ => unreachable!("test formats"),
    }
    .expect("registration")
}

/// Decodes one whole document and LOCATED-encodes its root to canonical bytes — the only encode path that carries
/// attached facts. `options` are the level-composition flags (`with_source`).
#[allow(
    clippy::too_many_lines,
    reason = "one bounded decode+located-encode driver per test helper"
)]
fn located_encode(
    bytes: &[u8],
    decode_format: &str,
    encode_format: &str,
    dialect: &str,
    with_source: bool,
) -> Result<Vec<u8>, CodecFailureKind> {
    let decoder_registration = registration(decode_format);
    let mut resources = resources();
    let source = source(bytes);
    let mut provider = decoder_registration
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
    .map_err(|_| CodecFailureKind::Overflow)?
    .with_fact_intent(FactIntent::Preserve);
    let handle = provider
        .bind(&requirement)
        .map_err(|_| CodecFailureKind::RequirementMismatch)?;
    let mut session = provider.open(&handle, &mut resources).map_err(|error| error.kind())?;
    let product = {
        let mut context = CodecRunContext::new(&mut resources);
        context.set_cooperative_credits(4_096);
        let result = session.decode(&mut context).map_err(|error| error.kind())?;
        let AccessOutcome::FullDocument(product) = result.into_parts().0 else {
            return Err(CodecFailureKind::RequirementMismatch);
        };
        product
    };
    let root = product
        .document()
        .node_handle(product.document().root())
        .map_err(|_| CodecFailureKind::RequirementMismatch)?;
    let options: Option<&(dyn core::any::Any + Send + Sync)> = match encode_format {
        jqf_codec_jqft::FORMAT_ID => Some(&jqf_codec_jqft::JqftEncodeOptions { with_source }),
        jqf_codec_jqft::FORMAT_ID_JQFB => Some(&jqf_codec_jqft::JqfbEncodeOptions { with_source }),
        _ => None,
    };
    let request = EncodeRequest {
        format: &FormatId::try_new(encode_format).map_err(|_| CodecFailureKind::RequirementMismatch)?,
        dialect: &DialectId::try_new(dialect).map_err(|_| CodecFailureKind::RequirementMismatch)?,
        diagnostics: DiagnosticPolicy::ErrorsOnly,
        preservation: PreservationRequest::None,
        options,
    };
    let factory = registration(encode_format)
        .encoder()
        .expect("encoder")
        .create_factory(request, &mut resources)
        .map_err(|error| error.kind())?;
    let mut session = factory
        .start(
            EncodeItem::Located {
                product: &product,
                node: root,
            },
            PreservationRequest::None,
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

/// The canonical-form law over facts: decode -> encode -> decode -> encode must be byte-identical on the second encode.
/// The jqft grammar is deterministic, so byte-identity of the encodes is fact-identity of the documents — the strongest
/// round-trip check a test can state.
fn fact_roundtrip_stable(bytes: &[u8]) {
    let first = located_encode(
        bytes,
        jqf_codec_jqft::FORMAT_ID,
        jqf_codec_jqft::FORMAT_ID,
        jqf_codec_jqft::JQFT_CANONICAL_DIALECT_ID,
        false,
    )
    .unwrap_or_else(|k| panic!("first located encode failed {k:?} for {bytes:?}"));
    let second = located_encode(
        &first,
        jqf_codec_jqft::FORMAT_ID,
        jqf_codec_jqft::FORMAT_ID,
        jqf_codec_jqft::JQFT_CANONICAL_DIALECT_ID,
        false,
    )
    .unwrap_or_else(|k| panic!("second located encode failed {k:?}"));
    assert_eq!(
        String::from_utf8_lossy(&first),
        String::from_utf8_lossy(&second),
        "the canonical form is not stable for {bytes:?}"
    );
}

/// Decodes one document and returns the root's attached facts as sorted `role=payload` strings, so a test can assert
/// the frozen names directly.
fn root_facts(bytes: &[u8]) -> Vec<String> {
    let mut resources = resources();
    let demand = CodecDemand::try_new(&resources);
    let requirement = AccessRequirement::try_whole(
        demand,
        AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
        &resources,
    )
    .expect("requirement")
    .with_fact_intent(FactIntent::Preserve);
    root_facts_for(bytes, &requirement, &mut resources)
}

/// A compact text render of a fact payload for assertions.
fn fact_payload_text(payload: &jqf_data::FactPayloadView<'_>) -> String {
    match payload {
        jqf_data::FactPayloadView::Text(text) => format!("text:{text}"),
        jqf_data::FactPayloadView::Map(map) => {
            let entries: Vec<String> = map
                .iter()
                .map(|(key, value)| format!("{key}:{}", fact_payload_text(&value)))
                .collect();
            format!("map[{}]", entries.join(" "))
        }
        jqf_data::FactPayloadView::List(list) => {
            let items: Vec<String> = list.iter().map(|item| fact_payload_text(&item)).collect();
            format!("list[{}]", items.join(" "))
        }
        jqf_data::FactPayloadView::Null => "null".into(),
        jqf_data::FactPayloadView::Bool(value) => format!("bool:{value}"),
        jqf_data::FactPayloadView::Integer(text) => format!("int:{text}"),
        jqf_data::FactPayloadView::Decimal { coefficient, scale } => {
            format!("dec:{coefficient}e{scale}")
        }
        jqf_data::FactPayloadView::Bytes(bytes) => format!("bytes:{}", bytes.len()),
        jqf_data::FactPayloadView::OpaqueBytes(bytes) => format!("opaque:{}", bytes.len()),
    }
}

/// The markup node facts attach and round-trip: `.@name`, `.@content`, `.@attrs`, the `.&` attributes, and the
/// array-of-children model.
#[test]
fn markup_facts_attach_and_round_trip() {
    // Name + content + nested markup (the `.@content` concatenation law).
    let src = b"%jqft 1\n<p \"text \" <b \"bold\"> \" more\">\n";
    fact_roundtrip_stable(src);
    // The canonical spelling is the single-line angle form.
    let canonical = located_encode(
        src,
        jqf_codec_jqft::FORMAT_ID,
        jqf_codec_jqft::FORMAT_ID,
        jqf_codec_jqft::JQFT_CANONICAL_DIALECT_ID,
        false,
    )
    .expect("encode");
    assert_eq!(
        String::from_utf8_lossy(&canonical),
        "%jqft 1\n<p \"text \" <b \"bold\"> \" more\">",
        "the canonical markup spelling is pinned"
    );
    // The root's `.@name` fact carries the expanded name.
    let root = root_facts(src);
    assert!(
        root.iter().any(|fact| fact == "jqft.name@1=text:p"),
        "the root must carry jqft.name@1=p, got {root:?}"
    );
}

/// `.&` attributes in the angle form attach as per-attribute facts and round-trip, including the quoted bracket form.
#[test]
fn attributes_attach_and_round_trip() {
    let src = b"%jqft 1\n<a &href=\"https://x\" &[\"aria-label\"]=\"v\" \"text\">\n";
    fact_roundtrip_stable(src);
    let canonical = located_encode(
        src,
        jqf_codec_jqft::FORMAT_ID,
        jqf_codec_jqft::FORMAT_ID,
        jqf_codec_jqft::JQFT_CANONICAL_DIALECT_ID,
        false,
    )
    .expect("encode");
    // The quoted attribute name re-spells in its bare form where the identifier grammar permits; `aria-label` stays
    // bare, so the canonical form has both attributes bare.
    let text = String::from_utf8_lossy(&canonical);
    assert!(text.contains("&href=\"https://x\""), "{text}");
    assert!(text.contains("&aria-label=\"v\""), "{text}");
    let root = root_facts(src);
    assert!(
        root.iter().any(|fact| fact == "attribute=text:https://x"),
        "the attribute fact must carry href, got {root:?}"
    );
    assert!(
        root.iter()
            .any(|fact| fact == "jqft.attrs@1=map[href:text:https://x aria-label:text:v]"),
        "the complete attribute map must attach, got {root:?}"
    );
}

/// `.@comment` with the §3.15 roles the `#` grammar produces (leading / inline / detached) attaches and round-trips
/// without role drift.
#[test]
fn comment_roles_attach_and_round_trip() {
    // Leading on the root value, inline on an entry value, detached on the root trailer — one document carrying all
    // three roles.
    let src = b"%jqft 1\n# doc intro\n{\n  # entry lead\n  a: 1, # inline note\n  b: 2,\n}\n# trailer note\n";
    fact_roundtrip_stable(src);
    let canonical = located_encode(
        src,
        jqf_codec_jqft::FORMAT_ID,
        jqf_codec_jqft::FORMAT_ID,
        jqf_codec_jqft::JQFT_CANONICAL_DIALECT_ID,
        false,
    )
    .expect("encode");
    let text = String::from_utf8_lossy(&canonical);
    assert!(text.contains("# doc intro"), "{text}");
    assert!(text.contains("# entry lead"), "{text}");
    assert!(text.contains("a: 1, # inline note"), "{text}");
    assert!(text.contains("# trailer note"), "{text}");
    // The detached comment rides the document trailer (after the root).
    assert!(text.trim_end().ends_with("}\n# trailer note"), "{text}");
}

/// The flat comment siblings are projections of the role-keyed map — `jqft.comment@1` answers the LEADING list only
/// (plain text, agreeing with the other codecs' `.@comment`), `jqft.comment_inline@1` the inline list (absent when
/// empty), and `jqft.comment_foot@1` an EMPTY list (the `#` grammar has no foot position). The map keeps
/// trailing/inner/detached and the `{text, style}` payload the flat lists cannot express, and the two cannot disagree
/// because the siblings are built from the map in one pass.
#[test]
fn flat_comment_siblings_agree_with_the_map() {
    let src = b"%jqft 1\n# doc intro\n{\n  # entry lead\n  a: 1, # inline note\n  b: 2,\n}\n# trailer note\n";
    let facts = root_facts(src);
    // The leading sibling is the map's leading texts, plain (no style).
    let leading = facts
        .iter()
        .find(|fact| fact.starts_with("jqft.comment@1="))
        .expect("leading sibling");
    assert_eq!(leading, "jqft.comment@1=list[text:doc intro]");
    // The root's inline list is empty, so no inline sibling attaches; the foot sibling is attached EMPTY (the surface
    // is complete).
    assert!(
        !facts.iter().any(|fact| fact.starts_with("jqft.comment_inline@1=")),
        "{facts:?}"
    );
    assert!(
        facts.iter().any(|fact| fact == "jqft.comment_foot@1=list[]"),
        "{facts:?}"
    );
    // The map keeps every role and the {text, style} payload.
    let map = facts
        .iter()
        .find(|fact| fact.starts_with("jqft.comment_map@1="))
        .expect("comment map");
    assert!(
        map.contains("leading:list[map[text:text:doc intro style:text:line]]"),
        "{map}"
    );
    assert!(map.contains("inline:list[] trailing:list[] inner:list[]"), "{map}");
    assert!(
        map.contains("detached:list[map[text:text:trailer note style:text:line]]"),
        "{map}"
    );
}

/// A leading comment before a markup node (or inside its body) attaches to the markup node — the element, not its first
/// text run — and round-trips.
#[test]
fn markup_leading_comments_round_trip() {
    let src = b"%jqft 1\n# c\n<a \"x\">\n";
    fact_roundtrip_stable(src);
    let canonical = located_encode(
        src,
        jqf_codec_jqft::FORMAT_ID,
        jqf_codec_jqft::FORMAT_ID,
        jqf_codec_jqft::JQFT_CANONICAL_DIALECT_ID,
        false,
    )
    .expect("encode");
    assert_eq!(
        String::from_utf8_lossy(&canonical),
        "%jqft 1\n# c\n<a \"x\">",
        "the leading comment stays with the element"
    );
    // A comment INSIDE the markup body (own-line before the content) also attaches to the element and normalizes to the
    // position before it.
    let inside = b"%jqft 1\n<p # c\n \"a\">\n";
    fact_roundtrip_stable(inside);
}

/// An inline comment on a markup STRING child has no spelling position inside the single-line markup form (a ` # c`
/// would swallow the rest of the line): the run refuses with a typed error.
#[test]
fn unspellable_inline_comment_on_markup_child_refuses() {
    let src = b"%jqft 1\n<p \"a\" # note\n \"b\">\n";
    let outcome = located_encode(
        src,
        jqf_codec_jqft::FORMAT_ID,
        jqf_codec_jqft::FORMAT_ID,
        jqf_codec_jqft::JQFT_CANONICAL_DIALECT_ID,
        false,
    );
    assert!(
        matches!(outcome, Err(CodecFailureKind::UnsupportedRepresentation)),
        "an unspellable child comment must refuse, got {outcome:?}"
    );
}

/// Tag chains by repetition survive the located encode: the outermost tag is emitted first and re-decoded
/// outermost-first (the CBOR storage law).
#[test]
fn tag_chains_round_trip_outermost_first() {
    let src = b"%jqft 1\n{chain: @tag(\"outer\") @tag(\"inner\") [1, 2]}\n";
    fact_roundtrip_stable(src);
    let canonical = located_encode(
        src,
        jqf_codec_jqft::FORMAT_ID,
        jqf_codec_jqft::FORMAT_ID,
        jqf_codec_jqft::JQFT_CANONICAL_DIALECT_ID,
        false,
    )
    .expect("encode");
    let text = String::from_utf8_lossy(&canonical);
    assert!(
        text.contains("@tag(\"outer\") @tag(\"inner\") ["),
        "the chain must re-emit outermost first: {text}"
    );
}

/// The bracket-key form: a string key in parentheses is an ordinary key and round-trips; a non-string key refuses with
/// the projection narrowing's law.
#[test]
fn bracket_keys_round_trip_and_non_string_refuses() {
    fact_roundtrip_stable(b"%jqft 1\n{(\"a\"): 1, (b): 2}\n");
    let outcome = located_encode(
        b"%jqft 1\n{(1): \"x\"}\n",
        jqf_codec_jqft::FORMAT_ID,
        jqf_codec_jqft::FORMAT_ID,
        jqf_codec_jqft::JQFT_CANONICAL_DIALECT_ID,
        false,
    );
    assert!(outcome.is_err(), "a non-string bracket key is not projectable");
}

/// `with_source` echoes the retained origin document byte-identically, and a run without the retention (an
/// owned/computed value) is a typed error — never a silently thinner file.
#[test]
fn with_source_echoes_and_refuses_without_retention() {
    let src = b"%jqft 1\n# a comment\n{name: \"ada\", id: @tag(\"uuid\") \"0198\"}\n";
    // The retained source echoes byte-identically, comment and header intact.
    let echoed = located_encode(
        src,
        jqf_codec_jqft::FORMAT_ID,
        jqf_codec_jqft::FORMAT_ID,
        jqf_codec_jqft::JQFT_CANONICAL_DIALECT_ID,
        true,
    )
    .expect("echo");
    assert_eq!(echoed, src, "--with-source must echo the origin document");
    // A computed value has no retained source: clean typed error.
    let registration = registration(jqf_codec_jqft::FORMAT_ID);
    let mut resources = resources();
    let request = EncodeRequest {
        format: &FormatId::try_new(jqf_codec_jqft::FORMAT_ID).expect("format"),
        dialect: &DialectId::try_new(jqf_codec_jqft::JQFT_CANONICAL_DIALECT_ID).expect("dialect"),
        diagnostics: DiagnosticPolicy::ErrorsOnly,
        preservation: PreservationRequest::None,
        options: Some(&jqf_codec_jqft::JqftEncodeOptions { with_source: true } as &(dyn core::any::Any + Send + Sync)),
    };
    let factory = registration
        .encoder()
        .expect("encoder")
        .create_factory(request, &mut resources)
        .expect("factory");
    let mut session = factory
        .start(
            EncodeItem::owned(&Value::Null),
            PreservationRequest::None,
            &mut resources,
        )
        .expect("start");
    let mut discarded = Vec::new();
    {
        let mut sink = jqf_codec_core::VecByteSink::new(&mut discarded);
        let mut context = CodecRunContext::new(&mut resources);
        context.set_cooperative_credits(4_096);
        let error = session
            .encode(&mut sink, &mut context)
            .expect_err("an owned value must refuse the source level");
        assert_eq!(error.kind(), CodecFailureKind::UnsupportedRepresentation);
    }
}

/// The family conversion matrix over a FACT-BEARING document: jqft text -> jqfb -> jqft text preserves the
/// markup/comment facts (the FACT chunk carries them; the jqft text encoder re-spells them).
#[test]
fn jqfb_carries_facts_and_round_trips_through_text() {
    let src = b"%jqft 1\n# lead\n<a &href=\"x\" \"text\" <b \"bold\">>\n";
    // text -> jqfb (with the retained source embedded).
    let image = located_encode(
        src,
        jqf_codec_jqft::FORMAT_ID,
        jqf_codec_jqft::FORMAT_ID_JQFB,
        jqf_codec_jqft::JQFB_CANONICAL_DIALECT_ID,
        true,
    )
    .expect("jqfb encode");
    // jqfb -> jqft text: the facts must re-spell, and the second text encode must be byte-identical to the first (the
    // canonical bijection).
    let text_out = located_encode(
        &image,
        jqf_codec_jqft::FORMAT_ID_JQFB,
        jqf_codec_jqft::FORMAT_ID,
        jqf_codec_jqft::JQFT_CANONICAL_DIALECT_ID,
        false,
    )
    .expect("jqfb -> text");
    let text_out2 = located_encode(
        &text_out,
        jqf_codec_jqft::FORMAT_ID,
        jqf_codec_jqft::FORMAT_ID,
        jqf_codec_jqft::JQFT_CANONICAL_DIALECT_ID,
        false,
    )
    .expect("text re-encode");
    assert_eq!(text_out, text_out2, "jqfb -> text is canonical");
    let text = String::from_utf8_lossy(&text_out);
    assert!(text.contains("<a &href=\"x\""), "{text}");
    assert!(text.contains("# lead"), "{text}");
    assert!(text.contains("<b \"bold\">"), "{text}");
}

/// A jqfb image decoded from a source carries the provenance and source facts on the root (`.@provenance` +
/// `jqfb.source@1`), per the frozen set.
#[test]
fn jqfb_provenance_and_source_facts_attach() {
    let src = b"%jqft 1\n{a: 1}\n";
    let image = located_encode(
        src,
        jqf_codec_jqft::FORMAT_ID,
        jqf_codec_jqft::FORMAT_ID_JQFB,
        jqf_codec_jqft::JQFB_CANONICAL_DIALECT_ID,
        true,
    )
    .expect("jqfb encode");
    // Decode the image and read the root facts directly.
    let decoder_registration = registration(jqf_codec_jqft::FORMAT_ID_JQFB);
    let mut resources = resources();
    let source = source(&image);
    let mut provider = decoder_registration
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
        .expect("provider");
    let demand = CodecDemand::try_new(&resources);
    let requirement = AccessRequirement::try_whole(
        demand,
        AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
        &resources,
    )
    .expect("requirement")
    .with_fact_intent(FactIntent::Preserve);
    let handle = provider.bind(&requirement).expect("bind");
    let mut session = provider.open(&handle, &mut resources).expect("open");
    let product = {
        let mut context = CodecRunContext::new(&mut resources);
        context.set_cooperative_credits(4_096);
        let result = session.decode(&mut context).expect("decode");
        let AccessOutcome::FullDocument(product) = result.into_parts().0 else {
            panic!("not a full document");
        };
        product
    };
    let document = product.document();
    let mut roles: Vec<String> = Vec::new();
    let mut reader = document.fact_reader(&mut resources).expect("fact reader");
    let limit = jqf_data::unbounded_batch_limit();
    loop {
        let poll = reader.poll_batch(limit, &mut resources).expect("poll");
        match poll {
            jqf_data::ReaderPoll::Batch(batch) => {
                for fact in batch.iter() {
                    roles.push(String::from(fact.role().as_str()));
                }
            }
            jqf_data::ReaderPoll::Pending => {
                resources.try_begin_next_cooperative_entry(4_096).expect("resume");
            }
            jqf_data::ReaderPoll::End(_) => break,
        }
    }
    assert!(
        roles.iter().any(|role| role == "jqft.provenance@1"),
        "the PROV chunk must surface as the provenance fact, got {roles:?}"
    );
    assert!(
        roles.iter().any(|role| role == "jqfb.source@1"),
        "the SOUR chunk must surface as the source fact, got {roles:?}"
    );
}

/// The `---`-separated document stream: each document's facts survive the canonical re-encode (multi-doc jqft is the
/// adjacent-value input model; the whole-document route serves the FIRST document of the stream).
#[test]
fn stream_round_trips_facts_per_document() {
    let src = b"%jqft 1\n# first doc\n{a: <p \"x\">}\n---\n# second doc\n{b: 2}\n";
    fact_roundtrip_stable(src);
    let canonical = located_encode(
        src,
        jqf_codec_jqft::FORMAT_ID,
        jqf_codec_jqft::FORMAT_ID,
        jqf_codec_jqft::JQFT_CANONICAL_DIALECT_ID,
        false,
    )
    .expect("encode");
    let text = String::from_utf8_lossy(&canonical);
    // The first document's comment (leading on the `<p>` value) and markup survive; the encode is a single-document
    // canonical form.
    assert!(text.contains("# first doc"), "{text}");
    assert!(text.contains("a: <p \"x\">"), "{text}");
    assert!(!text.contains("---"), "the whole route serves one document: {text}");
}

/// Located-encodes one jqfb scoped product as jqft text, so markup facts stay visible (JSON output masks them).
fn scoped_jqfb_as_jqft(image: &[u8], member: &str) -> Result<Vec<u8>, CodecFailureKind> {
    let decoder_registration = registration(jqf_codec_jqft::FORMAT_ID_JQFB);
    let mut resources = resources();
    let source = source(image);
    let mut provider = decoder_registration
        .decoder()
        .expect("decoder")
        .create_provider(
            source,
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(jqf_codec_jqft::JQFB_CANONICAL_DIALECT_ID).expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            &mut resources,
        )
        .map_err(|error| error.kind())?;
    let mut path = ExactPath::try_new(&resources);
    path.try_push_semantic_member(member, &resources)
        .map_err(|_| CodecFailureKind::Overflow)?;
    let footprint = AccessFootprint::try_exact(path, &resources);
    let demand = CodecDemand::try_new(&resources);
    let requirement = AccessRequirement::try_exact(
        footprint,
        demand,
        AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
        &resources,
    )
    .map_err(|_| CodecFailureKind::Overflow)?
    .with_fact_intent(FactIntent::Preserve);
    let handle = provider
        .bind(&requirement)
        .map_err(|_| CodecFailureKind::RequirementMismatch)?;
    let mut session = provider.open(&handle, &mut resources).map_err(|error| error.kind())?;
    let result = {
        let mut context = CodecRunContext::new(&mut resources);
        context.set_cooperative_credits(4_096);
        session.decode(&mut context).map_err(|error| error.kind())?
    };
    let AccessOutcome::Located(outcome) = result.into_parts().0 else {
        return Err(CodecFailureKind::RequirementMismatch);
    };
    let product = outcome.product();
    let root = product
        .document()
        .node_handle(product.document().root())
        .map_err(|_| CodecFailureKind::RequirementMismatch)?;
    let request = EncodeRequest {
        format: &FormatId::try_new(jqf_codec_jqft::FORMAT_ID).map_err(|_| CodecFailureKind::RequirementMismatch)?,
        dialect: &DialectId::try_new(jqf_codec_jqft::JQFT_CANONICAL_DIALECT_ID)
            .map_err(|_| CodecFailureKind::RequirementMismatch)?,
        diagnostics: DiagnosticPolicy::ErrorsOnly,
        preservation: PreservationRequest::None,
        options: Some(&jqf_codec_jqft::JqftEncodeOptions { with_source: false }),
    };
    let factory = registration(jqf_codec_jqft::FORMAT_ID)
        .encoder()
        .expect("encoder")
        .create_factory(request, &mut resources)
        .map_err(|error| error.kind())?;
    let mut session = factory
        .start(
            EncodeItem::Located { product, node: root },
            PreservationRequest::None,
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

/// A scoped locate of a fact-bearing subtree keeps the markup facts the floor keeps: `.doc` under jqft output is the
/// article, not the stripped `[["Title"]]` projection.
#[test]
fn scoped_jqfb_preserves_markup_facts_under_jqft_output() {
    let src = b"%jqft 1\n{doc: <article &lang=\"en\" <h1 \"Title\">>, n: 1}\n";
    let image = located_encode(
        src,
        jqf_codec_jqft::FORMAT_ID,
        jqf_codec_jqft::FORMAT_ID_JQFB,
        jqf_codec_jqft::JQFB_CANONICAL_DIALECT_ID,
        false,
    )
    .expect("jqfb encode");
    let floor = located_encode(
        &image,
        jqf_codec_jqft::FORMAT_ID_JQFB,
        jqf_codec_jqft::FORMAT_ID,
        jqf_codec_jqft::JQFT_CANONICAL_DIALECT_ID,
        false,
    )
    .expect("floor jqft");
    let scoped = scoped_jqfb_as_jqft(&image, "doc").expect("scoped .doc");
    let floor_text = String::from_utf8_lossy(&floor);
    let scoped_text = String::from_utf8_lossy(&scoped);
    assert!(
        floor_text.contains("<article &lang=\"en\""),
        "floor must keep markup: {floor_text}"
    );
    assert!(
        scoped_text.contains("<article &lang=\"en\""),
        "scoped .doc must keep markup the floor keeps: {scoped_text}"
    );
    assert!(
        scoped_text.contains("<h1 \"Title\">"),
        "scoped .doc must keep the child: {scoped_text}"
    );
}

fn root_facts_for(bytes: &[u8], requirement: &AccessRequirement, resources: &mut ResourceContext<'_>) -> Vec<String> {
    let decoder_registration = registration(jqf_codec_jqft::FORMAT_ID);
    let source = source(bytes);
    let mut provider = decoder_registration
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
            resources,
        )
        .expect("provider");
    let handle = provider.bind(requirement).expect("bind");
    let mut session = provider.open(&handle, resources).expect("open");
    let product = {
        let mut context = CodecRunContext::new(resources);
        context.set_cooperative_credits(4_096);
        let result = session.decode(&mut context).expect("decode");
        let AccessOutcome::FullDocument(product) = result.into_parts().0 else {
            panic!("not a full document");
        };
        product
    };
    let document = product.document();
    let mut out: Vec<String> = Vec::new();
    let root = document.root();
    for fact_id in document.owner_fact_ids(root) {
        let fact = document.fact(*fact_id).expect("fact");
        out.push(format!(
            "{}={}",
            fact.role().as_str(),
            fact_payload_text(&fact.payload())
        ));
    }
    out.sort();
    out
}

#[test]
fn identity_demand_does_not_attach_comment_facts() {
    let mut resources = resources();
    let requirement = AccessRequirement::try_whole(
        CodecDemand::try_new(&resources),
        AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
        &resources,
    )
    .expect("requirement");
    let facts = root_facts_for(b"%jqft 1\n# lead\n{a: 1}\n", &requirement, &mut resources);
    assert!(
        !facts.iter().any(|fact| fact.starts_with("jqft.comment")),
        "identity must skip comment facts, got {facts:?}"
    );
}

#[test]
fn comment_clause_attaches_comment_facts() {
    let mut resources = resources();
    let mut demand = CodecDemand::try_new(&resources);
    let kind = jqf_data::FactKindId::try_new("comment").expect("kind");
    let role = jqf_data::FactRoleId::try_new("comment").expect("role");
    demand
        .try_insert(&DemandClause::AttachedFact { kind, role })
        .expect("insert");
    let requirement = AccessRequirement::try_whole(
        demand,
        AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
        &resources,
    )
    .expect("requirement");
    let facts = root_facts_for(b"%jqft 1\n# lead\n{a: 1}\n", &requirement, &mut resources);
    assert!(
        facts.iter().any(|fact| fact.starts_with("jqft.comment@1=")),
        "comment clause must attach, got {facts:?}"
    );
}

#[test]
fn preserve_attaches_comment_facts() {
    let mut resources = resources();
    let requirement = AccessRequirement::try_whole(
        CodecDemand::try_new(&resources),
        AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
        &resources,
    )
    .expect("requirement")
    .with_fact_intent(FactIntent::Preserve);
    let facts = root_facts_for(b"%jqft 1\n# lead\n{a: 1}\n", &requirement, &mut resources);
    assert!(
        facts.iter().any(|fact| fact.starts_with("jqft.comment@1=")),
        "Preserve must attach comment facts, got {facts:?}"
    );
}

#[test]
fn identity_demand_does_not_attach_markup_facts() {
    let mut resources = resources();
    let requirement = AccessRequirement::try_whole(
        CodecDemand::try_new(&resources),
        AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
        &resources,
    )
    .expect("requirement");
    let facts = root_facts_for(b"%jqft 1\n<p \"text\">\n", &requirement, &mut resources);
    assert!(
        !facts.iter().any(|fact| fact.starts_with("jqft.name")),
        "identity must skip markup facts, got {facts:?}"
    );
}

#[test]
fn preserve_attaches_markup_facts() {
    let mut resources = resources();
    let requirement = AccessRequirement::try_whole(
        CodecDemand::try_new(&resources),
        AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
        &resources,
    )
    .expect("requirement")
    .with_fact_intent(FactIntent::Preserve);
    let facts = root_facts_for(b"%jqft 1\n<p \"text\">\n", &requirement, &mut resources);
    assert!(
        facts.iter().any(|fact| fact.starts_with("jqft.name@1=")),
        "Preserve must attach markup name facts, got {facts:?}"
    );
}

#[test]
fn identity_refuses_a_corrupt_fact_chunk_with_a_valid_digest() {
    let image = located_encode(
        b"%jqft 1\n# lead\n{a: 1}\n",
        jqf_codec_jqft::FORMAT_ID,
        jqf_codec_jqft::FORMAT_ID_JQFB,
        jqf_codec_jqft::JQFB_CANONICAL_DIALECT_ID,
        false,
    )
    .expect("jqfb encode");
    let bytes = image.as_slice();
    let footer_start = usize::try_from(u64::from_le_bytes(bytes[bytes.len() - 8..].try_into().unwrap())).unwrap();
    let footer = &bytes[bytes.len() - footer_start..];
    let count = usize::try_from(u64::from_le_bytes(footer[..8].try_into().unwrap())).unwrap();
    let mut fact_offset = None;
    let mut fact_len = 0usize;
    let mut digest_slot = 0usize;
    for index in 0..count {
        let entry = &footer[8 + index * 52..8 + (index + 1) * 52];
        let chunk_type = u32::from_le_bytes(entry[..4].try_into().unwrap());
        if chunk_type == 0x0000_0004 {
            fact_offset = Some(usize::try_from(u64::from_le_bytes(entry[4..12].try_into().unwrap())).unwrap());
            fact_len = usize::try_from(u64::from_le_bytes(entry[12..20].try_into().unwrap())).unwrap();
            digest_slot = bytes.len() - footer_start + 8 + index * 52 + 20;
            break;
        }
    }
    let fact_offset = fact_offset.expect("FACT chunk");
    assert!(fact_len > 0, "FACT chunk must carry records");
    let mut mutated = image.clone();
    let at = fact_offset + fact_len - 1;
    mutated[at] ^= 0xff;
    let digest = blake3::hash(&mutated[fact_offset..fact_offset + fact_len]);
    mutated[digest_slot..digest_slot + 32].copy_from_slice(digest.as_bytes());
    let error = located_encode(
        &mutated,
        jqf_codec_jqft::FORMAT_ID_JQFB,
        jqf_codec_jqft::FORMAT_ID,
        jqf_codec_jqft::JQFT_CANONICAL_DIALECT_ID,
        false,
    )
    .expect_err("identity must refuse a corrupt FACT chunk");
    assert_eq!(error, CodecFailureKind::InvalidInput);
}
