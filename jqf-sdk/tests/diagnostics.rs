//! The diagnostics vertical's three receipts, through the public SDK surface:
//!
//! 1. SCHEMA GOLDEN — the record stream's JSON serialization is pinned, so a
//!    binding that parses it cannot silently drift.
//! 2. LEDGER INVARIANCE — the request ledger is byte-identical under Off and
//!    All: the record stream is never charged to the request.
//! 3. PAYLOAD-FREE ON PROJECTED ROUTES — a projected-route run that raises
//!    per element produces records whose operand never carries the offending
//!    element's payload (the projection soundness condition, re-stated on
//!    data).
//!
//! These live in the SDK's own test suite (not jqf-sdk-smoke) because they
//! exercise the SDK surface directly; the smoke battery runs them via the
//! workspace test gate.

use jqf_codec_core::{DecodeRequest, DiagnosticPolicy, PreservationRequest, ValidationMode};
use jqf_data::{DialectId, FormatId};
use jqf_engine::{CodecRequirementPolicy, try_compile_program};
use jqf_resource::diag::{DiagnosticBuffer, DiagnosticRecord, DiagnosticSink};
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_sdk::{
    CodecCatalog, Diagnostics, EncodedItemReport, FacadeFraming, Input, ItemSink, PipelinePolicy, Request, record_json,
};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

const COOPERATIVE_CREDITS: u32 = 64;
static CONTROL: ContinueControl = ContinueControl;

struct CollectingSink {
    bytes: Vec<u8>,
}

impl CollectingSink {
    fn new() -> Self {
        Self { bytes: Vec::new() }
    }
}

impl ItemSink for CollectingSink {
    type Error = &'static str;

    fn begin_item(&mut self, _index: u64) -> Result<(), Self::Error> {
        Ok(())
    }
    fn write(&mut self, bytes: &[u8]) -> Result<usize, Self::Error> {
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn finish_item(&mut self, _index: u64, _report: EncodedItemReport) -> Result<(), Self::Error> {
        Ok(())
    }
}

fn resources() -> ResourceContext<'static> {
    ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
            .expect("account"),
        &CONTROL,
        WorkMeter::try_new_v1(COOPERATIVE_CREDITS).expect("work"),
    )
    .expect("resources")
}

/// Runs `program` over `input` through the SDK's `execute`, with the given
/// diagnostics policy, returning the retained records (or the failure).
fn run_with_policy(
    program: &str,
    input: &[u8],
    policy: DiagnosticPolicy,
) -> (Vec<jqf_resource::diag::OwnedDiagnosticRecord>, Option<String>) {
    let mut resources = resources();
    let registration = jqf_codec_json::registration().expect("json registration");
    let registrations = [&registration];
    let catalog = CodecCatalog::new(&registrations);
    let format = || FormatId::try_new(jqf_codec_json::FORMAT_ID).expect("format id");
    let dialect = || DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect id");
    let compiled = try_compile_program(
        program,
        CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly),
        &resources,
    )
    .expect("compile");
    let requirement = compiled.try_requirement(&resources).expect("requirement");
    let source = ResolvedSource::new(SourceRef::new(SourceId::new(1), SourceKind::Input), "<test>", input, 0);
    let diagnostics = Diagnostics::new(policy);
    let mut sink = CollectingSink::new();
    let policy_options = PipelinePolicy {
        decode: DecodeRequest {
            validation: ValidationMode::Strict,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            dialect: &DialectId::try_new("rfc8259").expect("dialect"),
            options: None,
            allow_adjacent_values: false,
            value_separator: jqf_codec_json::VALUE_SEPARATORS,
        },
        encode_diagnostics: DiagnosticPolicy::ErrorsOnly,
        preservation: PreservationRequest::Report,
        encode_options: None,
        cooperative_credits: COOPERATIVE_CREDITS,
        split: None,

        max_iterations: None,
    };
    let request = Request::new(&compiled, Input::Whole(input))
        .with_catalog(catalog)
        .with_source(source)
        .with_format(format(), dialect())
        .with_output_format(format(), dialect())
        .with_policy(policy_options)
        .with_framing(FacadeFraming::item_suffix(b"\n"))
        .with_resources(&mut resources)
        .with_diagnostics(diagnostics.as_ref())
        .with_requirement(&requirement);
    let outcome = jqf_sdk::execute(request, &mut sink);
    match outcome {
        Ok(_) => (diagnostics.map(|d| d.records()).unwrap_or_default(), None),
        Err(error) => (
            diagnostics.map(|d| d.records()).unwrap_or_default(),
            Some(format!("{:?}", error.pipeline_failure())),
        ),
    }
}

/// The `jqf: diag` NDJSON stream escapes control bytes.
/// `record_json` is exactly the line the CLI prints after `jqf: diag `; the
/// shared escaper (jqf-codec-json's `push_json_escaped`) must write a C0
/// control and DEL as escapes, never raw bytes — a raw byte here would make
/// the NDJSON line invalid JSON. An escaper that handled only quote,
/// backslash, and newline would under-escape here.
#[test]
fn diag_stream_escapes_control_bytes() {
    let buffer = DiagnosticBuffer::with_cap(1);
    let mut incoming = DiagnosticRecord::new(7);
    incoming.step_index = Some(1);
    incoming.input_ordinal = Some(0);
    incoming.kind = Some("string");
    incoming.operand = Some("a\u{1}b");
    incoming.payload = Some("c\u{7f}d");
    buffer.record(incoming);
    let record = buffer.records().pop().expect("record");
    let line = record_json(&record);
    assert!(line.contains("\\u0001"), "C0 control must be escaped: {line}");
    assert!(line.contains("\\u007f"), "DEL must be escaped: {line}");
    assert!(!line.contains('\u{1}'), "no raw control byte may survive: {line}");
    assert!(!line.contains('\u{7f}'), "no raw DEL may survive: {line}");
}

/// RECEIPT 1: the record stream's JSON shape is pinned. Any change to the
/// schema (fields, names, ordering) fails here rather than breaking bindings
/// silently.
#[test]
fn schema_golden_receipt() {
    let (records, failure) = run_with_policy(".a + .b", br#"{"a": 1, "b": 2}"#, DiagnosticPolicy::All);
    assert!(failure.is_none(), "the run must succeed: {failure:?}");
    let json: Vec<String> = records.iter().map(record_json).collect();
    assert_eq!(json.len(), 2, "route + cost records expected: {json:?}");
    // Route first: EVERY field is pinned. execute names the rung it served
    // (`single-document` here: adjacent values are off), so the operand is
    // the route name rather than null.
    assert_eq!(
        json[0],
        "{\"code\":1,\"name\":\"ROUTE_SELECTED\",\"revision\":1,\"class\":\"Informational\",\
         \"severity\":\"Info\",\"catchable\":false,\"caught\":null,\"step\":null,\"input\":null,\
         \"byte_offset\":null,\"halt_status\":null,\"kind\":null,\"operand\":\"single-document\",\"payload\":null}"
    );
    // Cost second: the operand is a formatted summary whose numbers vary, so
    // pin every fixed field through the operand's opening, the four operand
    // field names (presence; their relative order is the serializer's and not
    // asserted here), and the fixed tail.
    let cost = &json[1];
    assert!(
        cost.starts_with(
            "{\"code\":2,\"name\":\"COST_SNAPSHOT\",\"revision\":1,\"class\":\"Informational\",\
             \"severity\":\"Info\",\"catchable\":false,\"caught\":null,\"step\":null,\"input\":null,\
             \"byte_offset\":null,\"halt_status\":null,\"kind\":null,\"operand\":\"peak="
        ),
        "{cost:?}"
    );
    for field in [" input=", " output=", " spill_disk="] {
        assert!(cost.contains(field), "cost operand must carry {field}: {cost:?}");
    }
    assert!(
        cost.ends_with("\",\"payload\":null}"),
        "the cost record's fixed tail must be intact: {cost:?}"
    );
}

/// RECEIPT 2: the request ledger is identical under Off and All — the record
/// stream is never charged to the request.
#[test]
fn ledger_invariance_receipt() {
    let program = ".a | length";
    let input = br#"{"a": [1, 2, 3, 4, 5]}"#;
    let registration = jqf_codec_json::registration().expect("json registration");
    let registrations = [&registration];
    let catalog = CodecCatalog::new(&registrations);
    let format = || FormatId::try_new(jqf_codec_json::FORMAT_ID).expect("format id");
    let dialect = || DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect id");

    // One run per policy, inlined (the buffer must outlive the context, so
    // the two are declared together per run).
    let run_under = |policy: DiagnosticPolicy| -> (Vec<u8>, jqf_resource::UsageSnapshot) {
        let mut resources = resources();
        let compiled = try_compile_program(
            program,
            CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly),
            &resources,
        )
        .expect("compile");
        let requirement = compiled.try_requirement(&resources).expect("requirement");
        let source = ResolvedSource::new(SourceRef::new(SourceId::new(1), SourceKind::Input), "<test>", input, 0);
        let diagnostics = Diagnostics::new(policy);
        let mut sink = CollectingSink::new();
        let policy_options = PipelinePolicy {
            decode: DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new("rfc8259").expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: jqf_codec_json::VALUE_SEPARATORS,
            },
            encode_diagnostics: DiagnosticPolicy::ErrorsOnly,
            preservation: PreservationRequest::Report,
            encode_options: None,
            cooperative_credits: COOPERATIVE_CREDITS,
            split: None,

            max_iterations: None,
        };
        let request = Request::new(&compiled, Input::Whole(input))
            .with_catalog(catalog)
            .with_source(source)
            .with_format(format(), dialect())
            .with_output_format(format(), dialect())
            .with_policy(policy_options)
            .with_framing(FacadeFraming::item_suffix(b"\n"))
            .with_resources(&mut resources)
            .with_diagnostics(diagnostics.as_ref())
            .with_requirement(&requirement);
        jqf_sdk::execute(request, &mut sink).expect("run");
        let usage = resources.snapshot();
        (sink.bytes, usage)
    };

    let (bytes_off, usage_off) = run_under(DiagnosticPolicy::Off);
    let (bytes_all, usage_all) = run_under(DiagnosticPolicy::All);
    assert_eq!(bytes_off, bytes_all, "output must be identical under both policies");
    assert_eq!(
        usage_off.memory_peak_bytes(),
        usage_all.memory_peak_bytes(),
        "the record stream must never charge the request ledger"
    );
    assert_eq!(
        usage_off.memory_current_bytes(),
        usage_all.memory_current_bytes(),
        "current bytes must be identical too"
    );
}

#[test]
fn per_element_route_records_are_payload_free() {
    // The distinctive marker is a long unique string: if a per-element route
    // record leaked the offending element's payload, the marker would appear in it.
    let marker = "PAYLOAD-FREE-MARKER-7f3a9c21e8b54d06";
    let input = format!(r#"[{{"id": 1}}, {{"id": "{marker}"}}, {{"id": 3}}]"#);
    let (records, failure) = run_with_policy(".[] | .id + 1", input.as_bytes(), DiagnosticPolicy::All);
    // The program raises per element on the sequence drive (the `id` of the
    // second element is a string). The failure must be present...
    assert!(failure.is_some(), "the per-element run must fail: {records:?}");
    let operand_text: String = records.iter().fold(String::new(), |mut text, record| {
        if let Some(operand) = record.operand() {
            text.push_str(operand);
        }
        if let Some(payload) = record.payload() {
            text.push_str(payload);
        }
        text
    });
    assert!(
        !operand_text.contains(marker),
        "a per-element-route record leaked the offending element's payload: {operand_text:?}"
    );
}
