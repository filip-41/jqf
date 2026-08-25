//! `-n` record cursor: decode on `input`/`inputs` pull, not an eager table.

use jqf_codec_core::{DecodeRequest, DiagnosticPolicy, PreservationRequest, ValidationMode};
use jqf_codec_json::ndjson::{NdjsonDecodeOptions, NdjsonProfile};
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

fn json_dialect() -> &'static DialectId {
    Box::leak(Box::new(DialectId::try_new("rfc8259").expect("dialect")))
}

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
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, 64 << 20, 0, 128)).expect("account"),
        &CONTROL,
        WorkMeter::try_new_v1(COOPERATIVE_CREDITS).expect("work"),
    )
    .expect("resources")
}

struct NullFirstRun {
    bytes: Vec<u8>,
    records: u64,
}

fn run_null_first(input: &[u8], program: &str) -> Result<NullFirstRun, String> {
    let json = jqf_codec_json::registration().expect("json registration");
    let streams = jqf_codec_json::ndjson::registration().expect("ndjson registration");
    let registrations = [&json, &streams];
    let catalog = CodecCatalog::new(&registrations);
    let format = FormatId::try_new(jqf_codec_json::FORMAT_ID).expect("format");
    let dialect = DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect");
    let mut resources = resources();
    let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    let compiled = try_compile_program(program, policy, &resources).expect("compiles");
    let requirement = compiled
        .try_pulled_record_requirement(&resources)
        .expect("pulled-record requirement lowers");
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "test.ndjson",
        input,
        0,
    );
    let record_options = NdjsonDecodeOptions::try_new(None, INPUT_CEILING).expect("record ceiling");
    let records = jqf_codec_json::ndjson::create_record_provider(
        source,
        NdjsonProfile::Strict,
        record_options,
        DiagnosticPolicy::ErrorsOnly,
        ValidationMode::Strict,
        &mut resources,
    )
    .expect("record provider");
    let mut sink = CollectingSink::new();
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
    .with_format(format.clone(), dialect.clone())
    .with_output_format(format, dialect)
    .with_policy(PipelinePolicy {
        decode: DecodeRequest {
            validation: ValidationMode::Strict,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            dialect: json_dialect(),
            options: None,
            allow_adjacent_values: false,
            value_separator: jqf_codec_json::VALUE_SEPARATORS,
        },
        encode_diagnostics: DiagnosticPolicy::ErrorsOnly,
        preservation: PreservationRequest::None,
        encode_options: None,
        cooperative_credits: COOPERATIVE_CREDITS,
        split: None,

        max_iterations: None,
    })
    .with_framing(FacadeFraming::item_suffix(b"\n"))
    .with_resources(&mut resources)
    .with_requirement(&requirement)
    .with_null_input();
    match jqf_sdk::execute(request, &mut sink) {
        Ok(Outcome::Served(Report::Record(report))) => Ok(NullFirstRun {
            bytes: sink.bytes,
            records: report.records(),
        }),
        Ok(other) => Err(format!("unexpected outcome: {other:?}")),
        Err(error) => Err(format!("execute failed: {error}")),
    }
}

#[test]
fn input_v_publishes_one_value_and_ignores_an_unpulled_malformed_record() {
    let input = b"{\"v\":1}\n{\"v\":2}\n{\"v\":3}\nnot-json\n";
    let run = run_null_first(input, "input.v").unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(run.bytes, b"1\n");
    assert_eq!(run.records, 1);
}

#[test]
fn reduce_inputs_counts_every_valid_record() {
    let input = b"{\"v\":1}\n{\"v\":2}\n{\"v\":3}\n";
    let run = run_null_first(input, "reduce inputs as $r (0; . + 1)").unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(run.bytes, b"3\n");
    assert_eq!(run.records, 3);
}

#[test]
fn reduce_inputs_sum_matches_the_eager_table_bytes_on_a_valid_stream() {
    let input = b"{\"v\":1}\n{\"v\":2}\n{\"v\":3}\n";
    let run = run_null_first(input, "reduce inputs as $r (0; . + $r.v)").unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(run.bytes, b"6\n");
    assert_eq!(run.records, 3);
}
