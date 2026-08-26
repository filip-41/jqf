//! The NDJSON record vertical end to end, through the real SDK record drive.
//!
//! These are the vertical's behaviour tests: framing law, profile law, the
//! recovering exit law, output framing, and the byte-identity duty that ties
//! the explicit record route to the default adjacent-value path. Nothing here
//! reaches into the framer's internals — every assertion is about what a host
//! actually observes.

mod common;

fn req_dialect() -> &'static DialectId {
    Box::leak(Box::new(DialectId::try_new("rfc8259").expect("dialect")))
}

use jqf_codec_core::{
    DecodeRequest, DiagnosticPolicy, PreservationRequest, RecordIssueCode, RecordIssueSeverity, ValidationMode,
};
use jqf_codec_json::ndjson::{NdjsonDecodeOptions, NdjsonEncodeOptions, NdjsonProfile, NdjsonTerminator};
use jqf_data::{DialectId, FormatId};
use jqf_sdk::{
    CodecCatalog, CodecRequirementPolicy, EncodedItemReport, FacadeFraming, ItemSink, PipelinePolicy,
    RecordIssueReport, RecordSequenceReport, SequenceValueError, try_compile_program,
};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

const COOPERATIVE_CREDITS: u32 = 64;
const INPUT_CEILING: u64 = 1 << 20;

#[derive(Debug, Eq, PartialEq)]
struct SeenIssue {
    ordinal: u64,
    offset: u64,
    severity: RecordIssueSeverity,
    code: RecordIssueCode,
}

struct CollectingSink {
    bytes: Vec<u8>,
    items: usize,
    value_errors: Vec<String>,
    issues: Vec<SeenIssue>,
}

impl CollectingSink {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            items: 0,
            value_errors: Vec::new(),
            issues: Vec::new(),
        }
    }

    fn text(&self) -> &str {
        core::str::from_utf8(&self.bytes).expect("published bytes are UTF-8")
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
        self.value_errors.push(error.message().to_owned());
        Ok(())
    }

    fn report_record_issue(&mut self, issue: RecordIssueReport<'_>) -> Result<(), Self::Error> {
        self.issues.push(SeenIssue {
            ordinal: issue.ordinal(),
            offset: issue.offset(),
            severity: issue.severity(),
            code: issue.code(),
        });
        Ok(())
    }
}

/// A sink with a finite byte budget: each write publishes what fits and a
/// write arriving after the budget is spent fails. This is the one
/// deterministic way to kill a record MID-publication at this API level —
/// the failure lands between the failed record's already-published payload
/// bytes and its terminator, whatever order the encoder offers them in.
struct BudgetSink {
    bytes: Vec<u8>,
    budget: usize,
}

impl BudgetSink {
    fn new(budget: usize) -> Self {
        Self {
            bytes: Vec::new(),
            budget,
        }
    }
}

impl ItemSink for BudgetSink {
    type Error = &'static str;

    fn begin_item(&mut self, _index: u64) -> Result<(), Self::Error> {
        Ok(())
    }

    fn write(&mut self, bytes: &[u8]) -> Result<usize, Self::Error> {
        if self.budget == 0 {
            // The sink contract: an Err guarantees no prefix was published.
            return Err("budget exhausted");
        }
        let accepted = bytes.len().min(self.budget);
        self.bytes.extend_from_slice(&bytes[..accepted]);
        self.budget -= accepted;
        Ok(accepted)
    }

    fn finish_item(&mut self, _index: u64, _report: EncodedItemReport) -> Result<(), Self::Error> {
        Ok(())
    }
}

struct RunOptions {
    profile: NdjsonProfile,
    program: &'static str,
    max_record_bytes: Option<u64>,
    ndjson_output: Option<NdjsonTerminator>,
}

impl RunOptions {
    const fn strict(program: &'static str) -> Self {
        Self {
            profile: NdjsonProfile::Strict,
            program,
            max_record_bytes: None,
            ndjson_output: None,
        }
    }

    const fn recovering(program: &'static str) -> Self {
        Self {
            profile: NdjsonProfile::Recovering,
            program,
            max_record_bytes: None,
            ndjson_output: None,
        }
    }
}

type RunResult = Result<RecordSequenceReport, jqf_sdk::PipelineError<String>>;

// Generic over the sink so a test can fail publication in a controlled way
// without forking the whole harness.
#[allow(
    clippy::too_many_lines,
    reason = "one linear harness invocation mirroring the CLI's own record branch"
)]
fn run_records<S: ItemSink<Error = &'static str>>(input: &[u8], sink: &mut S, options: &RunOptions) -> RunResult {
    let json = jqf_codec_json::registration().expect("json registration is valid");
    let streams = jqf_codec_json::ndjson::registration().expect("ndjson registration is valid");
    let registrations = [&json, &streams];
    let catalog = CodecCatalog::new(&registrations);
    let format = FormatId::try_new(jqf_codec_json::FORMAT_ID).expect("format id is valid");
    let dialect = DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect id is valid");
    let (output_format, output_dialect) = match options.ndjson_output {
        None => (
            FormatId::try_new(jqf_codec_json::FORMAT_ID).expect("format id is valid"),
            DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect id is valid"),
        ),
        Some(_) => (
            FormatId::try_new(jqf_codec_json::ndjson::FORMAT_ID).expect("format id is valid"),
            DialectId::try_new(jqf_codec_json::ndjson::STRICT_DIALECT_ID).expect("dialect id is valid"),
        ),
    };
    let mut resources = common::resources();
    let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    let compiled = try_compile_program(options.program, policy, &resources).expect("compiles");
    let requirement = compiled.try_requirement(&resources).expect("requirement lowers");
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "test.ndjson",
        input,
        0,
    );
    let record_options =
        NdjsonDecodeOptions::try_new(options.max_record_bytes, INPUT_CEILING).expect("record ceiling normalizes");
    let records = jqf_codec_json::ndjson::create_record_provider(
        source,
        options.profile,
        record_options,
        DiagnosticPolicy::ErrorsOnly,
        options.profile.validation(),
        &mut resources,
    )
    .expect("record provider opens");
    let encode_options = options.ndjson_output.map(NdjsonEncodeOptions::new);
    let opaque = encode_options
        .as_ref()
        .map(|value| value as &(dyn core::any::Any + Send + Sync));
    let framing = if options.ndjson_output.is_some() {
        FacadeFraming::item_suffix(b"")
    } else {
        FacadeFraming::item_suffix(b"\n")
    };
    let request = jqf_sdk::Request::new(
        &compiled,
        jqf_sdk::Input::Records {
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
            dialect: req_dialect(),
            options: None,
            allow_adjacent_values: false,
            value_separator: jqf_codec_json::VALUE_SEPARATORS,
        },
        encode_diagnostics: DiagnosticPolicy::ErrorsOnly,
        preservation: PreservationRequest::None,
        encode_options: opaque,
        cooperative_credits: COOPERATIVE_CREDITS,
        split: None,

        max_iterations: None,
    })
    .with_framing(framing)
    .with_resources(&mut resources)
    .with_requirement(&requirement);
    match jqf_sdk::execute(request, sink) {
        Ok(jqf_sdk::Outcome::Served(jqf_sdk::Report::Record(report))) => Ok(report),
        Ok(other) => panic!("unexpected outcome: {other:?}"),
        Err(error) => match error {
            jqf_sdk::Failure::Pipeline(error) => Err(error),
            other => panic!("unexpected failure: {other:?}"),
        },
    }
}

/// The default adjacent-value drive over the same bytes, for byte identity.
fn run_adjacent(input: &[u8], sink: &mut CollectingSink, program: &str) {
    let json = jqf_codec_json::registration().expect("json registration is valid");
    let registrations = [&json];
    let catalog = CodecCatalog::new(&registrations);
    let format = || FormatId::try_new(jqf_codec_json::FORMAT_ID).expect("format id is valid");
    let dialect = || DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect id is valid");
    let mut resources = common::resources();
    let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    let compiled = try_compile_program(program, policy, &resources).expect("compiles");
    let requirement = compiled.try_requirement(&resources).expect("requirement lowers");
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "test.json",
        input,
        0,
    );
    let request = jqf_sdk::Request::new(&compiled, jqf_sdk::Input::Whole(input))
        .with_catalog(catalog)
        .with_source(source)
        .with_format(format(), dialect())
        .with_output_format(format(), dialect())
        .with_policy(PipelinePolicy {
            decode: DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: req_dialect(),
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
    match jqf_sdk::execute(request, sink).expect("adjacent sequence completes") {
        jqf_sdk::Outcome::Served(_) => {}
        jqf_sdk::Outcome::Declined => panic!("the sequence drive must not decline"),
    }
}

#[test]
fn a_conforming_stream_is_byte_identical_to_the_adjacent_value_path() {
    // The gate the whole vertical exists to satisfy: on input both paths
    // accept, the explicit record route publishes exactly the default path's
    // bytes. Anything else would make `--input-format ndjson` a different
    // language, not a different framing.
    let input = b"{\"v\":1}\n{\"v\":2}\n{\"v\":[3,4]}\n{\"v\":null}\n";
    let mut records = CollectingSink::new();
    let mut adjacent = CollectingSink::new();
    let report = run_records(input, &mut records, &RunOptions::strict(".v")).expect("completes");
    run_adjacent(input, &mut adjacent, ".v");
    assert_eq!(records.bytes, adjacent.bytes);
    assert_eq!(records.text(), "1\n2\n[3,4]\nnull\n");
    assert_eq!(report.records(), 4);
    assert_eq!(report.issues(), 0);
    assert_eq!(report.error_issues(), 0);
}

#[test]
fn mixed_lf_and_crlf_terminators_are_both_accepted_and_stripped() {
    let input = b"{\"v\":1}\r\n{\"v\":2}\n{\"v\":3}\r\n";
    let mut sink = CollectingSink::new();
    let report = run_records(input, &mut sink, &RunOptions::strict(".v")).expect("completes");
    assert_eq!(sink.text(), "1\n2\n3\n");
    assert_eq!(report.records(), 3);
}

#[test]
fn empty_input_is_a_clean_zero_record_stream() {
    let mut sink = CollectingSink::new();
    let report = run_records(b"", &mut sink, &RunOptions::strict(".v")).expect("completes");
    assert_eq!(sink.bytes, b"");
    assert_eq!(report.records(), 0);
    assert_eq!(report.issues(), 0);
}

#[test]
fn the_empty_suffix_after_a_final_terminator_is_not_a_record() {
    let mut sink = CollectingSink::new();
    let report = run_records(b"{\"v\":1}\n", &mut sink, &RunOptions::strict(".v")).expect("completes");
    assert_eq!(report.records(), 1);
    assert_eq!(report.issues(), 0);
}

#[test]
fn zero_and_many_cardinality_survive_per_record() {
    // A record may emit no results and a record may emit several; item
    // boundaries stay explicit, exactly as the adjacent path keeps them.
    let input = b"{\"v\":[]}\n{\"v\":[1,2,3]}\n";
    let mut sink = CollectingSink::new();
    let report = run_records(input, &mut sink, &RunOptions::strict(".v[]")).expect("completes");
    assert_eq!(sink.text(), "1\n2\n3\n");
    assert_eq!(report.items(), 3);
    assert_eq!(report.records(), 2);
}

#[test]
fn duplicate_keys_keep_the_last_value_within_one_record() {
    let mut sink = CollectingSink::new();
    run_records(b"{\"v\":1,\"v\":2}\n", &mut sink, &RunOptions::strict(".v")).expect("completes");
    assert_eq!(sink.text(), "2\n");
}

#[test]
fn two_values_on_one_physical_line_are_trailing_content_not_two_records() {
    // The single most important difference from the adjacent-value path: a
    // physical record holds exactly ONE text, so a second value on the line is
    // the payload codec's ordinary trailing-content failure, not a free record.
    let mut sink = CollectingSink::new();
    let failure = run_records(b"{\"v\":1} {\"v\":2}\n", &mut sink, &RunOptions::strict(".v"))
        .expect_err("trailing content fails");
    assert!(matches!(failure.failure(), jqf_sdk::PipelineFailure::Codec(_)));
    assert_eq!(sink.bytes, b"");
}

#[test]
fn strict_rejects_a_blank_record_and_keeps_the_earlier_prefix() {
    let mut sink = CollectingSink::new();
    let failure =
        run_records(b"{\"v\":1}\n\n{\"v\":2}\n", &mut sink, &RunOptions::strict(".v")).expect_err("blank record fails");
    assert!(matches!(failure.failure(), jqf_sdk::PipelineFailure::Codec(_)));
    // Records before the fault are published; the stream stops at the fault.
    assert_eq!(sink.text(), "1\n");
}

#[test]
fn strict_rejects_a_bare_carriage_return() {
    let mut sink = CollectingSink::new();
    let failure = run_records(b"{\"v\":1}\r{\"v\":2}\n", &mut sink, &RunOptions::strict(".v"))
        .expect_err("bare carriage return fails");
    assert!(matches!(failure.failure(), jqf_sdk::PipelineFailure::Codec(_)));
    assert_eq!(sink.bytes, b"");
}

#[test]
fn strict_accepts_a_complete_final_value_without_a_terminator() {
    // JSON Lines permits the last line's separator to be absent, and every
    // incumbent reader accepts it. Refusing it made jqf the only tool that
    // could not read an ordinary log file.
    let mut sink = CollectingSink::new();
    let report = run_records(b"{\"v\":1}\n{\"v\":2}", &mut sink, &RunOptions::strict(".v")).expect("strict completes");
    assert_eq!(sink.text(), "1\n2\n");
    assert_eq!(report.records(), 2);
    // An absent final terminator is not a fault at all: no issue of any
    // severity is raised for it.
    assert_eq!(report.issues(), 0);
    assert_eq!(report.error_issues(), 0);
}

#[test]
fn strict_still_rejects_a_truncated_final_record() {
    // The terminator is optional; a COMPLETE final value is not. A tail cut
    // mid-value has no terminator either, and must still fail — as a malformed
    // payload rather than a framing fault.
    let mut sink = CollectingSink::new();
    let failure = run_records(b"{\"v\":1}\n{\"v\":", &mut sink, &RunOptions::strict(".v"))
        .expect_err("a truncated final record fails");
    assert!(matches!(failure.failure(), jqf_sdk::PipelineFailure::Codec(_)));
    assert_eq!(sink.text(), "1\n");
}

#[test]
fn strict_rejects_a_source_start_byte_order_mark() {
    let mut sink = CollectingSink::new();
    let failure = run_records(b"\xef\xbb\xbf{\"v\":1}\n", &mut sink, &RunOptions::strict(".v"))
        .expect_err("byte-order mark fails");
    assert!(matches!(failure.failure(), jqf_sdk::PipelineFailure::Codec(_)));
    assert_eq!(sink.bytes, b"");
}

#[test]
fn strict_rejects_a_record_above_the_per_record_ceiling() {
    let mut sink = CollectingSink::new();
    let options = RunOptions {
        max_record_bytes: Some(4),
        ..RunOptions::strict(".")
    };
    let failure = run_records(b"1\n{\"v\":1}\n", &mut sink, &options).expect_err("oversize record fails");
    assert!(matches!(failure.failure(), jqf_sdk::PipelineFailure::Codec(_)));
    assert_eq!(sink.text(), "1\n");
}

#[test]
fn recovering_skips_a_blank_record_with_an_advisory_and_exits_zero() {
    let mut sink = CollectingSink::new();
    let report = run_records(b"{\"v\":1}\n\n{\"v\":2}\n", &mut sink, &RunOptions::recovering(".v"))
        .expect("recovering completes");
    assert_eq!(sink.text(), "1\n2\n");
    assert_eq!(report.records(), 2);
    assert_eq!(report.issues(), 1);
    // Advisories alone never force the exit class.
    assert_eq!(report.error_issues(), 0);
    assert_eq!(
        sink.issues,
        vec![SeenIssue {
            ordinal: 1,
            offset: 8,
            severity: RecordIssueSeverity::Advisory,
            code: RecordIssueCode::BlankRecord,
        }]
    );
}

#[test]
fn recovering_reports_a_bare_carriage_return_as_an_error_issue_and_continues() {
    let mut sink = CollectingSink::new();
    let report = run_records(
        b"{\"v\":1}\n{\"v\":2}\r{\"v\":3}\n{\"v\":4}\n",
        &mut sink,
        &RunOptions::recovering(".v"),
    )
    .expect("recovering completes");
    // Ordinal 1 is consumed by the faulty physical unit; recovery resumes after
    // its line feed, so the record that follows is ordinal 2.
    assert_eq!(sink.text(), "1\n4\n");
    assert_eq!(report.error_issues(), 1);
    assert_eq!(sink.issues[0].code, RecordIssueCode::BareCarriageReturn);
    assert_eq!(sink.issues[0].severity, RecordIssueSeverity::Error);
}

#[test]
fn recovering_reports_a_malformed_payload_as_an_error_issue_and_continues() {
    let mut sink = CollectingSink::new();
    let report = run_records(
        b"{\"v\":1}\n{\"v\":\n{\"v\":3}\n",
        &mut sink,
        &RunOptions::recovering(".v"),
    )
    .expect("recovering completes");
    assert_eq!(sink.text(), "1\n3\n");
    assert_eq!(report.error_issues(), 1);
    assert_eq!(sink.issues[0].code, RecordIssueCode::MalformedPayload);
}

#[test]
fn recovering_accepts_a_complete_final_value_without_a_terminator() {
    let mut sink = CollectingSink::new();
    let report =
        run_records(b"{\"v\":1}\n{\"v\":2}", &mut sink, &RunOptions::recovering(".v")).expect("recovering completes");
    assert_eq!(sink.text(), "1\n2\n");
    assert_eq!(report.records(), 2);
    // An absent final terminator is not a fault at all: no issue of any
    // severity is raised for it.
    assert_eq!(report.issues(), 0);
    assert_eq!(report.error_issues(), 0);
}

#[test]
fn recovering_consumes_a_source_start_byte_order_mark_with_an_advisory() {
    // The BOM advisory consumes NO ordinal — it shares ordinal 0 with the
    // record it precedes (the record-stream law: "the BOM advisory itself
    // carries ordinal 0, sharing it with the first record — the advisory
    // precedes, never follows") — so a second fault unit lands at ordinal 1.
    // Ordinals stay dense from zero over records AND issues.
    let mut sink = CollectingSink::new();
    let report = run_records(
        b"\xef\xbb\xbf{\"v\":1}\n\n{\"v\":2}\n",
        &mut sink,
        &RunOptions::recovering(".v"),
    )
    .expect("recovering completes");
    assert_eq!(sink.text(), "1\n2\n");
    assert_eq!(report.records(), 2);
    assert_eq!(report.error_issues(), 0);
    assert_eq!(
        sink.issues,
        vec![
            SeenIssue {
                ordinal: 0,
                offset: 0,
                severity: RecordIssueSeverity::Advisory,
                code: RecordIssueCode::InitialByteOrderMark,
            },
            SeenIssue {
                ordinal: 1,
                offset: 11,
                severity: RecordIssueSeverity::Advisory,
                code: RecordIssueCode::BlankRecord,
            },
        ]
    );
}

#[test]
fn a_byte_order_mark_after_the_source_start_is_an_ordinary_payload_byte() {
    let mut sink = CollectingSink::new();
    let report = run_records(
        b"{\"v\":1}\n\xef\xbb\xbf{\"v\":2}\n",
        &mut sink,
        &RunOptions::recovering(".v"),
    )
    .expect("recovering completes");
    // Strict JSON rejects it as a payload byte, which becomes the record's own
    // malformed-payload issue rather than a framing advisory.
    assert_eq!(sink.text(), "1\n");
    assert_eq!(report.error_issues(), 1);
    assert_eq!(sink.issues[0].code, RecordIssueCode::MalformedPayload);
}

#[test]
fn a_per_record_runtime_error_continues_and_the_last_record_sets_the_class() {
    // Same law as the adjacent path: an earlier failure that a later success
    // follows leaves the run successful.
    let mut sink = CollectingSink::new();
    let report = run_records(b"1\n{\"v\":2}\n", &mut sink, &RunOptions::strict(".v"))
        .expect("earlier runtime error does not fail the run");
    assert_eq!(sink.text(), "2\n");
    assert_eq!(sink.value_errors.len(), 1);
    assert_eq!(report.records(), 2);
}

#[test]
fn a_trailing_runtime_error_fails_the_run() {
    let mut sink = CollectingSink::new();
    let failure =
        run_records(b"{\"v\":2}\n1\n", &mut sink, &RunOptions::strict(".v")).expect_err("trailing runtime error fails");
    assert!(matches!(
        failure.failure(),
        jqf_sdk::PipelineFailure::TypeMismatch { .. }
    ));
    assert_eq!(sink.text(), "2\n");
}

#[test]
fn a_recovering_error_issue_forces_the_exit_class_even_after_later_successes() {
    let mut sink = CollectingSink::new();
    let report = run_records(
        b"{\"v\":\n{\"v\":2}\n{\"v\":3}\n",
        &mut sink,
        &RunOptions::recovering(".v"),
    )
    .expect("the run itself completes");
    assert_eq!(sink.text(), "2\n3\n");
    // The run's own result is success; the forced class rides on the report,
    // which is exactly what the facade turns into a nonzero exit.
    assert_eq!(report.error_issues(), 1);
}

#[test]
fn ndjson_output_appends_exactly_one_codec_owned_terminator() {
    let mut sink = CollectingSink::new();
    let options = RunOptions {
        ndjson_output: Some(NdjsonTerminator::Lf),
        ..RunOptions::strict(".v")
    };
    run_records(b"{\"v\":1}\n{\"v\":2}\n", &mut sink, &options).expect("completes");
    assert_eq!(sink.bytes, b"1\n2\n");
}

#[test]
fn ndjson_output_honours_the_crlf_terminator() {
    let mut sink = CollectingSink::new();
    let options = RunOptions {
        ndjson_output: Some(NdjsonTerminator::CrLf),
        ..RunOptions::strict(".v")
    };
    run_records(b"{\"v\":1}\n{\"v\":2}\n", &mut sink, &options).expect("completes");
    assert_eq!(sink.bytes, b"1\r\n2\r\n");
}

#[test]
fn ndjson_output_terminates_every_generator_emission_not_every_record() {
    let mut sink = CollectingSink::new();
    let options = RunOptions {
        ndjson_output: Some(NdjsonTerminator::Lf),
        ..RunOptions::strict(".v[]")
    };
    run_records(b"{\"v\":[1,2]}\n{\"v\":[]}\n", &mut sink, &options).expect("completes");
    assert_eq!(sink.bytes, b"1\n2\n");
}

#[test]
fn a_record_that_fails_mid_encode_publishes_no_terminator() {
    // The terminator is the CODEC's, never the facade's: it rides the
    // encoder's own staging buffer and reaches the sink only inside a
    // completed item's bytes (the facade suffix for NDJSON output is empty,
    // so no second writer can add one). A publication that dies partway
    // through a record therefore leaves every earlier record's terminator
    // intact and can never grow one of its own. Budget 3 fits record 1's
    // whole framed item (`1` + LF) and record 2's first payload byte, so the
    // failure lands before record 2's terminator regardless of how the
    // encoder chunks its offers.
    let mut sink = BudgetSink::new(3);
    let failure = run_records(
        b"{\"v\":1}\n{\"v\":2222}\n",
        &mut sink,
        &RunOptions {
            ndjson_output: Some(NdjsonTerminator::Lf),
            ..RunOptions::strict(".v")
        },
    )
    .expect_err("the mid-record publication failure fails the run");
    match failure.failure() {
        jqf_sdk::PipelineFailure::Sink(message) => assert_eq!(message, "budget exhausted"),
        other => panic!("expected the sink's own failure class, got {other:?}"),
    }
    // Record 1 published complete WITH its codec terminator; record 2 died
    // mid-payload and its surviving bytes carry NO trailing terminator.
    assert_eq!(sink.bytes, b"1\n2");
    #[allow(
        clippy::naive_bytecount,
        reason = "one-shot test assertion over a handful of bytes; bytecount would be a new dev-dep for nothing"
    )]
    let terminators = sink.bytes.iter().filter(|byte| **byte == b'\n').count();
    assert_eq!(terminators, 1, "exactly one terminator byte: the earlier record's");
}

#[test]
fn a_mismatched_dialect_and_validation_mode_is_rejected_before_any_source_byte() {
    let mut resources = common::resources();
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "test.ndjson",
        b"{\"v\":1}\n",
        0,
    );
    let options = NdjsonDecodeOptions::try_new(None, INPUT_CEILING).expect("normalizes");
    let rejected = jqf_codec_json::ndjson::create_record_provider(
        source,
        NdjsonProfile::Recovering,
        options,
        DiagnosticPolicy::ErrorsOnly,
        ValidationMode::Strict,
        &mut resources,
    );
    assert!(rejected.is_err());
}
