//! HTML (WHATWG-recovered documents) codec certification suite.
//!
//! Law rows exercised here, table-driven: table R — R-SCAN (whole-input validating scan; HTML's lenient recovery is a
//! declared deviation — mutations recover, never reject and never accept-then-fail), R-MAT (materialize(root) ==
//! whole-document build), R-ENC (decode → encode located root → re-decode semantic equal), R-ENC-B (encoder bytes ==
//! golden bytes), R-HINT (demand is a hint), R-VOC (entity/encoding vocabulary); table P — P-ZERO (failed encode
//! publishes zero bytes), P-BOUND (byte boundary law), P-CHUNK (bounded sink); table A — A-REPORT (report axes honest),
//! A-CANCEL (typed cancel); cross-cutting — the two-kind declaration (`CompleteDocument` only) and the span/`--edit`
//! law as a declared absence (HTML has no edit splice).

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

const DECODE_DIALECT: &str = jqf_codec_html::HTML_DOCUMENT_DIALECT_ID;
const ENCODE_DIALECT: &str = jqf_codec_html::HTML_DOCUMENT_SERIALIZE_DIALECT_ID;

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
        SourceRef::new(SourceId::new(99), SourceKind::Input),
        "test.html",
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

/// Decodes one HTML document to its owned root value.
fn decode(bytes: &[u8]) -> Result<Value, CodecError> {
    let mut resources = resources();
    let (value, _product) = decode_whole(bytes, &mut resources)?;
    Ok(value)
}

fn decode_whole<'s>(
    bytes: &'s [u8],
    resources: &mut ResourceContext<'_>,
) -> Result<(Value, jqf_codec_core::DocumentProduct<'s>), CodecError> {
    let registration = jqf_codec_html::registration().expect("registration");
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
        .map_err(|_| {
            CodecError::new(CodecFailureKind::InternalContractViolation {
                contract: "materialize HTML root",
            })
        })?;
    Ok((value, product))
}

/// Encodes the located root of a decoded document under the document-serialize profile.
fn encode_located(
    product: &jqf_codec_core::DocumentProduct<'_>,
    node: NodeHandle,
    resources: &mut ResourceContext<'_>,
) -> Result<(Vec<u8>, jqf_codec_core::PreservationReport), CodecError> {
    let format = FormatId::try_new(jqf_codec_html::FORMAT_ID).expect("format");
    let dialect = DialectId::try_new(ENCODE_DIALECT).expect("dialect");
    let registration = jqf_codec_html::registration().expect("registration");
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

/// Compact render: the recovered shape — an element is an ordered array of its children; comments are facts, never
/// items.
fn render(value: &Value) -> String {
    fn push(value: &Value, out: &mut String) {
        match value {
            Value::String(text) => {
                out.push('"');
                out.push_str(text);
                out.push('"');
            }
            Value::Array(items) => {
                out.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index != 0 {
                        out.push(' ');
                    }
                    push(item, out);
                }
                out.push(']');
            }
            Value::Object(members) => {
                out.push('{');
                for (index, entry) in members.iter().enumerate() {
                    if index != 0 {
                        out.push(' ');
                    }
                    out.push_str(entry.key());
                    out.push(':');
                    push(entry.value(), out);
                }
                out.push('}');
            }
            other => {
                let _ = core::write!(out, "{other:?}");
            }
        }
    }
    let mut out = String::new();
    push(value, &mut out);
    out
}

/// R-SCAN: the recovered child-array shape. Comments are facts, never items, so they do not appear in `render`.
#[test]
fn law_r_scan_and_mat_golden_corpus() {
    // A full document: doctype, head with a title, body with a paragraph and a comment. The recovered shape: html =
    // [head [title ["Hi"]], body [p ["one" [] "two"]]] — the `<br>` is an empty element array, comments are facts,
    // never items.
    let value = decode(
        b"<!DOCTYPE html><html><head><title>Hi</title></head><body><!-- lead --><p class=\"a\">one<br>two</p></body></html>",
    )
    .expect("decode");
    let expected = r#"[[["Hi"]] [["one" [] "two"]]]"#;
    assert_eq!(render(&value), expected);
    // The comment law: a leading comment is an attached fact of the body element, never a child item.
    let comments = decode(b"<body><!-- lead --><p>x</p></body>").expect("decode");
    assert_eq!(render(&comments), r#"[[] [["x"]]]"#);
    // The encoding determination: a windows-1252 byte decodes to U+00E9.
    let legacy = decode(b"<p>caf\xe9</p>").expect("decode");
    assert_eq!(render(&legacy), "[[] [[\"caf\u{e9}\"]]]");
    // Entity recovery: named and numeric character references are text.
    let entities = decode(b"<p>&amp;&lt;&#65;</p>").expect("decode");
    assert_eq!(render(&entities), "[[] [[\"&<A\"]]]");
}

/// R-SCAN corrupt-late: HTML is a LENIENT parser — the mutation matrix recovers rather than rejects (a declared
/// deviation from the strict-codec rows; the conformance is the recovery oracle). The half of the law that still binds:
/// mutations never panic and never accept-then-fail-at-materialize — a recovered document always materializes.
#[test]
fn law_r_scan_mutations_recover_never_panic() {
    let mutations: &[&[u8]] = &[
        // Truncation mid-document: the tree-construction algorithm recovers.
        b"<p>unterminated",
        b"<p>a<div>b</p>", // mis-nested
        b"<p>caf\xe9</p>", // non-UTF-8 byte: windows-1252 recovery
        b"<p>a\x00b</p>",  // NUL: U+FFFD replacement
        b"",
        b"<!--x-->", // comment-only input still recovers
    ];
    for bytes in mutations {
        let value = decode(bytes).unwrap_or_else(|error| panic!("lenient decode failed for {bytes:?}: {error}"));
        // Materializing the recovered tree must never fail either.
        let _ = render(&value);
    }
}

/// After-head start tags reprocess "in head" by pushing the existing head element back onto the stack of open elements;
/// the paired cleanup removes it again. Both halves must run through the bucket-maintaining helpers or the head bucket
/// underflows on the cleanup — common real-world markup (`</head><meta …>`) must decode, not panic.
#[test]
fn law_r_scan_after_head_head_tags_keep_open_filter_balanced() {
    let cases: &[&[u8]] = &[
        b"</head><meta>",
        b"<html><head></head><meta charset=\"utf-8\"><body><p>x</p></body></html>",
        b"</head><base><link><title>t</title><template></template><body><p>x</p>",
    ];
    for bytes in cases {
        let value = decode(bytes).unwrap_or_else(|error| panic!("decode failed for {bytes:?}: {error}"));
        let _ = render(&value);
    }
}

/// R-ENC: decode → encode the located root (document-serialize) → re-decode is the same semantic value.
#[test]
fn law_r_enc_roundtrip() {
    let cases: &[&str] = &[
        "<body><p>one<br>two</p></body>",
        "<p>caf\u{e9}</p>",
        "<p>&amp;&lt;&#65;</p>",
        "<body><!--a-->x<!--b--></body>",
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

/// The doctype law: a decoded DOCTYPE is a fact the document-serialize byte law cannot write (it serializes the
/// document element only), so a doctype-bearing document refuses with `UnsupportedRepresentation` instead of silently
/// dropping the fact — dropped output would re-decode into quirks mode. Nothing is published for the refused document.
#[test]
fn law_r_enc_doctype_document_is_refused() {
    let mut resources = resources();
    let (_, product) = decode_whole(
        b"<!DOCTYPE html><html><head><title>Hi</title></head><body><p>x</p></body></html>",
        &mut resources,
    )
    .expect("decode");
    let root = product.document().root_handle();
    let format = FormatId::try_new(jqf_codec_html::FORMAT_ID).expect("format");
    let dialect = DialectId::try_new(ENCODE_DIALECT).expect("dialect");
    let registration = jqf_codec_html::registration().expect("registration");
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
            .expect_err("doctype-bearing document must refuse");
        assert_eq!(error.kind(), CodecFailureKind::UnsupportedRepresentation);
    }
    assert!(out.is_empty(), "failed encode published {} bytes", out.len());
}

/// R-ENC-B + P-BOUND: the §1 value mapping's byte law — exactly one UTF-8 BOM, then the WHATWG serialization of the
/// lowered `root` element.
#[test]
fn law_r_enc_b_byte_boundary() {
    let mut resources = resources();
    let mut builder = jqf_data::ObjectBuilder::try_with_capacity(2).expect("builder");
    builder
        .try_insert_last(
            jqf_data::ObjectKey::try_from_str("a").expect("key"),
            Value::Number(jqf_data::Number::integer(jqf_data::Integer::from_i64(1))),
        )
        .expect("insert");
    builder
        .try_insert_last(
            jqf_data::ObjectKey::try_from_str("b").expect("key"),
            Value::String(jqf_data::Shared::try_from_str("x&y").expect("string")),
        )
        .expect("insert");
    let object = Value::Object(builder.try_finish().expect("object"));
    let format = FormatId::try_new(jqf_codec_html::FORMAT_ID).expect("format");
    let dialect = DialectId::try_new(ENCODE_DIALECT).expect("dialect");
    let registration = jqf_codec_html::registration().expect("registration");
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
        .start(EncodeItem::Owned(&object), PreservationRequest::None, &mut resources)
        .expect("session");
    let mut out = Vec::new();
    {
        let mut sink = VecByteSink::new(&mut out);
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        session.encode(&mut sink, &mut run).expect("encode");
    }
    let out = String::from_utf8(out).expect("utf8 output");
    assert_eq!(
        out, "\u{FEFF}<root><a>1</a><b>x&amp;y</b></root>",
        "the §1 value mapping byte law"
    );
}

/// R-HINT: demand is a hint — an empty demand and the full demand decode to the same value.
#[test]
fn law_r_hint_demand_is_a_hint() {
    let bytes = b"<p>one<br>two</p>";
    let mut resources = resources();
    let (full, _) = decode_whole(bytes, &mut resources).expect("full-demand decode");
    let registration = jqf_codec_html::registration().expect("registration");
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

/// R-VOC: the text vocabulary — named entities, numeric references and legacy encodings survive the roundtrip.
#[test]
fn law_r_voc_text_vocabulary_roundtrip() {
    let mut resources = resources();
    let (value, product) = decode_whole(b"<p>&amp;&lt;&#65; caf\xe9</p>", &mut resources).expect("decode");
    let root = product.document().root_handle();
    let (encoded, _) = encode_located(&product, root, &mut resources).expect("encode");
    let redecoded = decode(&encoded).expect("re-decode");
    assert_eq!(
        render(&value),
        render(&redecoded),
        "text vocabulary roundtrip (bytes {encoded:?})"
    );
}

/// P-ZERO: a failed encode publishes ZERO bytes to the sink.
#[test]
fn law_p_zero_failed_encode_publishes_nothing() {
    // The source profile has no serializer fallback: an owned value is refused outright, before any byte is offered.
    let mut resources = resources();
    let value = Value::String(jqf_data::Shared::try_from_str("hello").expect("string"));
    let format = FormatId::try_new(jqf_codec_html::FORMAT_ID).expect("format");
    let dialect = DialectId::try_new(jqf_codec_html::HTML_SOURCE_DIALECT_ID).expect("dialect");
    let registration = jqf_codec_html::registration().expect("registration");
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
            "owned value under the source profile: {error:?}"
        );
        let message = error.diagnostic().map_or("", jqf_source::Diagnostic::message);
        assert!(!message.contains('\\'), "diagnostic leaked a backslash: {message:?}");
    }
    assert!(out.is_empty(), "failed encode published {} bytes", out.len());
}

/// P-CHUNK: chunked publication is bounded — a sink capped at a small chunk still receives the complete golden output
/// via the backpressure retry surface.
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
    let mut body = String::from("<body>");
    for index in 0..64 {
        let _ = core::write!(body, "<p>{index}</p>");
    }
    body.push_str("</body>");
    let mut resources = resources();
    let (_value, product) = decode_whole(body.as_bytes(), &mut resources).expect("decode");
    let root = product.document().root_handle();
    let (golden, _) = encode_located(&product, root, &mut resources).expect("golden encode");
    assert!(!golden.is_empty());
    let format = FormatId::try_new(jqf_codec_html::FORMAT_ID).expect("format");
    let dialect = DialectId::try_new(ENCODE_DIALECT).expect("dialect");
    let registration = jqf_codec_html::registration().expect("registration");
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
            cap: 7,
        };
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        session.encode(&mut sink, &mut run).expect("bounded encode completes");
    }
    assert_eq!(out, golden, "bounded-sink bytes must match the unbounded run");
}

/// A-REPORT: the `PreservationReport` is honest — a roundtrip through the document-serialize profile reports Exact on
/// the axes the bytes really preserve.
#[test]
fn law_a_report_axes_are_honest() {
    let mut resources = resources();
    let (_value, product) = decode_whole(b"<p>x</p>", &mut resources).expect("decode");
    let root = product.document().root_handle();
    let (_, report) = encode_located(&product, root, &mut resources).expect("encode");
    assert_eq!(
        report.semantic_values(),
        PreservationOutcome::Exact,
        "the recovered tree survives serialization exactly"
    );
    // The serializer's byte law is deterministic but not documented as order-guaranteed, so the codec reports ordering
    // as Normalized — the honest report must never claim Exact for a change it made, and never claim Unrepresentable
    // for an order the roundtrip actually preserved.
    assert!(
        matches!(
            report.ordering(),
            PreservationOutcome::Exact | PreservationOutcome::Normalized
        ),
        "child order must be Exact or Normalized, got {:?}",
        report.ordering()
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
    let mut resources = ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
            .expect("account"),
        &CANCEL,
        WorkMeter::try_new_v1(1).expect("work"),
    )
    .expect("resources");
    let bytes = b"<html><body><p>one</p><p>two</p><p>three</p></body></html>";
    let registration = jqf_codec_html::registration().expect("registration");
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

/// Cross-cutting: the two-kind declaration — HTML serves `CompleteDocument` and never `RecordStream` — plus the
/// span/`--edit` law as a declared absence.
#[test]
fn law_table_declarations() {
    let registration = jqf_codec_html::registration().expect("registration");
    let operations = registration.descriptor().operations();
    assert!(operations.decode(), "HTML must declare decoder construction");
    assert!(operations.encode(), "HTML must declare encoder construction");
    let fragment = jqf_codec_html::registration_fragment().expect("fragment");
    assert!(
        fragment.descriptor().operations().decode() && !fragment.descriptor().operations().encode(),
        "html.fragment@1 decodes and does not advertise encode"
    );
    // The span/`--edit` law is a DECLARED absence here: HTML has no edit splice surface, so there is nothing the table
    // could pin. A skipped table is a declared absence, never a hole.
    let mut resources = resources();
    let provider = registration
        .decoder()
        .expect("decoder")
        .create_provider(
            source(b"<p>hi</p>"),
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
            "HTML must never advertise a record stream"
        );
    }
    assert!(
        routes
            .iter()
            .any(|route| route.bundle().result() == AccessResultKind::CompleteDocument),
        "HTML must advertise CompleteDocument"
    );
}

/// `html.fragment@1` recovers under the fixed `div` context: `<em>x</em>` is an element, not RCDATA text.
#[test]
fn law_fragment_default_context_is_div() {
    let mut resources = resources();
    let registration = jqf_codec_html::registration_fragment().expect("registration");
    let dialect = DialectId::try_new(jqf_codec_html::HTML_FRAGMENT_DIALECT_ID).expect("dialect");
    let mut provider = registration
        .decoder()
        .expect("decoder")
        .create_provider(
            source(b"<em>x</em>"),
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
    let root = product.document().root_handle();
    let value = product
        .document()
        .materialize_node_with(&mut jqf_data::MaterializeWorkspace::new(), root, &mut resources)
        .expect("materialize");
    let rendered = render(&value);
    assert!(
        rendered.contains('x') && !rendered.contains("<em>"),
        "div-context fragment must parse em as an element, got {rendered}"
    );
}

/// Comment-only input still recovers (synthetic empty html element).
#[test]
fn law_comment_only_input_recovers() {
    let value = decode(b"<!--x-->").expect("comment-only recovers");
    let _ = render(&value);
}
