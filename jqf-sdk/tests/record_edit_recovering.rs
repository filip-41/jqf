//! The record EDIT lane's recovering law: a malformed payload under a
//! recovering route is an ordered error issue the tally carries into the
//! returned report (the exit-class authority), never a torn-down drive —
//! the same rule `execute_record_sequence` keeps.

use jqf_codec_core::{DecodeRequest, DiagnosticPolicy, PreservationRequest, ValidationMode};
use jqf_codec_json::ndjson::{NdjsonDecodeOptions, NdjsonEncodeOptions, NdjsonProfile, NdjsonTerminator};
use jqf_data::{DialectId, FormatId};
use jqf_engine::{CodecRequirementPolicy, try_compile_program};
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_sdk::{
    CodecCatalog, EncodedItemReport, FacadeFraming, Input, ItemSink, Outcome, PipelinePolicy, Report, Request,
};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

const COOPERATIVE_CREDITS: u32 = 64;
const INPUT_CEILING: u64 = 1 << 20;
static CONTROL: ContinueControl = ContinueControl;

struct CollectingSink {
    bytes: Vec<u8>,
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
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, 64 << 20, 0, 128)).expect("account"),
        &CONTROL,
        WorkMeter::try_new_v1(COOPERATIVE_CREDITS).expect("work meter starts"),
    )
    .expect("resources start")
}

/// Runs the record EDIT lane over an NDJSON stream framed with `profile`,
/// returning the report and the published bytes.
fn run_record_edit(input: &[u8], profile: NdjsonProfile) -> Result<(jqf_sdk::RecordSequenceReport, Vec<u8>), String> {
    let json = jqf_codec_json::registration().expect("json registration");
    let streams = jqf_codec_json::ndjson::registration().expect("ndjson registration");
    let registrations = [&json, &streams];
    let catalog = CodecCatalog::new(&registrations);
    let format = FormatId::try_new(jqf_codec_json::FORMAT_ID).expect("format");
    let dialect = DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect");
    let decode_dialect = DialectId::try_new("rfc8259").expect("decode dialect");
    let output_format = FormatId::try_new(jqf_codec_json::ndjson::FORMAT_ID).expect("output");
    let output_dialect = DialectId::try_new(jqf_codec_json::ndjson::STRICT_DIALECT_ID).expect("output dialect");
    let mut resources = resources();
    let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    let compiled = try_compile_program(".", policy, &resources).expect("compiles");
    let requirement = compiled.try_requirement(&resources).expect("requirement lowers");
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "test.ndjson",
        input,
        0,
    );
    let record_options = NdjsonDecodeOptions::try_new(None, INPUT_CEILING).expect("record ceiling");
    let validation = match profile {
        NdjsonProfile::Recovering => ValidationMode::Recover,
        NdjsonProfile::Strict => ValidationMode::Strict,
    };
    let records = jqf_codec_json::ndjson::create_record_provider(
        source,
        profile,
        record_options,
        DiagnosticPolicy::ErrorsOnly,
        validation,
        &mut resources,
    )
    .expect("record provider");
    let encode_options = NdjsonEncodeOptions::new(NdjsonTerminator::Lf);
    let mut sink = CollectingSink { bytes: Vec::new() };
    let request = Request::new(
        &compiled,
        Input::Records {
            source: source.bytes(),
            records,
            slot: jqf_codec_json::ndjson::RECORD_ROUTE_SLOT,
        },
    )
    .with_catalog(catalog)
    .with_source(source)
    .with_format(format, dialect)
    .with_output_format(output_format, output_dialect)
    .with_policy(PipelinePolicy {
        decode: DecodeRequest {
            validation: ValidationMode::Strict,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            dialect: &decode_dialect,
            options: None,
            allow_adjacent_values: false,
            value_separator: jqf_codec_json::VALUE_SEPARATORS,
        },
        encode_diagnostics: DiagnosticPolicy::ErrorsOnly,
        preservation: PreservationRequest::None,
        encode_options: Some(&encode_options),
        cooperative_credits: COOPERATIVE_CREDITS,
        split: None,
        max_iterations: None,
    })
    .with_framing(FacadeFraming::item_suffix(b""))
    .with_resources(&mut resources)
    .with_requirement(&requirement)
    .editing();
    match jqf_sdk::execute(request, &mut sink) {
        Ok(Outcome::Served(Report::Record(report))) => Ok((report, sink.bytes)),
        other => Err(format!("unexpected outcome: {other:?}")),
    }
}

#[test]
fn a_malformed_payload_is_an_ordered_issue_and_the_lane_completes() {
    // The second record frames cleanly but its payload fails the strict JSON
    // ladder. Under the recovering route that is one ordered error issue;
    // the first record still edits and publishes, and the tallies reach the
    // report so the caller's exit-class law can fire.
    let (report, bytes) =
        run_record_edit(b"{\"a\":1}\nnot json\n", NdjsonProfile::Recovering).expect("the recovering lane completes");
    assert_eq!(report.items(), 1);
    assert_eq!(report.issues(), 1, "the malformed payload is one issue");
    assert_eq!(
        report.error_issues(),
        1,
        "an error-severity issue reaches the exit-class tally"
    );
    assert!(
        String::from_utf8_lossy(&bytes).contains("\"a\":1"),
        "first record published: {bytes:?}"
    );
    assert!(
        !String::from_utf8_lossy(&bytes).contains("not"),
        "the bad record publishes nothing"
    );
}

#[test]
fn a_malformed_payload_still_tears_down_under_strict() {
    let error = run_record_edit(b"{\"a\":1}\nnot json\n", NdjsonProfile::Strict)
        .expect_err("the strict profile aborts the drive");
    assert!(
        error.contains("Codec") || error.contains("InvalidInput") || error.contains("MalformedPayload"),
        "expected a codec teardown, got {error}"
    );
}
