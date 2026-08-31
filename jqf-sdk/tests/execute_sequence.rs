//! F11: `jqf-sdk` had no tests of its own. These exercise
//! `execute_sequence`'s adjacent-value decode loop end to end, through the
//! public SDK surface with the real JSON codec (`jqf-codec-json`), the same
//! way `jqf-cli` drives it for NDJSON/space-separated stdin streams.

/// A process-lifetime built-in dialect for request construction (123 X5).
fn json_dialect() -> &'static DialectId {
    Box::leak(Box::new(DialectId::try_new("rfc8259").expect("dialect")))
}

use jqf_codec_core::{DecodeRequest, DiagnosticPolicy, PreservationRequest, ValidationMode};
use jqf_data::{DialectId, FormatId};
use jqf_engine::{CodecRequirementPolicy, CompileOptions, try_compile_program};
use jqf_resource::{ContinueControl, MemoryCategory, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_sdk::{
    CodecCatalog, EncodedItemReport, FacadeFraming, Input, ItemSink, Outcome, PipelineFailure, PipelinePolicy,
    PublicationStatus, Report, Request, RuntimeMismatchClass, SequenceReport, SequenceValueError,
};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

const COOPERATIVE_CREDITS: u32 = 64;
static CONTROL: ContinueControl = ContinueControl;

/// Collects every published byte and counts item boundaries, without
/// truncating or rejecting any write (the loop-semantics tests here do not
/// need a sink that exercises partial-write or fault behavior).
struct CollectingSink {
    bytes: Vec<u8>,
    items: usize,
    reported: Vec<SequenceValueError>,
}

impl CollectingSink {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            items: 0,
            reported: Vec::new(),
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

    fn report_value_error(&mut self, error: SequenceValueError) -> Result<(), Self::Error> {
        self.reported.push(error);
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

/// Runs `execute_sequence` over `input` against the strict-JSON RFC 8259 route,
/// compiling `program_source` into the compiled program and its derived
/// requirement against the same `ResourceContext` execution runs under (the
/// program, its requirement, and the execution it authorizes share one account),
/// writing every published byte into `sink`. `sink` is populated with whatever
/// the loop wrote before any failure, so callers can inspect partial output on
/// both `Ok` and `Err`. Driving the real compile path exercises exactly what the
/// CLI does, including the program's `?` flags in the run interpretation.
fn run_sequence_with(
    input: &[u8],
    sink: &mut CollectingSink,
    program_source: &str,
) -> Result<SequenceReport, jqf_sdk::PipelineError<String>> {
    run_sequence_reporting_usage(input, sink, program_source, jqf_codec_json::VALUE_SEPARATORS).0
}

/// Extracts the pipeline error from a failure. The other failure classes
/// indicate test misuse: these tests never fail a streaming read and never
/// build an invalid request.
fn pipeline_only(error: jqf_sdk::Failure) -> jqf_sdk::PipelineError<String> {
    match error {
        jqf_sdk::Failure::Pipeline(error) => error,
        other => panic!("unexpected failure class: {other:?}"),
    }
}

/// Same run, additionally returning the ledger snapshot taken after the whole
/// sequence completed. The adjacent-value residency tests read it to prove that
/// a long sequence's charge is bounded by one value's working set plus the
/// recycled session's retained capacity, never by the number of values.
fn run_sequence_reporting_usage(
    input: &[u8],
    sink: &mut CollectingSink,
    program_source: &str,
    separator: &'static [u8],
) -> (
    Result<SequenceReport, jqf_sdk::PipelineError<String>>,
    jqf_resource::UsageSnapshot,
) {
    let mut resources = resources();
    // The run lives in a block so the Request — whose `&mut resources` borrow
    // is pinned to the program borrow's lifetime — drops before the snapshot.
    let result = {
        let registration = jqf_codec_json::registration().expect("json registration is valid");
        let registrations = [&registration];
        let catalog = CodecCatalog::new(&registrations);
        let format = || FormatId::try_new(jqf_codec_json::FORMAT_ID).expect("format id is valid");
        let dialect = || DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect id is valid");
        let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
        let compiled =
            try_compile_program(program_source, policy, CompileOptions::new(), &resources).expect("program compiles");
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
                    value_separator: separator,
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
        let outcome = jqf_sdk::execute(request, sink);
        match outcome {
            Ok(Outcome::Served(Report::Sequence(report))) => Ok(report),
            Ok(Outcome::Served(other)) => panic!("unexpected drive report: {other:?}"),
            Ok(Outcome::Declined) => panic!("the sequence drive must not decline"),
            Err(error) => Err(pipeline_only(error)),
        }
    };
    (result, resources.snapshot())
}

/// Same run against a CALLER-OWNED resource context, for tests that compare
/// ledger snapshots across differently sized inputs under one accounting
/// vocabulary.
fn run_sequence_under(
    input: &[u8],
    sink: &mut CollectingSink,
    program_source: &str,
    resources: &mut ResourceContext<'static>,
) -> Result<SequenceReport, jqf_sdk::PipelineError<String>> {
    let registration = jqf_codec_json::registration().expect("json registration is valid");
    let registrations = [&registration];
    let catalog = CodecCatalog::new(&registrations);
    let format = || FormatId::try_new(jqf_codec_json::FORMAT_ID).expect("format id is valid");
    let dialect = || DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect id is valid");
    let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    let compiled =
        try_compile_program(program_source, policy, CompileOptions::new(), resources).expect("program compiles");
    let requirement = compiled.try_requirement(resources).expect("requirement lowers");
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
        .with_resources(resources)
        .with_requirement(&requirement);
    let outcome = jqf_sdk::execute(request, sink).map_err(pipeline_only)?;
    match outcome {
        Outcome::Served(Report::Sequence(report)) => Ok(report),
        Outcome::Served(other) => panic!("unexpected drive report: {other:?}"),
        Outcome::Declined => panic!("the sequence drive must not decline"),
    }
}

/// Runs over the forced-floor identity `[.][0] | .`. Bare `.` on one
/// canonical document now takes the echo (`Report::Pipeline`); this file
/// pins the adjacent-value sequence drive, so the floor spelling is the
/// helper's program.
fn run_sequence(input: &[u8], sink: &mut CollectingSink) -> Result<SequenceReport, jqf_sdk::PipelineError<String>> {
    run_sequence_with(input, sink, "[.][0] | .")
}

/// Same run under a caller-supplied value-separator set (the CBOR-shaped
/// case: a codec whose alphabet overlaps RFC 8259's whitespace declares an
/// empty set so the drive leaves every byte for its decoder).
fn run_sequence_with_separator(
    input: &[u8],
    sink: &mut CollectingSink,
    program_source: &str,
    separator: &'static [u8],
) -> Result<SequenceReport, jqf_sdk::PipelineError<String>> {
    run_sequence_reporting_usage(input, sink, program_source, separator).0
}

#[test]
fn single_value_emits_exactly_one_item() {
    let mut sink = CollectingSink::new();
    let report = run_sequence(b"42", &mut sink).expect("single value decodes");
    assert_eq!(report.items(), 1);
    assert_eq!(sink.bytes, b"42\n");
    assert_eq!(sink.items, 1);
}

#[test]
fn two_values_separated_by_whitespace_emit_two_items_in_order() {
    let mut sink = CollectingSink::new();
    let report = run_sequence(b"1 2", &mut sink).expect("space-separated values decode");
    assert_eq!(report.items(), 2);
    assert_eq!(sink.bytes, b"1\n2\n");
}

#[test]
fn ndjson_newline_separated_values_emit_one_item_per_line() {
    let mut sink = CollectingSink::new();
    let report = run_sequence(b"1\n2\n3\n", &mut sink).expect("NDJSON stream decodes");
    assert_eq!(report.items(), 3);
    assert_eq!(sink.bytes, b"1\n2\n3\n");
}

#[test]
fn empty_input_emits_no_items_and_succeeds() {
    let mut sink = CollectingSink::new();
    let report = run_sequence(b"", &mut sink).expect("empty input is a zero-item success");
    assert_eq!(report.items(), 0);
    assert!(sink.bytes.is_empty());
    assert_eq!(sink.items, 0);
}

#[test]
fn whitespace_only_input_emits_no_items_and_succeeds() {
    let mut sink = CollectingSink::new();
    let report = run_sequence(b"   \n\t  ", &mut sink).expect("pure separator input is a zero-item success");
    assert_eq!(report.items(), 0);
    assert!(sink.bytes.is_empty());
}

/// The value-separator set is a decode-policy field, not a drive constant:
/// a codec whose alphabet overlaps RFC 8259's whitespace (CBOR, where `0x20`
/// is the complete item −1) declares an empty set so the drive never eats a
/// byte that begins an item. Over a lone space byte the default policy skips
/// it and publishes zero items; the empty set leaves the byte for the
/// decoder, which must reject it — the cursor was not advanced for it.
#[test]
fn empty_value_separator_set_does_not_advance_the_cursor() {
    let mut sink = CollectingSink::new();
    let report = run_sequence(b"\x20", &mut sink).expect("the default separator set skips the space byte");
    assert_eq!(report.items(), 0);
    assert!(sink.bytes.is_empty());

    let mut sink = CollectingSink::new();
    let error = run_sequence_with_separator(b"\x20", &mut sink, ".", &[])
        .expect_err("a separator byte left for the decoder must fail the decode");
    assert!(matches!(error.failure(), PipelineFailure::Codec(_)));
    assert_eq!(sink.items, 0);
    assert!(sink.bytes.is_empty());
}

#[test]
fn trailing_whitespace_after_the_last_value_terminates_cleanly() {
    let mut sink = CollectingSink::new();
    let report = run_sequence(b"1   \n\t  ", &mut sink)
        .expect("trailing separator after the last value must not be treated as another value");
    assert_eq!(report.items(), 1);
    assert_eq!(sink.bytes, b"1\n");
}

#[test]
fn malformed_second_value_still_publishes_the_first_item_and_surfaces_the_error() {
    let mut sink = CollectingSink::new();
    let mut resources = resources();
    let live_account_baseline = resources.snapshot();
    let error = run_sequence_under(b"1 garbage", &mut sink, ".", &mut resources)
        .expect_err("a malformed second value must fail the whole request");
    // The first value was already fully encoded and published to the sink
    // before the loop advanced to the malformed second value: partial
    // progress is not rolled back on a later item's failure.
    assert_eq!(sink.bytes, b"1\n");
    assert_eq!(sink.items, 1);
    assert!(matches!(error.failure(), PipelineFailure::Codec(_)));
    drop(error);
    assert_eq!(
        resources.snapshot().memory_current_bytes(),
        live_account_baseline.memory_current_bytes(),
        "the failed middle value must release its builder, parser, and shared-schema owners"
    );
}

/// Documents `execute_sequence`'s `TypeMismatch` behavior for a mixed-type
/// adjacent stream: a static-path step that resolves to the wrong semantic
/// category on a later item aborts the whole request immediately (mirroring
/// the malformed-syntax case above) rather than skipping just that item.
/// Output already published for earlier items is retained; no further items
/// in the stream are decoded, even syntactically valid ones after the
/// mismatch. This matches Stage 7's parse-error-class scope: an adjacent
/// stream, like a single document, is one request that fails as a whole once
/// any one item cannot be produced.
/// Indexing into a null value is not a type mismatch: `null | .a` yields null
/// and the stream continues, exactly as the reference does. The whole stream of nulls is
/// published, one null per item, and the request succeeds.
#[test]
fn null_indexed_by_key_yields_null_and_continues() {
    let mut sink = CollectingSink::new();
    let report =
        run_sequence_with(b"null null null", &mut sink, ".a").expect("indexing into null yields null, not an error");
    assert_eq!(report.items(), 3);
    assert_eq!(sink.bytes, b"null\nnull\nnull\n");
    assert_eq!(sink.items, 3);
}

/// A multi-step path that reaches a null partway (`{"a":null} | .a.b`) also
/// yields null rather than erroring on the deeper step.
#[test]
fn deep_path_reaching_null_yields_null() {
    let mut sink = CollectingSink::new();
    let report =
        run_sequence_with(br#"{"a":null}"#, &mut sink, ".a.b").expect("a path that reaches null resolves to null");
    assert_eq!(report.items(), 1);
    assert_eq!(sink.bytes, b"null\n");
}

/// `null` indexes to null and publishes it; the bare number `5` is a genuine
/// non-null mismatch. Under continue-on-error it is reported, but as the LAST
/// value its class is the sequence's exit class → `Err` (the reference: `null`, exit 5).
#[test]
fn null_index_continues_but_a_trailing_non_null_mismatch_sets_the_error_class() {
    let mut sink = CollectingSink::new();
    let error = run_sequence_with(b"null 5", &mut sink, ".a")
        .expect_err("the trailing non-null mismatch is the sequence's exit class");
    assert_eq!(sink.bytes, b"null\n");
    assert_eq!(sink.items, 1);
    assert_eq!(sink.reported.len(), 1);
    assert!(matches!(
        error.failure(),
        PipelineFailure::TypeMismatch { step_index: 0, .. }
    ));
}

/// D4 continue-on-error: a per-value RUNTIME type mismatch is reported and the
/// sequence continues to the next adjacent value; the exit class is the LAST
/// value's class. `{"a":1} 5 {"a":2}` with `.a` prints `1` and `2` (the bare
/// number's `.a` mismatch is reported and skipped) and exits 0 because the last
/// value succeeded — byte-identical to the reference. This flips the prior
/// stop-on-error contract, which diverged from the reference; the corpus pins the parity.
#[test]
fn runtime_type_mismatch_mid_stream_is_reported_and_the_sequence_continues() {
    let mut sink = CollectingSink::new();
    let report = run_sequence_with(br#"{"a":1} 5 {"a":2}"#, &mut sink, ".a")
        .expect("a runtime mismatch continues; the last value succeeds so the sequence is Ok");
    assert_eq!(sink.bytes, b"1\n2\n");
    assert_eq!(sink.items, 2);
    // The middle value's mismatch was reported once, at value index 1.
    assert_eq!(sink.reported.len(), 1);
    assert_eq!(sink.reported[0].value_index(), 1);
    assert_eq!(sink.reported[0].class(), RuntimeMismatchClass::Index);
    assert_eq!(report.items(), 2);
}

/// The exit class is the LAST value's class: a trailing runtime failure makes
/// the sequence `Err` even though earlier values published. `[1] 5` with `.[]`
/// prints `1` (the array element) then the bare number's iterate error is the
/// last value's class → exit nonzero (the reference: `1`, exit 5).
#[test]
fn trailing_runtime_failure_sets_the_sequence_error_class() {
    let mut sink = CollectingSink::new();
    let error = run_sequence_with(b"[1] 5", &mut sink, ".[]")
        .expect_err("the last value's iterate error is the sequence's exit class");
    assert_eq!(sink.bytes, b"1\n");
    assert_eq!(sink.items, 1);
    assert_eq!(sink.reported.len(), 1);
    assert_eq!(sink.reported[0].class(), RuntimeMismatchClass::Iterate);
    assert!(matches!(error.failure(), PipelineFailure::IterateMismatch { .. }));
}

/// The third mismatch class end to end: a non-string object construction key.
/// `{"n":5}` with `{(.n): 1}` produces a number key, aborting with the DISTINCT
/// object-key class (the reference: "Cannot use number (5) as object key", exit 5) — both
/// the reported per-value class and the failure variant name it.
#[test]
fn non_string_object_key_is_the_object_key_mismatch_class() {
    let mut sink = CollectingSink::new();
    let error = run_sequence_with(br#"{"n":5}"#, &mut sink, "{(.n): 1}").expect_err("a non-string dynamic key aborts");
    assert!(sink.bytes.is_empty());
    assert_eq!(sink.reported.len(), 1);
    assert_eq!(sink.reported[0].class(), RuntimeMismatchClass::ObjectKey);
    assert!(matches!(
        error.failure(),
        PipelineFailure::ObjectKeyMismatch {
            actual_type: jqf_data::ValueKind::Number
        }
    ));
}

/// A mid-fan-out object-key mismatch publishes the completed prefix then aborts:
/// `{"k":["a",5],"v":1}` with `{(.k[]): .v}` publishes `{"a":1}` then hits the
/// number key (the reference: `{"a":1}`, exit 5).
#[test]
fn object_key_mismatch_publishes_the_completed_prefix_then_aborts() {
    let mut sink = CollectingSink::new();
    let error = run_sequence_with(br#"{"k":["a",5],"v":1}"#, &mut sink, "{(.k[]): .v}")
        .expect_err("the number key aborts after the string key's object publishes");
    assert_eq!(
        sink.bytes,
        br#"{"a":1}"#.iter().chain(b"\n").copied().collect::<Vec<u8>>()
    );
    assert_eq!(sink.reported.len(), 1);
    assert_eq!(sink.reported[0].class(), RuntimeMismatchClass::ObjectKey);
    assert!(matches!(error.failure(), PipelineFailure::ObjectKeyMismatch { .. }));
}

/// The mirror: a leading runtime failure that a later success follows returns
/// `Ok`. `5 [1]` with `.[]` reports the number's iterate error, then the array
/// element `1` publishes and is the last value → exit 0 (the reference: `1`, exit 0).
#[test]
fn leading_runtime_failure_followed_by_success_is_ok() {
    let mut sink = CollectingSink::new();
    let report = run_sequence_with(b"5 [1]", &mut sink, ".[]")
        .expect("the last value succeeds, so the sequence is Ok despite the earlier error");
    assert_eq!(sink.bytes, b"1\n");
    assert_eq!(sink.items, 1);
    assert_eq!(report.items(), 1);
    assert_eq!(sink.reported.len(), 1);
    assert_eq!(sink.reported[0].value_index(), 0);
    assert_eq!(sink.reported[0].class(), RuntimeMismatchClass::Iterate);
}

/// `.[]` fan-out interleaved with a runtime failure: `[1] 5 [3] 7` prints `1`
/// and `3` (each array's single element), reports the two bare numbers' iterate
/// errors, and the last value (`7`) errors → exit nonzero (the reference: `1 3`, exit 5).
#[test]
fn fan_out_over_adjacent_values_continues_past_iterate_errors() {
    let mut sink = CollectingSink::new();
    let error = run_sequence_with(b"[1] 5 [3] 7", &mut sink, ".[]")
        .expect_err("the last value (7) errors, setting the exit class");
    assert_eq!(sink.bytes, b"1\n3\n");
    assert_eq!(sink.items, 2);
    assert_eq!(sink.reported.len(), 2);
    assert!(matches!(error.failure(), PipelineFailure::IterateMismatch { .. }));
}

/// A single value that emits a prefix then aborts mid-fan-out: `[{"b":1},5]`
/// with `.[].b` publishes `1`, then `.b` on the bare number is an index mismatch
/// that tears down the fan-out (the reference: `1`, exit 5). As the only value it is the
/// last, so the sequence is `Err`.
#[test]
fn fan_out_emits_prefix_then_aborts_on_a_deeper_mismatch() {
    let mut sink = CollectingSink::new();
    let error = run_sequence_with(br#"[{"b":1},5]"#, &mut sink, ".[].b")
        .expect_err("a deeper step's mismatch tears down the fan-out after the prefix");
    assert_eq!(sink.bytes, b"1\n");
    assert_eq!(sink.items, 1);
    assert!(matches!(
        error.failure(),
        PipelineFailure::TypeMismatch { step_index: 1, .. }
    ));
}

/// A suppressed per-element mismatch inside a fan-out skips that element and the
/// iteration continues: `[{"b":1},5,{"b":3}]` with `.[].b?` prints `1` and `3`
/// (the bare number's `.b?` is suppressed) at exit 0.
#[test]
fn fan_out_optional_skips_a_suppressed_element_and_continues() {
    let mut sink = CollectingSink::new();
    let report = run_sequence_with(br#"[{"b":1},5,{"b":3}]"#, &mut sink, ".[].b?")
        .expect("a per-element suppression continues the fan-out");
    assert_eq!(sink.bytes, b"1\n3\n");
    assert_eq!(report.items(), 2);
}

/// A construct fan-out whose SECOND field's path fails on a later element:
/// `[{id:1,attrs:{warehouse:"w1"}},{id:2,attrs:"oops"}]` with
/// `.[] | {id: .id, w: .attrs.warehouse}`. The pre-pass must probe EVERY
/// field's full path so the walk declines while nothing is on the sink; a
/// first-key-only check passes the prefix through and then falls back to a
/// floor rerun that republishes element 1 (the duplicate-publish defect).
#[test]
fn construct_fan_out_declines_on_a_deep_path_mismatch_without_republishing() {
    let mut sink = CollectingSink::new();
    let error = run_sequence_with(
        br#"[{"id":1,"attrs":{"warehouse":"w1"}},{"id":2,"attrs":"oops"}]"#,
        &mut sink,
        ".[] | {id: .id, w: .attrs.warehouse}",
    )
    .expect_err("the deep mismatch is the floor's raise");
    // Exactly ONE copy of element 1: the fast path declined before any byte
    // was published, and the floor rerun served the whole program itself.
    assert_eq!(sink.bytes, b"{\"id\":1,\"w\":\"w1\"}\n");
    assert_eq!(sink.items, 1);
    assert!(matches!(error.failure(), PipelineFailure::TypeMismatch { .. }));
}

/// The collected twin of [`construct_fan_out_declines_on_a_deep_path_mismatch_without_republishing`]:
/// nothing reaches the sink (the array is encoded only after a complete walk)
/// and the floor raise is the same type mismatch.
#[test]
fn collected_construct_fan_out_declines_on_a_deep_path_mismatch_without_publishing() {
    let mut sink = CollectingSink::new();
    let error = run_sequence_with(
        br#"[{"id":1,"attrs":{"warehouse":"w1"}},{"id":2,"attrs":"oops"}]"#,
        &mut sink,
        "[.[] | {id: .id, w: .attrs.warehouse}]",
    )
    .expect_err("the deep mismatch is the floor's raise");
    assert!(sink.bytes.is_empty(), "collect publishes nothing on decline");
    assert_eq!(sink.items, 0);
    assert!(matches!(error.failure(), PipelineFailure::TypeMismatch { .. }));
}

/// Missing + zero-items under the residual-flows law: `{}` with `.a[]?` pushes
/// `.a` down (missing → null), the residual `.[]?` iterates that null and
/// suppresses, publishing nothing at exit 0.
#[test]
fn missing_then_optional_iterate_publishes_zero_items() {
    let mut sink = CollectingSink::new();
    let report = run_sequence_with(b"{}", &mut sink, ".a[]?")
        .expect("a suppressed iterate over a pushed-down null is a zero-item success");
    assert_eq!(report.items(), 0);
    assert!(sink.bytes.is_empty());
    assert_eq!(report.publication(), PublicationStatus::NotStarted);
}

/// Missing + abort under the residual-flows law: `{}` with `.a[]` pushes `.a`
/// down (missing → null), the residual `.[]` iterates that null and errors
/// (the reference: "Cannot iterate over null", exit 5). As the only value it is the last,
/// so the sequence is `Err` with the distinct iterate class.
#[test]
fn missing_then_iterate_aborts_with_the_iterate_class() {
    let mut sink = CollectingSink::new();
    let error = run_sequence_with(b"{}", &mut sink, ".a[]").expect_err("iterating a pushed-down null errors");
    assert!(sink.bytes.is_empty());
    assert_eq!(sink.reported.len(), 1);
    assert_eq!(sink.reported[0].class(), RuntimeMismatchClass::Iterate);
    assert!(matches!(error.failure(), PipelineFailure::IterateMismatch { .. }));
}

/// `.a[].b`-shaped fan-out over an NDJSON stream, the prefix-pushdown headline:
/// each object's `.a` array is iterated and `.b` projected, in order across
/// values.
#[test]
fn prefixed_fan_out_projects_children_across_the_stream() {
    let mut sink = CollectingSink::new();
    let report = run_sequence_with(br#"{"a":[{"b":1},{"b":2}]} {"a":[{"b":3}]}"#, &mut sink, ".a[].b")
        .expect("prefixed fan-out projects each child");
    assert_eq!(sink.bytes, b"1\n2\n3\n");
    assert_eq!(report.items(), 3);
}

/// The optional `?` variant of the mismatch-mid-stream case: `.a?` suppresses
/// the bare number `5`'s type mismatch, publishing nothing for it and
/// continuing to the next adjacent value (the reference's `{"a":1} 5 {"a":3}` with `.a?`
/// prints `1` then `3`). This is the new zero-output-continues cardinality.
#[test]
fn optional_suffix_suppresses_mismatch_and_continues_the_sequence() {
    let mut sink = CollectingSink::new();
    let report = run_sequence_with(br#"{"a":1} 5 {"a":3}"#, &mut sink, ".a?")
        .expect("a suppressed mismatch must not fail the sequence");
    assert_eq!(report.items(), 2, "only the two objects publish; 5 suppresses");
    assert_eq!(sink.bytes, b"1\n3\n");
    assert_eq!(sink.items, 2);
}

/// When every value in the source suppresses, the sequence publishes nothing
/// and succeeds with `NotStarted` publication (existing Publication semantics):
/// `5 5` with `.a?` prints nothing, exit 0.
#[test]
fn all_suppressed_source_publishes_nothing_and_succeeds() {
    let mut sink = CollectingSink::new();
    let report = run_sequence_with(b"5 5", &mut sink, ".a?").expect("an all-suppressed source is a zero-item success");
    assert_eq!(report.items(), 0);
    assert!(sink.bytes.is_empty());
    assert_eq!(sink.items, 0);
    assert_eq!(report.publication(), PublicationStatus::NotStarted);
}

// Recycled adjacent-value access session (`ReusableAccessSession`).
//
// The sequence loop now resets the previous value's session in place instead of
// constructing one per value. Two laws hold that reuse: a value that failed
// must leave the recycled session exactly as clean as a fresh one
// (poisoned-session law), and the recycled capacity must stay bounded by ONE
// session plus one value's working set, never by the value count (ledger law).

/// A per-value RUNTIME failure recycles into the next value untouched: the
/// mismatch value publishes nothing, and every later value decodes and
/// publishes exactly as it would from a fresh session.
#[test]
fn recycled_session_survives_a_runtime_failure_mid_sequence() {
    let mut sink = CollectingSink::new();
    let report = run_sequence_with(
        br#"{"a":1}
5
{"a":2}
{"a":3}"#,
        &mut sink,
        ".a",
    )
    .expect("a recovered trailing value makes the sequence succeed");
    assert_eq!(report.items(), 3);
    assert_eq!(sink.bytes, b"1\n2\n3\n");
    assert_eq!(sink.reported.len(), 1);
    assert_eq!(sink.reported[0].class(), RuntimeMismatchClass::Index);
    assert_eq!(sink.reported[0].value_index(), 1);
}

/// A value whose exact path is MISSING leaves the locate phase in a different
/// terminal state than a located hit. The next value must still locate.
#[test]
fn recycled_session_survives_a_missing_path_mid_sequence() {
    let mut sink = CollectingSink::new();
    let report = run_sequence_with(
        br#"{"a":1}
{"b":2}
{"a":3}"#,
        &mut sink,
        ".a",
    )
    .expect("a missing member forwards null and continues");
    assert_eq!(report.items(), 3);
    assert_eq!(sink.bytes, b"1\nnull\n3\n");
    assert!(sink.reported.is_empty());
}

/// A deeply nested value grows the recycled frame stack; the shallow value that
/// follows must not observe any of those frames.
#[test]
fn recycled_session_resets_frames_between_deep_and_shallow_values() {
    let deep = b"{\"a\":[[[[[[[[1]]]]]]]]}\n{\"a\":2}\n{\"a\":[3]}";
    let mut sink = CollectingSink::new();
    let report = run_sequence_with(deep, &mut sink, ".a").expect("mixed depths decode");
    assert_eq!(report.items(), 3);
    assert_eq!(sink.bytes, b"[[[[[[[[1]]]]]]]]\n2\n[3]\n");
}

/// A recycled session must publish bytes IDENTICAL to what one fresh session
/// per value publishes. Running each value as its own single-value sequence is
/// the fresh-session oracle for the same bytes.
#[test]
fn recycled_sequence_bytes_match_one_fresh_session_per_value() {
    let values: [&[u8]; 6] = [
        br#"{"a":{"b":[1,2,3]}}"#,
        // An escaped string forces the copy/escape path, a different
        // materializer shape than the zero-copy span path around it.
        br#"{"a":"\u00e9\n\t"}"#,
        br#"{"a":null}"#,
        br#"{"a":-0.5e3}"#,
        br#"{"a":{}}"#,
        br#"{"a":[]}"#,
    ];
    let mut fresh = Vec::new();
    for value in values {
        let mut sink = CollectingSink::new();
        run_sequence_with(value, &mut sink, ".a").expect("single value decodes");
        fresh.extend_from_slice(&sink.bytes);
    }
    let joined = values.join(&b'\n');
    let mut sink = CollectingSink::new();
    let report = run_sequence_with(&joined, &mut sink, ".a").expect("adjacent values decode");
    assert_eq!(report.items(), values.len() as u64);
    assert_eq!(sink.bytes, fresh);
}

/// Residency: the recycled session is retained capacity, not an accumulating
/// charge. A sequence ten times as long must not raise the ledger's peak
/// committed bytes — the charge is bounded by the largest simultaneously live
/// value, never by the number of values decoded through the recycled slot.
#[test]
fn recycled_session_charge_is_bounded_by_one_value_not_the_value_count() {
    // Every value is byte-identical, so the largest simultaneously live value
    // is the same in both runs and only the COUNT differs: any growth in the
    // ledger peak between them is charge the sequence failed to release.
    fn peak_for(values: usize) -> u64 {
        let mut input = Vec::new();
        for _ in 0..values {
            input.extend_from_slice(b"{\"a\":1234,\"b\":\"xyzzy\"}\n");
        }
        let mut sink = CollectingSink::new();
        let (report, usage) = run_sequence_reporting_usage(&input, &mut sink, ".a", jqf_codec_json::VALUE_SEPARATORS);
        assert_eq!(report.expect("sequence decodes").items(), values as u64);
        usage.memory_peak_bytes()
    }
    let short = peak_for(40);
    let long = peak_for(400);
    assert!(
        long <= short,
        "a 10x longer sequence raised the ledger peak from {short} to {long}: the recycled \
         session is accumulating charge per value instead of retaining one workspace"
    );
}

#[test]
fn adjacent_schema_residency_is_bounded_after_one_large_then_many_tiny_records() {
    fn usage_for(tiny_records: usize) -> (jqf_resource::UsageSnapshot, jqf_resource::UsageSnapshot) {
        let payload = "x".repeat(128 * 1024);
        let mut input = format!("{{\"v\":\"{payload}\"}}\n").into_bytes();
        for _ in 0..tiny_records {
            input.extend_from_slice(b"{\"v\":1}\n");
        }
        let mut sink = CollectingSink::new();
        let mut resources = resources();
        let baseline = resources.snapshot();
        let report = run_sequence_under(&input, &mut sink, ".v", &mut resources);
        assert_eq!(
            report.expect("large and tiny records decode").items(),
            tiny_records as u64 + 1
        );
        (baseline, resources.snapshot())
    }

    let (short_baseline, short) = usage_for(40);
    let (long_baseline, long) = usage_for(4_000);
    assert_eq!(
        short_baseline.memory_current_bytes(),
        long_baseline.memory_current_bytes()
    );
    assert!(
        long.memory_peak_bytes() <= short.memory_peak_bytes(),
        "100x more tiny records raised peak request memory from {} to {}",
        short.memory_peak_bytes(),
        long.memory_peak_bytes()
    );
    assert_eq!(
        long.memory_current_bytes(),
        long_baseline.memory_current_bytes(),
        "provider, parser shell, shared schema, builders, and products must release after \
         execution, leaving only the request ledger's own live baseline \
         (retained={}, working={}, diagnostic={}, pending_io={})",
        long.memory(MemoryCategory::Retained).current(),
        long.memory(MemoryCategory::Working).current(),
        long.memory(MemoryCategory::Diagnostic).current(),
        long.memory(MemoryCategory::PendingIo).current(),
    );
}

#[test]
fn duplicate_object_member_transfers_only_the_last_value_under_the_first_position() {
    let mut input = String::from("{\"large\":[");
    for index in 0..2_048 {
        if index != 0 {
            input.push(',');
        }
        input.push_str("null");
    }
    input.push_str("],\"small\":1}");
    let mut sink = CollectingSink::new();

    let report = run_sequence_with(input.as_bytes(), &mut sink, "{a:.large,a:.small}")
        .expect("the overwritten large member is not transferred into the finished object");
    assert_eq!(report.items(), 1);
    assert_eq!(sink.bytes, b"{\"a\":1}\n");
}

// Recycled ordered-emission encoder (`ReusableEncoderSession`).
//
// The publication path now restarts one retained encoder — its frame stack and
// its 16 KiB output staging buffer — per ordered item instead of allocating and
// dropping a complete encoder per emission. The same two laws hold it: the
// reset must leave nothing of the previous item behind, and the retained
// capacity must stay bounded by one encoder.

/// A large emission followed by a small one: the recycled staging buffer must
/// publish exactly the small item's bytes, with nothing left over from the
/// large one.
#[test]
fn recycled_encoder_resets_staged_bytes_between_items() {
    let mut wide = Vec::from(*b"{\"a\":[");
    for index in 0..2_000 {
        if index != 0 {
            wide.push(b',');
        }
        wide.extend_from_slice(format!("{index}").as_bytes());
    }
    wide.extend_from_slice(b"]}");
    let mut expected = Vec::new();
    {
        let mut sink = CollectingSink::new();
        run_sequence_with(&wide, &mut sink, ".a").expect("wide value decodes");
        expected.extend_from_slice(&sink.bytes);
    }
    expected.extend_from_slice(b"7\n");
    expected.extend_from_slice(b"\"tail\"\n");

    let mut input = wide.clone();
    input.extend_from_slice(b"\n{\"a\":7}\n{\"a\":\"tail\"}");
    let mut sink = CollectingSink::new();
    let report = run_sequence_with(&input, &mut sink, ".a").expect("mixed sizes decode");
    assert_eq!(report.items(), 3);
    assert_eq!(sink.bytes, expected);
}

/// The reverse order: a small emission must not shrink or truncate the wide one
/// that follows it through the same recycled buffer.
#[test]
fn recycled_encoder_grows_back_after_a_small_item() {
    let mut wide = Vec::from(*b"{\"a\":[");
    for index in 0..2_000 {
        if index != 0 {
            wide.push(b',');
        }
        wide.extend_from_slice(format!("\"v{index}\"").as_bytes());
    }
    wide.extend_from_slice(b"]}");
    let mut expected = Vec::from(*b"0\n");
    {
        let mut sink = CollectingSink::new();
        run_sequence_with(&wide, &mut sink, ".a").expect("wide value decodes");
        expected.extend_from_slice(&sink.bytes);
    }
    let mut input = Vec::from(*b"{\"a\":0}\n");
    input.extend_from_slice(&wide);
    let mut sink = CollectingSink::new();
    let report = run_sequence_with(&input, &mut sink, ".a").expect("small then wide decodes");
    assert_eq!(report.items(), 2);
    assert_eq!(sink.bytes, expected);
}

/// Residency: many emissions out of ONE document (fan-out) must not raise the
/// ledger peak with the emission COUNT — the recycled encoder is one retained
/// capacity, not one per published item.
///
/// The document itself is held constant and only the number of PUBLISHED items
/// varies (`.a` publishes the array once, `.a[]` publishes every element), so
/// the difference between the two peaks is exactly what publication costs. A
/// per-item encoder would add its 16 KiB staging buffer per element.
#[test]
fn recycled_encoder_charge_is_bounded_by_one_item_not_the_item_count() {
    const ELEMENTS: usize = 2_000;
    let mut input = Vec::from(*b"{\"a\":[");
    for index in 0..ELEMENTS {
        if index != 0 {
            input.push(b',');
        }
        input.extend_from_slice(b"\"item\"");
    }
    input.extend_from_slice(b"]}");

    let peak_for = |program: &str, expected_items: u64| {
        let mut sink = CollectingSink::new();
        let (report, usage) =
            run_sequence_reporting_usage(&input, &mut sink, program, jqf_codec_json::VALUE_SEPARATORS);
        assert_eq!(report.expect("document decodes").items(), expected_items);
        usage.memory_peak_bytes()
    };
    let one_item = peak_for(".a", 1);
    let every_element = peak_for(".a[]", ELEMENTS as u64);
    let publication_cost = every_element.saturating_sub(one_item);
    // One recycled encoder is a 16 KiB staging buffer plus a small frame stack;
    // 2000 un-recycled ones would be ~32 MB.
    assert!(
        publication_cost < 1 << 20,
        "publishing {ELEMENTS} items instead of 1 cost {publication_cost} extra peak bytes over \
         the same document: the recycled encoder is accumulating charge per published item"
    );
}

/// Runs the streaming adjacent-value drive over `input` delivered as the two
/// chunks `input[..cut]` and `input[cut..]`, against the identity program, and
/// returns the bytes it published. The two-chunk shape is the read(2) cut a
/// pipe imposes: the first chunk is everything the decoder can see when it is
/// first asked, and the rest arrives only after it answers.
fn run_streaming_chunks(input: &[u8], cut: usize) -> Result<Vec<u8>, jqf_sdk::PipelineError<String>> {
    let registration = jqf_codec_json::registration().expect("json registration is valid");
    let registrations = [&registration];
    let catalog = CodecCatalog::new(&registrations);
    let format = || FormatId::try_new(jqf_codec_json::FORMAT_ID).expect("format id is valid");
    let dialect = || DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect id is valid");
    let mut resources = resources();
    let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    let compiled = try_compile_program(".", policy, CompileOptions::new(), &resources).expect("program compiles");
    let requirement = compiled.try_requirement(&resources).expect("requirement lowers");
    let mut chunks: Vec<Vec<u8>> = vec![input[..cut].to_vec(), input[cut..].to_vec()];
    let mut sink = CollectingSink::new();
    let request = Request::new(
        &compiled,
        Input::Streaming(Box::new(move |buffer| {
            if chunks.is_empty() {
                return Ok(0);
            }
            let chunk = chunks.remove(0);
            let count = chunk.len().min(buffer.len());
            buffer[..count].copy_from_slice(&chunk[..count]);
            Ok(count)
        })),
    )
    .with_catalog(catalog)
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
    let outcome = jqf_sdk::execute(request, &mut sink).map_err(pipeline_only)?;
    match outcome {
        Outcome::Served(Report::Sequence(_)) => {}
        Outcome::Served(other) => panic!("unexpected drive report: {other:?}"),
        Outcome::Declined => panic!("the streaming sequence drive must not decline"),
    }
    Ok(sink.bytes)
}

/// Asserts that `input` publishes the same bytes wherever the read cuts it,
/// against the uncut delivery as the control — the streaming law in one line:
/// the output of a stream is a function of its bytes, never of where read(2)
/// happened to split them.
fn assert_every_cut_matches_the_uncut_stream(input: &[u8]) {
    let whole = run_streaming_chunks(input, input.len())
        .unwrap_or_else(|error| panic!("uncut {input:?} must decode: {error:?}"));
    for cut in 1..input.len() {
        let published = run_streaming_chunks(input, cut)
            .unwrap_or_else(|error| panic!("cut {cut} of {input:?} must decode: {error:?}"));
        assert_eq!(
            published,
            whole,
            "cut {cut} of {input:?} published {} instead of {}",
            String::from_utf8_lossy(&published),
            String::from_utf8_lossy(&whole)
        );
    }
}

/// A pipe delivers a growing source in arbitrary chunks. A chunk that ends in
/// the middle of a literal (`{"a":tr`) used to fail the whole stream as
/// `invalid-literal` because the parser labeled the cut at the token start,
/// which the streaming drive could not recognize as "would complete with more
/// input". The cut must be reported at the window's end so the drive holds and
/// refills.
#[test]
fn streaming_chunks_that_cut_a_literal_still_complete_the_document() {
    let input = br#"{"a":true,"b":1234}"#;
    let published = run_streaming_chunks(input, 8).expect("stream must hold a literal cut and complete");
    assert_eq!(published, b"{\"a\":true,\"b\":1234}\n");
}

/// A bare number is ended by the bytes running out, not by a delimiter, so a
/// read cut inside one used to publish TWO numbers (`1234567` piped as
/// `1234` + `567` emitted `1234` then `567`, exit 0 — the corruption was
/// silent and depended only on where read(2) cut). Every cut must publish the
/// one number the bytes spell.
#[test]
fn a_read_cut_inside_a_bare_number_still_publishes_one_number() {
    for input in [
        b"1234567".as_slice(),
        b"3.14159",
        b"-12345",
        b"15e3",
        b"1e-45",
        b"12345678901234567890",
    ] {
        assert_every_cut_matches_the_uncut_stream(input);
    }
}

/// The same law across an adjacent-value boundary: the numbers stay two, and
/// the whitespace between them stays the only thing that separates them.
#[test]
fn a_read_cut_between_adjacent_numbers_still_publishes_both_whole() {
    assert_every_cut_matches_the_uncut_stream(b"12 34");
    assert_every_cut_matches_the_uncut_stream(b"true false");
    assert_every_cut_matches_the_uncut_stream(b"1 2 3");
}

/// A multi-byte UTF-8 scalar the read cut splits is valid input whose
/// remaining bytes have not arrived. It used to be rejected `json.invalid-utf8`
/// because the validator ran as if the window were the whole input and labeled
/// the truncation at the sequence's LEAD — one to three bytes before the cut,
/// where the drive's hold check does not look.
#[test]
fn a_read_cut_inside_a_multibyte_scalar_still_decodes_the_string() {
    for input in [
        "\"abécd\"",
        "\"ab€cd\"",
        "\"ab\u{1d11e}cd\"",
        "[\"€\",1]",
        "{\"kéy\":\"v\u{1d11e}\"}",
    ] {
        assert_every_cut_matches_the_uncut_stream(input.as_bytes());
    }
}

#[test]
fn a_small_held_value_publishes_on_the_completing_read() {
    // The doubling gate applies only above one STREAMING_READ_CHUNK. A
    // five-byte hold (`{"a":`) plus the completing `1}` must publish on
    // that refill, not wait for the window to double or for EOF.
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    let published = Rc::new(RefCell::new(Vec::<u8>::new()));
    let published_for_read = Rc::clone(&published);
    let mut chunks: Vec<&[u8]> = vec![b"{\"a\":", b"1}"];
    let saw_publish_before_eof = Rc::new(Cell::new(false));
    let saw_for_read = Rc::clone(&saw_publish_before_eof);
    let registration = jqf_codec_json::registration().expect("json registration is valid");
    let registrations = [&registration];
    let catalog = CodecCatalog::new(&registrations);
    let format = || FormatId::try_new(jqf_codec_json::FORMAT_ID).expect("format id is valid");
    let dialect = || DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect id is valid");
    let mut resources = resources();
    let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    let compiled = try_compile_program(".", policy, CompileOptions::new(), &resources).expect("program compiles");
    let requirement = compiled.try_requirement(&resources).expect("requirement lowers");
    let mut sink = SharedSink {
        bytes: Rc::clone(&published),
    };
    let request = Request::new(
        &compiled,
        Input::Streaming(Box::new(move |buffer| {
            if chunks.is_empty() {
                saw_for_read.set(!published_for_read.borrow().is_empty());
                return Ok(0);
            }
            let chunk = chunks.remove(0);
            let count = chunk.len().min(buffer.len());
            buffer[..count].copy_from_slice(&chunk[..count]);
            Ok(count)
        })),
    )
    .with_catalog(catalog)
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
    jqf_sdk::execute(request, &mut sink).expect("small held value completes");
    assert!(
        saw_publish_before_eof.get(),
        "a completed small value must publish on the completing read, not at EOF"
    );
    assert_eq!(&*published.borrow(), b"{\"a\":1}\n");
}

struct SharedSink {
    bytes: std::rc::Rc<std::cell::RefCell<Vec<u8>>>,
}

impl ItemSink for SharedSink {
    type Error = &'static str;

    fn begin_item(&mut self, _index: u64) -> Result<(), Self::Error> {
        Ok(())
    }

    fn write(&mut self, bytes: &[u8]) -> Result<usize, Self::Error> {
        self.bytes.borrow_mut().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn finish_item(&mut self, _index: u64, _report: EncodedItemReport) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[test]
fn a_split_destination_collision_refuses_the_second_writer() {
    let registration = jqf_codec_json::registration().expect("json registration is valid");
    let registrations = [&registration];
    let catalog = CodecCatalog::new(&registrations);
    let format = || FormatId::try_new(jqf_codec_json::FORMAT_ID).expect("format id is valid");
    let dialect = || DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect id is valid");
    let mut resources = resources();
    let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    let compiled = try_compile_program(".", policy, CompileOptions::new(), &resources).expect("program compiles");
    let split =
        try_compile_program("\"same.json\"", policy, CompileOptions::split_exp(), &resources).expect("split compiles");
    let requirement = compiled.try_requirement(&resources).expect("requirement lowers");
    let input = b"1\n2\n";
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
            split: Some(&split),

            max_iterations: None,
        })
        .with_framing(FacadeFraming::item_suffix(b"\n"))
        .with_resources(&mut resources)
        .with_requirement(&requirement);
    let error = match jqf_sdk::execute(request, &mut sink) {
        Ok(outcome) => panic!("collision must refuse, got {outcome:?}"),
        Err(error) => error,
    };
    match error.pipeline_failure() {
        Some(PipelineFailure::SplitCollision {
            name,
            first_index,
            second_index,
        }) => {
            assert_eq!(name, "same.json");
            assert_eq!(*first_index, 0);
            assert_eq!(*second_index, 1);
        }
        other => panic!("expected SplitCollision, got {other:?}"),
    }
    assert_eq!(
        sink.bytes, b"1\n",
        "first writer stands; the colliding item is not published"
    );
}

#[test]
fn path_type_names_the_located_node_kind() {
    // PATH | type Exact-locates the prefix; residual type must see the
    // named node (an array here), not the document root object.
    let mut sink = CollectingSink::new();
    let report = run_sequence_with(br#"{"users":[1,2,3]}"#, &mut sink, ".users | type").expect("runs");
    assert_eq!(report.items(), 1);
    assert_eq!(sink.bytes, b"\"array\"\n");
    let mut root = CollectingSink::new();
    run_sequence_with(br#"{"users":[1,2,3]}"#, &mut root, "type").expect("runs");
    assert_eq!(root.bytes, b"\"object\"\n");
}

#[test]
fn empty_path_slice_then_type_raises_on_an_object() {
    // `.[1:3] | type` is not bare-root `type`. An object has no array slice;
    // the residual must raise instead of printing the root kind.
    let mut sink = CollectingSink::new();
    let error = run_sequence_with(br#"{"a":1}"#, &mut sink, ".[1:3] | type").expect_err("slicing an object must raise");
    assert!(
        sink.bytes != b"\"object\"\n",
        "must not type the root: {}",
        String::from_utf8_lossy(&sink.bytes)
    );
    assert!(
        matches!(error.failure(), PipelineFailure::TypeMismatch { .. }),
        "object slice is a type mismatch: {:?}",
        error.failure()
    );
}
