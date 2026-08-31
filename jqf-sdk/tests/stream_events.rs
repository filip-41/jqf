//! Streaming `--stream` dispatcher and trailing-codec-failure laws.

use core::any::Any;

use jqf_codec_core::{CodecFailureKind, DecodeRequest, DiagnosticPolicy, PreservationRequest, ValidationMode};
use jqf_data::{DialectId, FormatId};
use jqf_engine::{CodecRequirementPolicy, CompileOptions, try_compile_program};
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_sdk::{
    CodecCatalog, EncodedItemReport, FacadeFraming, Failure, Input, ItemSink, PipelineFailure, PipelinePolicy,
    ReadFailure, Request, RequestError,
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
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, 64 << 20, 0, 128)).expect("account allocates"),
        &CONTROL,
        WorkMeter::try_new_v1(COOPERATIVE_CREDITS).expect("work meter starts"),
    )
    .expect("resources start")
}

fn format() -> FormatId {
    FormatId::try_new(jqf_codec_json::FORMAT_ID).expect("format id is valid")
}

fn dialect() -> DialectId {
    DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect id is valid")
}

fn compile(program: &str, resources: &ResourceContext<'_>) -> jqf_engine::CompiledProgram {
    try_compile_program(
        program,
        CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly),
        CompileOptions::new(),
        resources,
    )
    .expect("program compiles")
}

fn raw_nul_policy() -> PipelinePolicy<'static> {
    let options: &'static jqf_codec_json::JsonEncodeOptions = Box::leak(Box::new(jqf_codec_json::JsonEncodeOptions {
        indent: jqf_codec_json::JsonIndent::Compact,
        raw_strings: true,
        sort_keys: false,
        ascii_output: false,
        raw_output_nul: true,
    }));
    let dialect: &'static DialectId = Box::leak(Box::new(DialectId::try_new("rfc8259").expect("dialect")));
    PipelinePolicy {
        decode: DecodeRequest {
            validation: ValidationMode::Strict,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            dialect,
            options: None,
            allow_adjacent_values: true,
            value_separator: jqf_codec_json::VALUE_SEPARATORS,
        },
        encode_diagnostics: DiagnosticPolicy::ErrorsOnly,
        preservation: PreservationRequest::None,
        encode_options: Some(options as &(dyn Any + Send + Sync)),
        cooperative_credits: COOPERATIVE_CREDITS,
        split: None,

        max_iterations: None,
    }
}

#[test]
fn trailing_codec_failure_is_not_a_success() {
    // Whole-input `--stream` with a trailing per-event encode failure must
    // return the codec class. Pre-fix the Codec arm never set last_error,
    // so the terminal match saw None and reported success.
    let registration = jqf_codec_json::registration().expect("json registration is valid");
    let registrations = [&registration];
    let catalog = CodecCatalog::new(&registrations);
    let mut resources = resources();
    let compiled = compile(r#""a\u0000b""#, &resources);
    let input = b"null";
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "test.json",
        input,
        0,
    );
    let mut sink = CollectingSink::new();
    let request = Request::new(&compiled, Input::Whole(input))
        .with_catalog(catalog)
        .with_source(source)
        .with_format(format(), dialect())
        .with_output_format(format(), dialect())
        .with_policy(raw_nul_policy())
        .with_framing(FacadeFraming::item_suffix(b"\0"))
        .with_resources(&mut resources)
        .as_stream_events(false);
    let error = match jqf_sdk::execute(request, &mut sink) {
        Ok(outcome) => panic!("trailing codec failure must not succeed: {outcome:?}"),
        Err(error) => error,
    };
    match error {
        Failure::Pipeline(error) => match error.failure() {
            PipelineFailure::Codec(codec) => {
                assert_eq!(codec.kind(), CodecFailureKind::RawNulByte);
            }
            other => panic!("expected codec failure, got {other:?}"),
        },
        other => panic!("expected pipeline failure, got {other:?}"),
    }
}

#[test]
fn streaming_events_refuse_null_input() {
    let registration = jqf_codec_json::registration().expect("json registration is valid");
    let registrations = [&registration];
    let catalog = CodecCatalog::new(&registrations);
    let mut resources = resources();
    let compiled = compile(".", &resources);
    let source = ResolvedSource::new(SourceRef::new(SourceId::new(1), SourceKind::Input), "test.json", b"", 0);
    let mut sink = CollectingSink::new();
    let request = Request::new(&compiled, Input::Streaming(Box::new(|_buf| Ok::<_, ReadFailure>(0))))
        .with_catalog(catalog)
        .with_source(source)
        .with_format(format(), dialect())
        .with_output_format(format(), dialect())
        .with_resources(&mut resources)
        .as_stream_events(false)
        .with_null_input();
    match jqf_sdk::execute(request, &mut sink) {
        Err(Failure::Request(RequestError::IncompatibleStreamEventsOption("null_input"))) => {}
        other => panic!("expected typed refusal, got {other:?}"),
    }
}

#[test]
fn streaming_events_refuse_slurp() {
    let registration = jqf_codec_json::registration().expect("json registration is valid");
    let registrations = [&registration];
    let catalog = CodecCatalog::new(&registrations);
    let mut resources = resources();
    let compiled = compile(".", &resources);
    let source = ResolvedSource::new(SourceRef::new(SourceId::new(1), SourceKind::Input), "test.json", b"", 0);
    let mut sink = CollectingSink::new();
    let request = Request::new(&compiled, Input::Streaming(Box::new(|_buf| Ok::<_, ReadFailure>(0))))
        .with_catalog(catalog)
        .with_source(source)
        .with_format(format(), dialect())
        .with_output_format(format(), dialect())
        .with_resources(&mut resources)
        .as_stream_events(false)
        .slurped();
    match jqf_sdk::execute(request, &mut sink) {
        Err(Failure::Request(RequestError::IncompatibleStreamEventsOption("slurped"))) => {}
        other => panic!("expected typed refusal, got {other:?}"),
    }
}
