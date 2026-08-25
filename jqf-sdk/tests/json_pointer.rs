//! The `json_pointer` RFC 6901 read laws, end to end through the public SDK
//! surface with the real JSON codec.
//!
//! The compat corpus's `json_pointer` rows pin the `[match]`/`[]` byte law
//! and the corrected leading-zero row, but its kind asserts a successful exit,
//! so the RAISE half of the law is invisible to it: RFC 6901 §4's leading-zero
//! rejection  and the module header's two-armed error split
//!  — an argument-EVALUATION error drops the prefix, a
//! NAVIGATION error keeps the arrays already collected and raises after them.
//! These tests pin the three behaviors the corpus cannot see, through
//! `execute_sequence` exactly as the CLI drives it.

/// A process-lifetime built-in dialect for request construction (123 X5).
fn json_dialect() -> &'static DialectId {
    Box::leak(Box::new(DialectId::try_new("rfc8259").expect("dialect")))
}

use jqf_codec_core::{DecodeRequest, DiagnosticPolicy, PreservationRequest, ValidationMode};
use jqf_data::{DialectId, FormatId};
use jqf_engine::{CodecRequirementPolicy, try_compile_program};
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_sdk::{
    CodecCatalog, EncodedItemReport, FacadeFraming, Input, ItemSink, Outcome, PipelineFailure, PipelinePolicy, Report,
    Request, SequenceReport,
};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

const COOPERATIVE_CREDITS: u32 = 64;
static CONTROL: ContinueControl = ContinueControl;

/// Collects every published byte and counts item boundaries, without
/// truncating or rejecting any write.
struct CollectingSink {
    bytes: Vec<u8>,
    items: usize,
}

impl CollectingSink {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            items: 0,
        }
    }
}

impl ItemSink for CollectingSink {
    type Error = &'static str;

    fn begin_item(&mut self, _index: u64) -> Result<(), Self::Error> {
        self.items += 1;
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

/// Runs `execute_sequence` over `input` against the strict-JSON RFC 8259
/// route, writing every published byte into `sink` so callers can inspect
/// partial output on both `Ok` and `Err`.
fn run_sequence_with(
    input: &[u8],
    sink: &mut CollectingSink,
    program_source: &str,
) -> Result<SequenceReport, jqf_sdk::PipelineError<String>> {
    let registration = jqf_codec_json::registration().expect("json registration is valid");
    let registrations = [&registration];
    let catalog = CodecCatalog::new(&registrations);
    let format = || FormatId::try_new(jqf_codec_json::FORMAT_ID).expect("format id is valid");
    let dialect = || DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect id is valid");
    let mut resources = resources();
    let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    let compiled = try_compile_program(program_source, policy, &resources).expect("program compiles");
    let requirement = compiled.try_requirement(&resources).expect("requirement lowers");
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "test.json",
        input,
        0,
    );
    let request = Request::new(&compiled, Input::Whole(input))
        .with_catalog(catalog)
        .with_source(source)
        .with_format(format(), dialect())
        .with_output_format(format(), dialect())
        .with_policy(PipelinePolicy {
            decode: DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: json_dialect(),
                options: None,
                allow_adjacent_values: true,
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
        .with_requirement(&requirement);
    let outcome = jqf_sdk::execute(request, sink).map_err(pipeline_only)?;
    match outcome {
        Outcome::Served(Report::Sequence(report)) => Ok(report),
        Outcome::Served(other) => panic!("unexpected drive report: {other:?}"),
        Outcome::Declined => panic!("the sequence drive must not decline"),
    }
}

/// Extracts the pipeline error from a failure; the other failure classes
/// indicate test misuse.
fn pipeline_only(error: jqf_sdk::Failure) -> jqf_sdk::PipelineError<String> {
    match error {
        jqf_sdk::Failure::Pipeline(error) => error,
        other => panic!("unexpected failure class: {other:?}"),
    }
}

const CATALOG: &[u8] = br#"{"rows":[{"id":1},{"name":"second"}]}"#;

/// RFC 6901 §4 : a leading-zero array index is a raise, never a
/// navigation. The corpus's `json_pointer` kind cannot host this row (it
/// asserts exit 0), so it is pinned here through the real pipeline.
#[test]
fn leading_zero_array_index_raises_end_to_end() {
    let mut sink = CollectingSink::new();
    let error = run_sequence_with(CATALOG, &mut sink, r#"json_pointer("/rows/01/name")"#)
        .expect_err("a leading-zero index raises");
    assert!(sink.bytes.is_empty());
    assert!(matches!(error.failure(), PipelineFailure::Raised(_)));
}

/// The corpus row's corrected spelling navigates: `/rows/1/name` is the
/// RFC-conforming way to name the second element, and it still answers.
#[test]
fn the_legal_index_navigates() {
    let mut sink = CollectingSink::new();
    let report =
        run_sequence_with(CATALOG, &mut sink, r#"json_pointer("/rows/1/name")"#).expect("a legal index navigates");
    assert_eq!(sink.bytes, b"[\"second\"]\n");
    assert_eq!(sink.items, 1);
    assert_eq!(report.items(), 1);
}

/// The module header's first error arm : an error during ARGUMENT
/// EVALUATION drops the prefix — `json_pointer(1,2,error("x"); "/a")` emits
/// nothing before the raise, because the `?` propagates before any `PathEmit`
/// frame exists.
#[test]
fn an_argument_evaluation_error_drops_the_prefix() {
    let mut sink = CollectingSink::new();
    let error = run_sequence_with(br#"{"a":1}"#, &mut sink, r#"json_pointer(1,2,error("x"); "/a")"#)
        .expect_err("the argument-evaluation error raises");
    assert!(sink.bytes.is_empty());
    assert!(matches!(error.failure(), PipelineFailure::Raised(_)));
}

/// The module header's second error arm : an error during
/// NAVIGATION keeps the arrays already collected and raises after them —
/// `json_pointer(.; "/a", "bad")` publishes `[1]`, then raises, the same
/// ordered-prefix law the engine's own generators keep.
#[test]
fn a_navigation_error_keeps_the_collected_prefix() {
    let mut sink = CollectingSink::new();
    let error = run_sequence_with(br#"{"a":1}"#, &mut sink, r#"json_pointer(.; "/a", "bad")"#)
        .expect_err("the navigation error raises after the prefix");
    assert_eq!(sink.bytes, b"[1]\n");
    assert_eq!(sink.items, 1);
    assert!(matches!(error.failure(), PipelineFailure::Raised(_)));
}
