//! json-seq (RFC 7464) codec receipt battery.
//!
//! Pins the json-seq surface as of the codec's first wave: the registration's
//! dialect set (the strict input identity plus the jqf output identity, with
//! `json-seq.recover@1` RESERVED and never advertised), the record route
//! inventory (exactly one route, slot 0, Whole/RecordStream), the framing law
//! (RS boundaries, §2.4 truncation, coalescing, the trailing-RS tail, unframed
//! input, strict-vs-recovering), the ENCODE law (RS + `json.jqf@1` bytes + LF,
//! the raw-string RS suppression, the `-j`/`--raw-output0` suffix laws, pretty
//! by default), and the SDK's recovering drive (a malformed payload becomes an
//! ordered issue and the stream continues).

fn req_dialect() -> &'static DialectId {
    Box::leak(Box::new(
        DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect"),
    ))
}

use crate::drive::{resume, source};
use jqf_codec_core::{
    AccessFootprintKind, AccessResultKind, CodecRunContext, DecodeRequest, DiagnosticPolicy, PreservationRequest,
    RecordBatch, RecordBatchLimit, RecordEntry, RecordIssueCode, RecordPoll, RecordStreamAbort, RouteSlot,
    ValidationMode,
};
use jqf_data::{DialectId, FormatId};
use jqf_engine::CompiledProgram;
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_sdk::{CodecCatalog, FacadeFraming, ItemSink, PipelinePolicy};

static CONTROL: ContinueControl = ContinueControl;

fn resources() -> ResourceContext<'static> {
    ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(1 << 20, u64::MAX, 8 << 20, 0, 64)).expect("account"),
        &CONTROL,
        WorkMeter::try_new_v1(4_096).expect("work"),
    )
    .expect("resources")
}

/// A sink that collects every published byte, exactly.
struct ByteSink {
    bytes: Vec<u8>,
}

impl ItemSink for ByteSink {
    type Error = &'static str;

    fn begin_item(&mut self, _index: u64) -> Result<(), Self::Error> {
        Ok(())
    }

    fn write(&mut self, bytes: &[u8]) -> Result<usize, Self::Error> {
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn finish_item(&mut self, _index: u64, _report: jqf_sdk::EncodedItemReport) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// Pins the registration surface: exactly the strict input dialect and the
/// jqf output dialect; the reserved recovering identity is NOT advertised.
fn registration_surface() -> Result<(), String> {
    let registration = jqf_codec_json::seq::registration().map_err(|error| format!("{error:?}"))?;
    let descriptor = registration.descriptor();
    if descriptor.format().as_str() != "json-seq" {
        return Err(format!("unexpected format {}", descriptor.format().as_str()));
    }
    let dialects = descriptor.dialects();
    let expected = ["json-seq.strict@1", "json-seq.jqf@1"];
    if dialects.len() != expected.len()
        || dialects
            .iter()
            .zip(expected)
            .any(|(left, right)| left.as_str() != right)
    {
        return Err(format!(
            "unexpected json-seq dialect set: {}",
            dialects
                .iter()
                .map(|dialect| dialect.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if dialects.iter().any(|dialect| dialect.as_str() == "json-seq.recover@1") {
        return Err("the reserved recovering dialect must not be advertised".into());
    }
    let _ = DialectId::try_new("json-seq.strict@1").map_err(|error| format!("{error:?}"))?;
    let _ = DialectId::try_new("json-seq.jqf@1").map_err(|error| format!("{error:?}"))?;
    Ok(())
}

/// Pins the record route inventory: exactly one route, slot 0, Whole
/// footprint, `RecordStream` result kind — the same shape NDJSON and CSV
/// advertise. A record stream is not an access observation.
fn route_inventory() -> Result<(), String> {
    let mut resources = resources();
    let options = jqf_codec_json::seq::JsonSeqDecodeOptions::try_new(None, 1 << 20)
        .map_err(|error| format!("json-seq record ceiling: {:?}", error.kind()))?;
    let provider = jqf_codec_json::seq::create_record_provider(
        source(b"\x1e{\"a\":1}\n"),
        jqf_codec_json::seq::JsonSeqProfile::Strict,
        options,
        DiagnosticPolicy::ErrorsOnly,
        ValidationMode::Strict,
        &mut resources,
    )
    .map_err(|error| format!("json-seq record provider: {:?}", error.kind()))?;
    let routes = provider.record_route_descriptions();
    if routes.len() != 1
        || routes[0].slot() != jqf_codec_json::seq::RECORD_ROUTE_SLOT
        || routes[0].bundle().footprint() != AccessFootprintKind::Whole
        || routes[0].bundle().result() != AccessResultKind::RecordStream
    {
        return Err("json-seq did not advertise exactly one record-stream route at slot 0".into());
    }
    Ok(())
}

/// What one framer drive observed.
#[derive(Default)]
struct Observed {
    payloads: Vec<Vec<u8>>,
    issue_codes: Vec<u8>,
    completed: bool,
}

fn drive(bytes: &[u8], profile: jqf_codec_json::seq::JsonSeqProfile) -> Result<Observed, String> {
    let mut resources = resources();
    let options = jqf_codec_json::seq::JsonSeqDecodeOptions::try_new(None, 1 << 20)
        .map_err(|error| format!("ceiling: {:?}", error.kind()))?;
    let mut provider = jqf_codec_json::seq::create_record_provider(
        source(bytes),
        profile,
        options,
        DiagnosticPolicy::ErrorsOnly,
        profile.validation(),
        &mut resources,
    )
    .map_err(|error| format!("provider: {:?}", error.kind()))?;
    let mut stream = provider
        .open_record_route(RouteSlot::new(0), &mut resources)
        .map_err(|error| format!("route: {:?}", error.kind()))?;
    let limit = RecordBatchLimit::new(64, 1 << 20).expect("limit");
    let mut batch = RecordBatch::new();
    let mut observed = Observed::default();
    loop {
        batch.clear();
        let mut run = CodecRunContext::new(&mut resources);
        match stream.poll(limit, &mut batch, &mut run) {
            Ok(RecordPoll::End(_)) => {
                observed.completed = true;
                break;
            }
            Ok(RecordPoll::Pending) => {
                resume(&mut resources);
                continue;
            }
            Ok(RecordPoll::Filled) => {}
            Err(_) => break,
        }
        for entry in batch.entries() {
            match entry {
                RecordEntry::Record(record) => {
                    observed.payloads.push(record.lease().payload().to_vec());
                }
                RecordEntry::Issue(issue) => observed.issue_codes.push(code_tag(issue.code())),
            }
        }
    }
    // Terminal law: a completed stream refuses a further poll; abort is
    // idempotent.
    {
        batch.clear();
        let mut run = CodecRunContext::new(&mut resources);
        if observed.completed && stream.poll(limit, &mut batch, &mut run).is_ok() {
            return Err("a completed stream accepted a further poll".into());
        }
        let first = stream.abort(&mut run);
        let second = stream.abort(&mut run);
        if !matches!(
            (first, second),
            (
                Ok(RecordStreamAbort::Aborted | RecordStreamAbort::AlreadyTerminal),
                Ok(RecordStreamAbort::AlreadyTerminal)
            )
        ) {
            return Err("abort is not idempotent".into());
        }
    }
    Ok(observed)
}

fn code_tag(code: RecordIssueCode) -> u8 {
    match code {
        RecordIssueCode::TruncatedTopLevelScalar => 1,
        RecordIssueCode::UnframedInput => 2,
        RecordIssueCode::MalformedPayload => 3,
        RecordIssueCode::OversizeRecord => 4,
        _ => 5,
    }
}

/// The framing corpus: every law of the ledger §4.4, pinned on both profiles.
fn framing_corpus() -> Result<(), String> {
    let rs = |items: &[&str]| -> Vec<u8> {
        let mut bytes = Vec::new();
        for item in items {
            bytes.push(0x1e);
            bytes.extend_from_slice(item.as_bytes());
            bytes.push(b'\n');
        }
        bytes
    };
    // A well-framed stream delivers every item, both profiles.
    let good = rs(&["{\"a\":1}", "{\"b\":2}"]);
    for profile in [
        jqf_codec_json::seq::JsonSeqProfile::Strict,
        jqf_codec_json::seq::JsonSeqProfile::Recovering,
    ] {
        let observed = drive(&good, profile)?;
        if !observed.completed || observed.payloads.len() != 2 || !observed.issue_codes.is_empty() {
            return Err(format!("well-framed stream failed under {profile:?}"));
        }
    }
    // The §2.4 canaries: `<RS>123<RS>` and `<RS>true<RS>` are rejected by
    // strict (published nothing) and become one issue each in recovering.
    for canary in [b"\x1e123\x1e{\"b\":2}\n".to_vec(), b"\x1etrue\x1e{\"b\":2}\n".to_vec()] {
        let strict = drive(&canary, jqf_codec_json::seq::JsonSeqProfile::Strict)?;
        if strict.completed || !strict.payloads.is_empty() {
            return Err(format!("strict must reject the §2.4 canary {canary:02x?}"));
        }
        let recovering = drive(&canary, jqf_codec_json::seq::JsonSeqProfile::Recovering)?;
        if !recovering.completed
            || recovering.payloads != vec![b"{\"b\":2}\n".to_vec()]
            || recovering.issue_codes != vec![1]
        {
            return Err(format!("recovering must skip the §2.4 canary {canary:02x?}"));
        }
    }
    // A scalar with any trailing JSON whitespace is complete (space, tab, LF,
    // CR all satisfy §2.4 — jq's rule, probed).
    for item in ["123 ", "123\t", "123\n", "123\r"] {
        let mut input = Vec::new();
        input.push(0x1e);
        input.extend_from_slice(item.as_bytes());
        input.push(0x1e);
        input.extend_from_slice(b"{\"b\":2}");
        let observed = drive(&input, jqf_codec_json::seq::JsonSeqProfile::Strict)?;
        if !observed.completed || observed.payloads.len() != 2 {
            return Err(format!("item {item:?} must parse with trailing whitespace"));
        }
    }
    // A number at EOF without whitespace is truncated; a trailing-RS tail is a
    // strict failure and silent in recovering; an RS-only input is the same.
    for (input, expect_strict_fail) in [
        (b"\x1e123".to_vec(), true),
        (b"\x1e{\"a\":1}\n\x1e".to_vec(), true),
        (b"\x1e".to_vec(), true),
        (b"\x1e\x1e".to_vec(), true),
        (b"".to_vec(), false),
    ] {
        let strict = drive(&input, jqf_codec_json::seq::JsonSeqProfile::Strict)?;
        if strict.completed == expect_strict_fail {
            return Err(format!("strict completion mismatch for {input:02x?}"));
        }
        let recovering = drive(&input, jqf_codec_json::seq::JsonSeqProfile::Recovering)?;
        if !recovering.completed {
            return Err(format!("recovering must complete {input:02x?}"));
        }
    }
    // Unframed input: strict fails, recovering reports one advisory.
    let unframed = drive(b"{\"a\":1}\n", jqf_codec_json::seq::JsonSeqProfile::Recovering)?;
    if !unframed.completed || !unframed.payloads.is_empty() || unframed.issue_codes != vec![2] {
        return Err("unframed input must produce one advisory issue".into());
    }
    // Consecutive RS bytes coalesce; bytes before the first RS are dropped.
    let coalesced = drive(b"\x1e\x1e{\"a\":1}\n", jqf_codec_json::seq::JsonSeqProfile::Strict)?;
    if !coalesced.completed || coalesced.payloads != vec![b"{\"a\":1}\n".to_vec()] {
        return Err("consecutive RS bytes must coalesce".into());
    }
    let prefixed = drive(b"{\"a\":1}\x1e{\"b\":2}\n", jqf_codec_json::seq::JsonSeqProfile::Strict)?;
    if !prefixed.completed || prefixed.payloads != vec![b"{\"b\":2}\n".to_vec()] {
        return Err("bytes before the first RS must be dropped".into());
    }
    Ok(())
}

fn program_for(source_text: &str, resources: &ResourceContext<'_>) -> Result<CompiledProgram, String> {
    let policy = jqf_engine::CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    jqf_engine::try_compile_program(source_text, policy, resources)
        .map_err(|error| format!("compile {source_text:?}: {error}"))
}

/// Encodes one ordered run over an adjacent-JSON input, collecting the bytes.
fn encode_run(input: &[u8], options: jqf_codec_json::seq::JsonSeqEncodeOptions) -> Result<(Vec<u8>, u64), String> {
    let mut resources = resources();
    let json = jqf_codec_json::registration().map_err(|error| format!("{error:?}"))?;
    let json_seq = jqf_codec_json::seq::registration().map_err(|error| format!("{error:?}"))?;
    let registrations = [&json, &json_seq];
    let catalog = CodecCatalog::new(&registrations);
    let format = FormatId::try_new(jqf_codec_json::FORMAT_ID).map_err(|e| e.to_string())?;
    let dialect = DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).map_err(|e| e.to_string())?;
    let output_format = FormatId::try_new(jqf_codec_json::seq::FORMAT_ID).map_err(|e| e.to_string())?;
    let output_dialect = DialectId::try_new(jqf_codec_json::seq::JQF_DIALECT_ID).map_err(|e| e.to_string())?;
    let program = program_for(".", &resources)?;
    let requirement = program
        .try_requirement(&resources)
        .map_err(|error| format!("requirement: {:?}", error.kind()))?;
    let source = source(input);
    let mut sink = ByteSink { bytes: Vec::new() };
    let request = jqf_sdk::Request::new(&program, jqf_sdk::Input::Whole(input))
        .with_catalog(catalog)
        .with_source(source)
        .with_format(format, dialect)
        .with_output_format(output_format, output_dialect)
        .with_policy({
            let dialect = req_dialect();
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
                encode_options: Some(&options as &(dyn core::any::Any + Send + Sync)),
                cooperative_credits: 4_096,
                split: None,

                max_iterations: None,
            }
        })
        .with_framing(FacadeFraming::item_suffix(b""))
        .with_resources(&mut resources)
        .with_requirement(&requirement);
    let outcome =
        jqf_sdk::execute(request, &mut sink).map_err(|error| format!("sequence: {:?}", error.pipeline_failure()))?;
    let report = match outcome {
        jqf_sdk::Outcome::Served(jqf_sdk::Report::Sequence(report)) => report,
        other => return Err(format!("sequence outcome unexpected: {other:?}")),
    };
    Ok((sink.bytes, report.items()))
}

/// The ENCODE law: RS + `json.jqf@1` bytes + LF per item (including the
/// last); pretty by default; the `-r` raw-string RS suppression; the
/// `-j`/`--raw-output0` suffix laws; `-c` compact.
fn encode_corpus() -> Result<(), String> {
    use jqf_codec_json::JsonEncodeOptions;
    use jqf_codec_json::seq::{JsonSeqEncodeOptions, JsonSeqSuffix};
    let input = b"{\"a\":1}\n{\"b\":2}\n";
    let pretty = JsonEncodeOptions {
        indent: jqf_codec_json::JsonIndent::Spaces(2),
        ..JsonEncodeOptions::default()
    };
    let compact = JsonEncodeOptions {
        indent: jqf_codec_json::JsonIndent::Compact,
        ..JsonEncodeOptions::default()
    };
    // Pretty by default: RS + `{\n  "a": 1\n}` + LF per item.
    let (bytes, items) = encode_run(input, JsonSeqEncodeOptions::new(pretty, JsonSeqSuffix::Lf))?;
    if items != 2 {
        return Err(format!("encode published {items} items, expected 2"));
    }
    let expected_pretty: &[u8] = b"\x1e{\n  \"a\": 1\n}\n\x1e{\n  \"b\": 2\n}\n";
    if bytes != expected_pretty {
        return Err(format!("pretty encode mismatch: {bytes:02x?}"));
    }
    // Compact: RS + `{"a":1}` + LF.
    let (bytes, _) = encode_run(input, JsonSeqEncodeOptions::new(compact, JsonSeqSuffix::Lf))?;
    if bytes != b"\x1e{\"a\":1}\n\x1e{\"b\":2}\n" {
        return Err(format!("compact encode mismatch: {bytes:02x?}"));
    }
    // `-r` on a root STRING: raw bytes + LF, NO RS prefix (jq's raw arm).
    let raw = JsonEncodeOptions {
        indent: jqf_codec_json::JsonIndent::Compact,
        raw_strings: true,
        ..JsonEncodeOptions::default()
    };
    let (bytes, _) = encode_run(
        b"\"str\"\n\"other\"\n",
        JsonSeqEncodeOptions::new(raw, JsonSeqSuffix::Lf),
    )?;
    if bytes != b"str\nother\n" {
        return Err(format!("raw-string encode must drop the RS: {bytes:02x?}"));
    }
    // `-r` on a NON-string keeps the RS: RS + `123` + LF.
    let (bytes, _) = encode_run(b"123 \n", JsonSeqEncodeOptions::new(raw, JsonSeqSuffix::Lf))?;
    if bytes != b"\x1e123\n" {
        return Err(format!("raw non-string encode must keep the RS: {bytes:02x?}"));
    }
    // `-j`: RS + value, no LF.
    let (bytes, _) = encode_run(input, JsonSeqEncodeOptions::new(compact, JsonSeqSuffix::NoSuffix))?;
    if bytes != b"\x1e{\"a\":1}\x1e{\"b\":2}" {
        return Err(format!("-j encode mismatch: {bytes:02x?}"));
    }
    // `--raw-output0` on a string: raw + NUL, no RS no LF.
    let raw0 = JsonEncodeOptions {
        indent: jqf_codec_json::JsonIndent::Compact,
        raw_strings: true,
        raw_output_nul: true,
        ..JsonEncodeOptions::default()
    };
    let (bytes, _) = encode_run(b"\"str\"\n", JsonSeqEncodeOptions::new(raw0, JsonSeqSuffix::Nul))?;
    if bytes != b"str\0" {
        return Err(format!("raw0 string encode mismatch: {bytes:02x?}"));
    }
    // `--raw-output0` on a number: RS + value + NUL.
    let raw0_nonstring = JsonEncodeOptions {
        indent: jqf_codec_json::JsonIndent::Compact,
        raw_output_nul: true,
        ..JsonEncodeOptions::default()
    };
    let (bytes, _) = encode_run(b"123 \n", JsonSeqEncodeOptions::new(raw0_nonstring, JsonSeqSuffix::Nul))?;
    if bytes != b"\x1e123\0" {
        return Err(format!("raw0 number encode mismatch: {bytes:02x?}"));
    }
    // Zero items produce zero bytes.
    let (bytes, items) = encode_run(b"", JsonSeqEncodeOptions::new(compact, JsonSeqSuffix::Lf))?;
    if items != 0 || !bytes.is_empty() {
        return Err("zero items must produce zero bytes".into());
    }
    Ok(())
}

/// The SDK's recovering drive: a malformed payload becomes an ordered issue
/// and the stream continues (the flag-scoped `--seq` input law).
fn sdk_recovery() -> Result<(), String> {
    let mut resources = resources();
    let json = jqf_codec_json::registration().map_err(|error| format!("{error:?}"))?;
    let json_seq = jqf_codec_json::seq::registration().map_err(|error| format!("{error:?}"))?;
    let registrations = [&json, &json_seq];
    let catalog = CodecCatalog::new(&registrations);
    let format = FormatId::try_new(jqf_codec_json::FORMAT_ID).map_err(|e| e.to_string())?;
    let dialect = DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).map_err(|e| e.to_string())?;
    let output_format = FormatId::try_new(jqf_codec_json::seq::FORMAT_ID).map_err(|e| e.to_string())?;
    let output_dialect = DialectId::try_new(jqf_codec_json::seq::JQF_DIALECT_ID).map_err(|e| e.to_string())?;
    let program = program_for(".", &resources)?;
    let requirement = program
        .try_requirement(&resources)
        .map_err(|error| format!("requirement: {:?}", error.kind()))?;
    let bytes = b"\x1e{\"a\":1}\n\x1e{bad\n\x1e{\"b\":2}\n";
    let source = source(bytes);
    let options = jqf_codec_json::seq::JsonSeqDecodeOptions::try_new(None, 1 << 20)
        .map_err(|error| format!("ceiling: {:?}", error.kind()))?;
    let provider = jqf_codec_json::seq::create_record_provider(
        source,
        jqf_codec_json::seq::JsonSeqProfile::Recovering,
        options,
        DiagnosticPolicy::ErrorsOnly,
        ValidationMode::Recover,
        &mut resources,
    )
    .map_err(|error| format!("provider: {:?}", error.kind()))?;
    let encode_options = jqf_codec_json::seq::JsonSeqEncodeOptions::default();
    let mut sink = ByteSink { bytes: Vec::new() };
    let request = jqf_sdk::Request::new(
        &program,
        jqf_sdk::Input::Records {
            source: source.bytes(),
            records: provider,
            slot: jqf_codec_json::seq::RECORD_ROUTE_SLOT,
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
        encode_options: Some(&encode_options as &(dyn core::any::Any + Send + Sync)),
        cooperative_credits: 4_096,
        split: None,

        max_iterations: None,
    })
    .with_framing(FacadeFraming::item_suffix(b""))
    .with_resources(&mut resources)
    .with_requirement(&requirement);
    let outcome = jqf_sdk::execute(request, &mut sink)
        .map_err(|error| format!("record sequence: {:?}", error.pipeline_failure()))?;
    let report = match outcome {
        jqf_sdk::Outcome::Served(jqf_sdk::Report::Record(report)) => report,
        other => return Err(format!("record outcome unexpected: {other:?}")),
    };
    // The malformed unit was skipped; the two good items published; the issue
    // is on the record route's report (the CLI's --seq law ignores its exit
    // class, which is a CLI fact, not a codec fact).
    if report.records() != 2 || report.issues() != 1 || report.error_issues() != 1 {
        return Err(format!(
            "recovering report unexpected: records={} issues={} error_issues={}",
            report.records(),
            report.issues(),
            report.error_issues()
        ));
    }
    if sink.bytes != b"\x1e{\"a\":1}\n\x1e{\"b\":2}\n" {
        return Err(format!("recovering publish mismatch: {:02x?}", sink.bytes));
    }
    Ok(())
}

pub fn run() -> Result<(), String> {
    let results = [
        ("registration surface", registration_surface()),
        ("route inventory", route_inventory()),
        ("framing corpus", framing_corpus()),
        ("encode corpus", encode_corpus()),
        ("sdk recovery", sdk_recovery()),
    ];
    let mut failures = 0;
    for (label, result) in results {
        match result {
            Ok(()) => println!("json-seq-smoke: {label}: ok"),
            Err(error) => {
                failures += 1;
                println!("json-seq-smoke: {label}: FAIL: {error}");
            }
        }
    }
    if failures != 0 {
        println!("json-seq-smoke: {failures} receipt(s) failed");
        return Err(format!("{failures} receipt(s) failed"));
    }
    println!("json-seq-smoke: all receipts pass");
    Ok(())
}
