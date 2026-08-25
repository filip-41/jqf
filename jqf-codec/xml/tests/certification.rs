//! XML (XML 1.0 Fifth Edition + Namespaces) codec certification suite.
//!
//! Law rows exercised here, table-driven:
//!   table R — R-SCAN (whole-input validating scan, corrupt-late),
//!             R-MAT (materialize(root) == whole-document build),
//!             R-ENC (decode → encode located root → re-decode semantic
//!                    equal), R-ENC-B (encoder bytes == golden bytes),
//!             R-HINT (demand is a hint), R-VOC (escape/entity vocabulary);
//!   table P — P-ZERO (failed encode publishes zero bytes),
//!             P-BOUND (byte boundary law), P-CHUNK (bounded sink);
//!   table A — A-REPORT (report axes honest), A-CANCEL (typed cancel);
//!   cross-cutting — the two-kind declaration (`CompleteDocument` only) and
//!   the span/`--edit` law (XML binds spans and serves `--edit`).

use jqf_codec_core::{
    AccessGuarantees, AccessOutcome, AccessRequirement, AccessResultKind, ByteSink, CodecDemand, CodecError,
    CodecFailureKind, CodecRunContext, DecodeRequest, DemandClause, DiagnosticPolicy, EncodeItem, EncodeRequest,
    PreservationOutcome, PreservationRequest, ValidationMode, VecByteSink,
};
use jqf_data::{DialectId, FormatId, NodeHandle, Value};
use jqf_resource::{
    ContinueControl, Control, ControlOutcome, RequestAccount, ResourceContext, ResourceLimits, WorkMeter,
};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};
use std::fmt::Write as _;

static CONTROL: ContinueControl = ContinueControl;

const DECODE_DIALECT: &str = jqf_codec_xml::XML_DETERMINISTIC_DIALECT_ID;
const ENCODE_DIALECT: &str = jqf_codec_xml::XML_DETERMINISTIC_DIALECT_ID;

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
        SourceRef::new(SourceId::new(98), SourceKind::Input),
        "test.xml",
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

/// Decodes one XML document to its owned root value.
fn decode(bytes: &[u8]) -> Result<Value, CodecError> {
    let mut resources = resources();
    let (value, _product) = decode_whole(bytes, &mut resources)?;
    Ok(value)
}

/// Decodes one XML document on the whole-document route, returning the root
/// value and the authoritative product (for located-item encoding).
fn decode_whole<'s>(
    bytes: &'s [u8],
    resources: &mut ResourceContext<'_>,
) -> Result<(Value, jqf_codec_core::DocumentProduct<'s>), CodecError> {
    let registration = jqf_codec_xml::registration().expect("registration");
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
    let product = product.try_clone().expect("clone");
    let root = product.document().root_handle();
    let value = product
        .document()
        .materialize_node_with(&mut jqf_data::MaterializeWorkspace::new(), root, resources)
        .map_err(|_| jqf_codec_core::data_contract("materialize XML root"))?;
    Ok((value, product))
}

/// Encodes the located root of a decoded document under the deterministic
/// profile.
fn encode_located(
    product: &jqf_codec_core::DocumentProduct<'_>,
    node: NodeHandle,
    resources: &mut ResourceContext<'_>,
) -> Result<(Vec<u8>, jqf_codec_core::PreservationReport), CodecError> {
    let format = FormatId::try_new(jqf_codec_xml::FORMAT_ID).expect("format");
    let dialect = DialectId::try_new(ENCODE_DIALECT).expect("dialect");
    let registration = jqf_codec_xml::registration().expect("registration");
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
        .start(
            EncodeItem::Located { product, node },
            PreservationRequest::None,
            resources,
        )
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

/// Compact render: an element is an ordered array of its raw mixed-content
/// children; attributes and names are facts, never members.
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
                format!("{}", float.get())
            } else {
                format!("{number:?}")
            }
        }
        V::String(text) => format!("{text:?}"),
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
        other => format!("{other:?}"),
    }
}

/// R-SCAN + R-MAT: the golden corpus (the projection law) scans and
/// materializes to the expected semantic shape. Table-driven.
#[test]
fn law_r_scan_and_mat_golden_corpus() {
    let cases: &[(&str, &str)] = &[
        // An element with only text is a one-child array; an empty element
        // is an empty array. Attributes are facts, never members.
        ("<a>hi</a>", "[\"hi\"]"),
        ("<a></a>", "[]"),
        ("<a/>", "[]"),
        ("<a b=\"1\">hi</a>", "[\"hi\"]"),
        // Mixed content keeps ORDER: text, element, text.
        ("<a>b<x/>e</a>", "[\"b\", [], \"e\"]"),
        // Nested elements.
        ("<a><b><c>v</c></b></a>", "[[[\"v\"]]]"),
        // Comments are children; processing instructions render their target.
        ("<a>x<!--cmt-->y</a>", "[\"x\", \"cmt\", \"y\"]"),
        ("<a><?pi data?></a>", "[\"<?pi data?>\"]"),
        // CDATA is text.
        ("<a><![CDATA[raw <>&]]></a>", "[\"raw <>&\"]"),
        // Character references and the five predefined entities.
        ("<a>&lt;&amp;A&#65;</a>", "[\"<&AA\"]"),
        // Internal subset entity.
        ("<!DOCTYPE r [<!ENTITY co \"Codec\">]><r>&co;</r>", "[\"Codec\"]"),
        // Namespaces: declared prefixes resolve; expanded names are facts.
        ("<p xmlns:n=\"urn:x\"><n:e>v</n:e></p>", "[[\"v\"]]"),
        // The predeclared xml prefix is bound at parse start.
        ("<a xml:lang=\"en\">v</a>", "[\"v\"]"),
        // Document-level prolog: an XML declaration and comments are skipped.
        ("<?xml version=\"1.0\"?><a>v</a>", "[\"v\"]"),
        // Entity expansion is bounded and recursive references nest.
        (
            "<!DOCTYPE r [<!ENTITY a \"1\"><!ENTITY b \"&a;2\">]><r>&b;</r>",
            "[\"12\"]",
        ),
    ];
    for (source_text, expected) in cases {
        let value =
            decode(source_text.as_bytes()).unwrap_or_else(|error| panic!("decode failed for {source_text:?}: {error}"));
        let rendered = render(&value);
        assert_eq!(rendered, *expected, "decode mismatch for {source_text:?}");
    }
}

/// R-SCAN corrupt-late: a corrupt byte anywhere fails the scan — malformed
/// or unrepresentable documents are rejected, never accept-then-fail.
#[test]
fn law_r_scan_rejects_corrupt_mutations() {
    let rejections: &[(&[u8], CodecFailureKind)] = &[
        // Mismatched end tag.
        (b"<a><b></a>", CodecFailureKind::InvalidInput),
        // Missing end tag at EOF.
        (b"<a><b></b>", CodecFailureKind::InvalidInput),
        // Undeclared prefix in a start tag.
        (b"<a><a:bad/></a>", CodecFailureKind::InvalidInput),
        // A second root element (the whole document is ONE document).
        (b"<a/><b/>", CodecFailureKind::InvalidInput),
        // No root element at all.
        (b"", CodecFailureKind::InvalidInput),
        // A duplicated expanded attribute name.
        (
            b"<a xmlns:x=\"x\" x:b=\"1\" x:b=\"2\"/>",
            CodecFailureKind::InvalidInput,
        ),
        // Unbound entity reference.
        (
            b"<!DOCTYPE r [<!ENTITY a \"1\">]><r>&b;</r>",
            CodecFailureKind::InvalidInput,
        ),
        // Raw '<' in attribute content.
        (b"<a b=\"<\"/>", CodecFailureKind::InvalidInput),
    ];
    for (bytes, kind) in rejections {
        match decode(bytes) {
            Ok(value) => panic!("expected reject for {bytes:?}, decoded to {value:?}"),
            Err(error) => assert_eq!(error.kind(), *kind, "{bytes:?}: {error:?}"),
        }
    }
    // A truncation of a golden case is the corrupt-late mutation: the tail
    // is cut mid-document, so the scan must reject.
    match decode(b"<a>b<x/>e") {
        Ok(value) => panic!("truncated document decoded to {value:?}"),
        Err(error) => assert_eq!(error.kind(), CodecFailureKind::InvalidInput),
    }
}

/// R-ENC: decode → encode the located root (deterministic) → re-decode is
/// the same semantic value.
#[test]
fn law_r_enc_roundtrip() {
    let cases: &[&str] = &[
        "<a>hi</a>",
        "<a b=\"1\">hi</a>",
        "<a>b<x/>e</a>",
        "<a><![CDATA[raw <>&]]></a>",
        "<a>&lt;&amp;A&#65;</a>",
        "<p xmlns:n=\"urn:x\"><n:e>v</n:e></p>",
        "<a xml:lang=\"en\">v</a>",
    ];
    for source_text in cases {
        let mut resources = resources();
        let (value, product) = decode_whole(source_text.as_bytes(), &mut resources)
            .unwrap_or_else(|error| panic!("decode failed for {source_text:?}: {error}"));
        let root = product.document().root_handle();
        let (encoded, _) = encode_located(&product, root, &mut resources)
            .unwrap_or_else(|error| panic!("encode failed for {source_text:?}: {error}"));
        let redecoded = decode(&encoded).unwrap_or_else(|error| panic!("re-decode failed for {encoded:?}: {error}"));
        assert_eq!(
            render(&value),
            render(&redecoded),
            "roundtrip mismatch for {source_text:?} (bytes {encoded:?})"
        );
    }
}

/// R-ENC-B + P-BOUND: the deterministic serializer's byte law — explicit
/// start and end tags, one trailing LF, entities re-escaped.
#[test]
fn law_r_enc_b_byte_boundary() {
    // Every element gets explicit end tags; the document ends with one LF.
    let mut resources_a = resources();
    let (_value, product) = decode_whole(b"<a>b<x/>e</a>", &mut resources_a).expect("decode");
    let root = product.document().root_handle();
    let (encoded, _) = encode_located(&product, root, &mut resources_a).expect("encode");
    assert_eq!(encoded, b"<a>b<x></x>e</a>\n");

    // Character data re-escapes `&` and `<`; attribute values are
    // double-quoted with the named references.
    let mut resources_b = resources();
    let (_value, product) = decode_whole(b"<a b=\"1&amp;2\">x &amp; y</a>", &mut resources_b).expect("decode");
    let root = product.document().root_handle();
    let (encoded, _) = encode_located(&product, root, &mut resources_b).expect("encode");
    assert_eq!(encoded, b"<a b=\"1&amp;2\">x &amp; y</a>\n");
}

/// R-HINT: demand is a hint — an empty demand and the full demand decode to
/// the same value.
#[test]
fn law_r_hint_demand_is_a_hint() {
    let bytes = b"<a>b<x/>e</a>";
    let mut resources = resources();
    let (full, _) = decode_whole(bytes, &mut resources).expect("full-demand decode");
    let registration = jqf_codec_xml::registration().expect("registration");
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
    let requirement = requirement(&resources, CodecDemand::try_new(&resources));
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
    let hint = product
        .document()
        .materialize_node_with(
            &mut jqf_data::MaterializeWorkspace::new(),
            product.document().root_handle(),
            &mut resources,
        )
        .expect("materialize");
    assert_eq!(render(&full), render(&hint), "demand hint independence");
}

/// R-VOC: the escape/entity vocabulary survives decode → encode — a decoded
/// `&amp;` re-encodes as `&amp;`, never as a raw `&`.
#[test]
fn law_r_voc_escape_vocabulary_roundtrip() {
    let mut resources = resources();
    let (_value, product) = decode_whole(b"<a>x &amp; y &lt; z</a>", &mut resources).expect("decode");
    let root = product.document().root_handle();
    let (encoded, _) = encode_located(&product, root, &mut resources).expect("encode");
    assert_eq!(encoded, b"<a>x &amp; y &lt; z</a>\n");
}

/// P-ZERO: a failed encode publishes ZERO bytes to the sink.
#[test]
fn law_p_zero_failed_encode_publishes_nothing() {
    // A document with a doctype is unrepresentable in the deterministic
    // profile (the decoder carries the doctype fact; the serializer refuses).
    let mut resources = resources();
    let (_value, product) =
        decode_whole(b"<!DOCTYPE r [<!ENTITY a \"1\">]><r>&a;</r>", &mut resources).expect("decode");
    let root = product.document().root_handle();
    let format = FormatId::try_new(jqf_codec_xml::FORMAT_ID).expect("format");
    let dialect = DialectId::try_new(ENCODE_DIALECT).expect("dialect");
    let registration = jqf_codec_xml::registration().expect("registration");
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
        .start(
            EncodeItem::Located {
                product: &product,
                node: root,
            },
            PreservationRequest::None,
            &mut resources,
        )
        .expect("session");
    let mut out = Vec::new();
    {
        let mut sink = VecByteSink::new(&mut out);
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        let error = session
            .encode(&mut sink, &mut run)
            .expect_err("doctype must be refused");
        assert_eq!(
            error.kind(),
            CodecFailureKind::UnsupportedRepresentation,
            "doctype-bearing document: {error:?}"
        );
    }
    assert!(out.is_empty(), "failed encode published {} bytes", out.len());
}

/// P-CHUNK: chunked publication is bounded — a sink capped at a small chunk
/// still receives the complete golden output via the backpressure retry
/// surface.
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
    // A document large enough that the serializer offers several chunks.
    let mut body = String::from("<a>");
    for index in 0..64 {
        let _ = core::write!(body, "<i>{index}</i>");
    }
    body.push_str("</a>");
    let mut resources = resources();
    let (_value, product) = decode_whole(body.as_bytes(), &mut resources).expect("decode");
    let root = product.document().root_handle();
    let (golden, _) = encode_located(&product, root, &mut resources).expect("golden encode");
    assert!(!golden.is_empty());
    let format = FormatId::try_new(jqf_codec_xml::FORMAT_ID).expect("format");
    let dialect = DialectId::try_new(ENCODE_DIALECT).expect("dialect");
    let registration = jqf_codec_xml::registration().expect("registration");
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
        .start(
            EncodeItem::Located {
                product: &product,
                node: root,
            },
            PreservationRequest::None,
            &mut resources,
        )
        .expect("session");
    let mut out = Vec::new();
    {
        let mut sink = CappedSink {
            target: &mut out,
            cap: 5,
        };
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        session.encode(&mut sink, &mut run).expect("bounded encode completes");
    }
    assert_eq!(out, golden, "bounded-sink bytes must match the unbounded run");
}

/// A-REPORT: the `PreservationReport` is honest — an exact located roundtrip
/// reports Exact on the axes the bytes really preserve.
#[test]
fn law_a_report_axes_are_honest() {
    let mut resources = resources();
    let (_value, product) = decode_whole(b"<a b=\"1\">hi</a>", &mut resources).expect("decode");
    let root = product.document().root_handle();
    let (_, report) = encode_located(&product, root, &mut resources).expect("encode");
    assert_eq!(
        report.semantic_values(),
        PreservationOutcome::Exact,
        "the deterministic profile preserves the semantic tree exactly"
    );
    assert_eq!(
        report.tags_and_facts(),
        PreservationOutcome::Exact,
        "the element names and attributes (facts) are preserved"
    );
    // The serializer re-binds gathered namespace prefixes (a canonical
    // change), so ordering is reported Normalized even on a document with no
    // namespaces — the honest report must never claim Exact for a change it
    // made, and never claim Unrepresentable for an order the roundtrip
    // actually preserved.
    assert!(
        matches!(
            report.ordering(),
            PreservationOutcome::Exact | PreservationOutcome::Normalized
        ),
        "child order must be Exact or Normalized, got {:?}",
        report.ordering()
    );
}

/// A-CANCEL: a cancelled run stops at the next work check with a typed
/// error — never a panic, never a partial value.
#[test]
fn law_a_cancel_stops_at_the_work_check() {
    // `ResourceContext::new` itself observes control, so the cancellation
    // must arrive on a LATER check: the first check (construction) continues,
    // every check after it cancels.
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
    let mut resources = ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
            .expect("account"),
        &CANCEL,
        WorkMeter::try_new_v1(1).expect("work"),
    )
    .expect("resources");
    let bytes = b"<a><b>one</b><c>two</c><d>three</d></a>";
    let registration = jqf_codec_xml::registration().expect("registration");
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

/// Span binding: a text leaf's authored span slices the source
/// to its exact text bytes (entities preserved), and an attribute value's
/// authored span slices to its quoted bytes — so the edit lane can splice at
/// exact offsets and re-escape position-correctly (an attribute value and a
/// text node escape differently). The spans are an addressing channel, never
/// a second value: the sibling round-trip tests pin that the encoder output
/// never moves.
#[test]
fn text_and_attribute_spans_slice_to_authored_bytes() {
    let xml = r#"<root a="1" b='x &amp; y'>hi &amp; there</root>"#;
    let mut resources = resources();
    let (_, product) = decode_whole(xml.as_bytes(), &mut resources).expect("decode");
    let document = product.document();
    let segment = document.source_segment().expect("source segment");

    // The document root IS the root element node; its array view holds the
    // mixed-content children.
    let root_view = document.value_view(document.root_handle()).expect("root view");
    let element_node = root_view.node();

    // The element's own extent span: start tag through end tag.
    let extent = document
        .node_source_span(element_node)
        .expect("span lookup")
        .expect("element extent is bound");
    assert_eq!(
        &segment[extent.start() as usize..extent.end() as usize],
        xml.as_bytes(),
        "element extent must name the whole authored element"
    );

    // The one text child: its span names the AUTHORED bytes `hi &amp; there`
    // (entity references preserved), not the decoded `hi & there`.
    let text_view = root_view
        .array()
        .expect("element array")
        .expect("element array present")
        .get(0)
        .expect("text child");
    let text_span = document
        .node_source_span(text_view.node())
        .expect("span lookup")
        .expect("text leaf has an authored span");
    assert_eq!(
        &segment[text_span.start() as usize..text_span.end() as usize],
        b"hi &amp; there",
        "text span must name the authored bytes, entities preserved"
    );

    // Attribute quoted-value spans live on the attribute facts, not on
    // minted Null nodes. A second attribute is not node-id adjacent.
    let a_span = attribute_fact_span(document, element_node, "a");
    assert_eq!(
        &segment[a_span.start() as usize..a_span.end() as usize],
        b"\"1\"",
        "attribute span must include its quotes"
    );
    let b_span = attribute_fact_span(document, element_node, "b");
    assert_eq!(
        &segment[b_span.start() as usize..b_span.end() as usize],
        b"'x &amp; y'",
        "attribute span must name the authored bytes, entities and quote kept"
    );
}

/// Cross-cutting: the two-kind declaration — XML serves `CompleteDocument` and
/// never `RecordStream` — plus the span/`--edit` law (XML binds spans and
/// serves `--edit`).
#[test]
fn law_table_declarations() {
    let registration = jqf_codec_xml::registration().expect("registration");
    let operations = registration.descriptor().operations();
    assert!(operations.decode(), "XML must declare decoder construction");
    assert!(operations.encode(), "XML must declare encoder construction");
    // The span/`--edit` law: spans bind, the
    // leaf seam renders position-correctly, and the splice policy lands
    // appends before the end tag and cuts removals at their own extent. The
    // codec-level law lives in
    // `text_spans_align_through_element_children_and_the_leaf_seam_is_position_aware`
    // above; the end-to-end contract is the edit differential's xml arm.
    let mut resources = resources();
    let provider = registration
        .decoder()
        .expect("decoder")
        .create_provider(
            source(b"<a/>"),
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
            "XML must never advertise a record stream"
        );
    }
}

/// Edit seam: text-leaf spans ALIGN through element children —
/// the parse links an element child into its parent's content and must push
/// the span-alignment placeholder with it, or every text span after the
/// first element child binds to the wrong node. The
/// test pins the exact span of each of a root's mixed children, and that the
/// leaf seam renders position-correctly: an attribute addressing node's
/// span is its quoted value, a text leaf's span is its authored bytes.
#[test]
fn text_spans_align_through_element_children_and_the_leaf_seam_is_position_aware() {
    // Root content: element a, text " ", element b — the text AFTER the
    // first element child is the alignment trap (`open_element` pushed
    // the element event without the content_spans placeholder, so the space
    // lost its span and b's text span walked onto the wrong node).
    let xml = b"<r><a>1</a> <b>2</b></r>";
    let mut resources = resources();
    let (_, product) = decode_whole(xml, &mut resources).expect("decode");
    let document = product.document();
    let segment = document.source_segment().expect("source segment");
    let root = document.value_view(document.root_handle()).expect("root view");
    let children = root.array().expect("array").expect("children");
    let mut spans = Vec::new();
    for child in children.iter() {
        let span = document
            .node_source_span(child.node())
            .expect("span lookup")
            .expect("every mixed-content child binds its authored span");
        spans.push(&segment[span.start() as usize..span.end() as usize]);
    }
    assert_eq!(
        spans,
        vec![&b"<a>1</a>"[..], &b" "[..], &b"<b>2</b>"[..],],
        "the element extents and the inter-child text span must each name \
         their own authored bytes"
    );

    // The leaf seam's position grammar: the same value renders as character
    // data on a text leaf and as a quoted, attribute-escaped value when the
    // authored span is an attribute's quoted bytes.
    let xml = br#"<r a="1">t</r>"#;
    let (_, product) = decode_whole(xml, &mut resources).expect("decode");
    let document = product.document();
    let segment = document.source_segment().expect("source segment");
    let root = document.value_view(document.root_handle()).expect("root view");
    let text_child = root
        .array()
        .expect("array")
        .expect("children")
        .get(0)
        .expect("text child view");
    let value = jqf_data::Value::String(jqf_data::Shared::try_from_str("a & b").expect("string"));
    // The leaf seam renders through the encoder factory's public hook; the
    // authored bytes name the position's quote style.
    let factory = leaf_factory(&mut resources);
    let text_rendered = factory
        .render_leaf(
            document,
            text_child.node(),
            &[],
            segment,
            &value,
            Some(&segment[text_span(document, text_child.node())]),
            &mut resources,
        )
        .expect("text leaf render");
    assert_eq!(text_rendered, b"a &amp; b", "text position escapes as character data");

    // The attribute fact carries the quoted-value span; the render keeps
    // the quote pair with attribute escaping. Passing the ELEMENT node with
    // those authored bytes is the edit lane's contract after Null nodes
    // were removed.
    let attr_span = attribute_fact_span(document, root.node(), "a");
    let attr_rendered = factory
        .render_leaf(
            document,
            root.node(),
            &[],
            segment,
            &value,
            Some(&segment[attr_span.start() as usize..attr_span.end() as usize]),
            &mut resources,
        )
        .expect("attribute leaf render");
    assert_eq!(
        attr_rendered, b"\"a &amp; b\"",
        "an attribute position renders quoted and attribute-escaped"
    );
}

fn attribute_fact_span(document: &jqf_data::Document<'_>, element: jqf_data::NodeId, kind: &str) -> jqf_source::Span {
    for fact_id in document.owner_fact_ids(element) {
        let fact = document.fact(*fact_id).expect("fact");
        if fact.role().as_str() == jqf_codec_core::markup::ATTRIBUTE_FACT && fact.kind().as_str() == kind {
            return fact.source_span().expect("attribute fact binds a span");
        }
    }
    panic!("missing attribute fact {kind}");
}

/// The retained authored span bytes of one node, as a byte range.
fn text_span(document: &jqf_data::Document<'_>, node: jqf_data::NodeId) -> core::ops::Range<usize> {
    let span = document.node_source_span(node).expect("span").expect("bound");
    span.start() as usize..span.end() as usize
}

/// An XML encoder factory for the leaf-seam probe (the deterministic
/// profile; the splice hooks are profile-independent).
fn leaf_factory(resources: &mut jqf_resource::ResourceContext<'_>) -> jqf_codec_core::ErasedEncoderFactory {
    let format = jqf_data::FormatId::try_new(jqf_codec_xml::FORMAT_ID).expect("format");
    let dialect = jqf_data::DialectId::try_new(jqf_codec_xml::XML_DETERMINISTIC_DIALECT_ID).expect("dialect");
    jqf_codec_xml::registration()
        .expect("registration")
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
            resources,
        )
        .expect("factory")
}
