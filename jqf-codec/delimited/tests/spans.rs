//! Per-node authored span binding for the delimited record payloads.
//!
//! Each record document binds two span shapes: the ROOT node carries the record's extent (the row's authored bytes,
//! terminator excluded) and every field VALUE node carries its RAW authored span — quotes included, never the decoded
//! string. The span is an addressing channel, never a second value: this module asserts that a value node's span slices
//! the retained source segment to exactly the field's authored bytes, the contract the edit lane's splice reads.

use jqf_codec_core::{
    AccessGuarantees, AccessOutcome, AccessRequirement, CodecDemand, CodecRunContext, DecodeRequest, DemandClause,
    DiagnosticPolicy, ReusableAccessSession, ValidationMode,
};
use jqf_data::NodeId;
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

static CONTROL: ContinueControl = ContinueControl;

fn resources() -> ResourceContext<'static> {
    ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(1 << 20, u64::MAX, 64 << 20, 0, 128)).expect("account allocates"),
        &CONTROL,
        WorkMeter::try_new_v1(4096).expect("work meter starts"),
    )
    .expect("resources start")
}

/// The whole-document requirement a `.`-class program lowers to: the strict guarantee pair plus the semantic-root and
/// value-shape demands.
fn requirement(resources: &ResourceContext<'_>) -> AccessRequirement {
    let mut demand = CodecDemand::try_new(resources);
    demand.try_insert(&DemandClause::SemanticRoot).expect("semantic root");
    demand.try_insert(&DemandClause::ValueShape).expect("value shape");
    let guarantees = AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly);
    AccessRequirement::try_whole(demand, guarantees, resources).expect("whole requirement")
}

/// Decodes ONE record's payload range through the whole-document payload route and returns the source-backed product,
/// mirroring the record drive's `open_range_reusing` shape (`-` class programs bind the lazy whole document, the DECODE
/// route slot).
fn decode_record(input: &[u8], payload_start: u64, payload_end: u64) -> jqf_codec_core::DocumentProduct<'_> {
    let mut resources = resources();
    let registration = jqf_codec_delimited::registration().expect("csv registration");
    let mut provider = registration
        .decoder()
        .expect("decoder")
        .create_provider(
            ResolvedSource::new(
                SourceRef::new(SourceId::new(1), SourceKind::Input),
                "spans.csv",
                input,
                0,
            ),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &jqf_data::DialectId::try_new(jqf_codec_delimited::JQF_RFC4180_DIALECT_ID).expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            &mut resources,
        )
        .expect("provider");
    let requirement = requirement(&resources);
    let handle = provider.bind(&requirement).expect("bind");
    let mut reuse = ReusableAccessSession::new();
    let access = provider
        .open_range_reusing(&handle, payload_start, payload_end, &mut reuse, &mut resources)
        .expect("payload open");
    let mut run = CodecRunContext::new(&mut resources);
    let result = access.decode(&mut run).expect("decode");
    let AccessOutcome::FullDocument(product) = result.outcome() else {
        panic!("expected a full document from the whole-record route");
    };
    product.try_clone().expect("clone spans test product")
}

/// Every span the document binds, in node order, as (start, end) pairs.
fn all_spans(document: &jqf_data::Document<'_>) -> Vec<(u32, u32)> {
    (0..document.node_count())
        .map(|index| {
            let node = NodeId::try_from_index(index).expect("node index");
            document
                .node_source_span(node)
                .expect("node source span")
                .map(|span| (span.start(), span.end()))
        })
        .collect::<Option<Vec<_>>>()
        .expect("every node binds its authored span")
}

/// A field value node's span slices the source segment to the field's OWN authored bytes — quotes and separators
/// included. For the array dialect the root (node 0) is the record's extent and each field value binds its raw byte
/// range; the headered dialect binds the same record extent on the object root.
#[test]
fn a_field_value_span_slices_the_source_to_its_authored_bytes() {
    // Record 1 `ada,"a,b"` occupies bytes 10..19 of the retained source; the narrowed payload segment is exactly those
    // bytes.
    let input = b"name,note\nada,\"a,b\"\nbob,plain\n";
    let product = decode_record(input, 10, 19);
    let document = product.document();
    let segment = document.source_segment().expect("the record document is source-backed");
    assert_eq!(segment, b"ada,\"a,b\"");
    // Node order: the root array first, then the two field values.
    let spans = all_spans(document);
    assert_eq!(spans, [(0, 9), (0, 3), (4, 9)], "record+field spans");
    // Slicing the segment with each span reproduces the authored bytes: the second field keeps its quotes, `"a,b"`,
    // never the decoded `a,b`.
    assert_eq!(&segment[spans[0].0 as usize..spans[0].1 as usize], b"ada,\"a,b\"");
    assert_eq!(&segment[spans[1].0 as usize..spans[1].1 as usize], b"ada");
    assert_eq!(&segment[spans[2].0 as usize..spans[2].1 as usize], b"\"a,b\"");
}

/// Under the TSV grammar a quote is plain field data, and the bound span keeps it: the authored bytes ARE the raw
/// field.
#[test]
fn a_tsv_field_span_keeps_quotes_as_data() {
    // `x\tq\na\t"b"\tc\n`: record 1 `a\t"b"\tc` occupies bytes 4..11.
    let input = b"x\tq\na\t\"b\"\tc\n";
    let product = decode_record_tsv(input, 4, 11);
    let document = product.document();
    let segment = document.source_segment().expect("the record document is source-backed");
    assert_eq!(segment, b"a\t\"b\"\tc");
    let spans = all_spans(document);
    assert_eq!(spans, [(0, 7), (0, 1), (2, 5), (6, 7)], "record+field spans");
    assert_eq!(&segment[spans[2].0 as usize..spans[2].1 as usize], b"\"b\"");
}

fn decode_record_tsv(input: &[u8], payload_start: u64, payload_end: u64) -> jqf_codec_core::DocumentProduct<'_> {
    let mut resources = resources();
    let tsv = jqf_codec_delimited::registration_tsv().expect("tsv registration");
    let options = Some(
        &jqf_codec_delimited::CsvDecodeOptions::try_new_tsv(None, 1 << 20, false).expect("tsv options")
            as &(dyn core::any::Any + Send + Sync),
    );
    let mut provider = tsv
        .decoder()
        .expect("decoder")
        .create_provider(
            ResolvedSource::new(
                SourceRef::new(SourceId::new(1), SourceKind::Input),
                "spans.tsv",
                input,
                0,
            ),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &jqf_data::DialectId::try_new(jqf_codec_delimited::TSV_UTF8_DIALECT_ID).expect("dialect"),
                options,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            &mut resources,
        )
        .expect("provider");
    let requirement = requirement(&resources);
    let handle = provider.bind(&requirement).expect("bind");
    let mut reuse = ReusableAccessSession::new();
    let access = provider
        .open_range_reusing(&handle, payload_start, payload_end, &mut reuse, &mut resources)
        .expect("payload open");
    let mut run = CodecRunContext::new(&mut resources);
    let result = access.decode(&mut run).expect("decode");
    let AccessOutcome::FullDocument(product) = result.outcome() else {
        panic!("expected a full document from the whole-record route");
    };
    product.try_clone().expect("clone spans test product")
}

/// The headered dialect binds the same record extent on the OBJECT root, and each member value binds its raw field span
/// (the splice policy keys on the same per-field spans).
#[test]
fn the_headered_dialect_binds_the_record_extent_on_the_object_root() {
    let input = b"name,note\nada,\"a,b\"\nbob,plain\n";
    let mut resources = resources();
    let registration = jqf_codec_delimited::registration().expect("csv registration");
    let options = Some(
        &jqf_codec_delimited::CsvDecodeOptions::try_new(None, None, 1 << 20, true).expect("headered options")
            as &(dyn core::any::Any + Send + Sync),
    );
    let mut provider = registration
        .decoder()
        .expect("decoder")
        .create_provider(
            ResolvedSource::new(
                SourceRef::new(SourceId::new(1), SourceKind::Input),
                "spans-h.csv",
                input,
                0,
            ),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &jqf_data::DialectId::try_new(jqf_codec_delimited::JQF_RFC4180_HEADER_DIALECT_ID)
                    .expect("dialect"),
                options,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            &mut resources,
        )
        .expect("provider");
    let requirement = requirement(&resources);
    let handle = provider.bind(&requirement).expect("bind");
    let mut reuse = ReusableAccessSession::new();
    let access = provider
        .open_range_reusing(&handle, 10, 19, &mut reuse, &mut resources)
        .expect("payload open");
    let mut run = CodecRunContext::new(&mut resources);
    let result = access.decode(&mut run).expect("decode");
    let AccessOutcome::FullDocument(product) = result.outcome() else {
        panic!("expected a full document from the whole-record route");
    };
    let document = product.document();
    let segment = document.source_segment().expect("the record document is source-backed");
    let spans = all_spans(document);
    assert_eq!(spans, [(0, 9), (0, 3), (4, 9)], "record+field spans");
    assert_eq!(&segment[spans[0].0 as usize..spans[0].1 as usize], b"ada,\"a,b\"");
    // The object root (node 0, the build's first node) carries the record extent; the member values carry the raw field
    // bytes including quotes.
    let root = NodeId::try_from_index(0).expect("root index");
    let root_span = document
        .node_source_span(root)
        .expect("root span")
        .expect("the object root binds the record extent");
    assert_eq!((root_span.start(), root_span.end()), (0, 9));
}
