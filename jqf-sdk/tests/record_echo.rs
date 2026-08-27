//! Record-lane S4 canonical source echo vs compact render.
//!
//! Identity over an NDJSON stream echoes a record's payload bytes when that
//! record is canonical, and renders otherwise. Any byte mismatch against a
//! program that always renders (`[.][0]`) kills the echo.

use std::fs;
use std::path::PathBuf;

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

fn json_dialect() -> &'static DialectId {
    Box::leak(Box::new(DialectId::try_new("rfc8259").expect("dialect")))
}

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
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, 64 << 20, 0, 128)).expect("account"),
        &CONTROL,
        WorkMeter::try_new_v1(COOPERATIVE_CREDITS).expect("work"),
    )
    .expect("resources")
}

fn run_ndjson(input: &[u8], program: &str) -> Result<Vec<u8>, String> {
    let json = jqf_codec_json::registration().expect("json registration");
    let streams = jqf_codec_json::ndjson::registration().expect("ndjson registration");
    let registrations = [&json, &streams];
    let catalog = CodecCatalog::new(&registrations);
    let format = FormatId::try_new(jqf_codec_json::FORMAT_ID).expect("format");
    let dialect = DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect");
    let output_format = FormatId::try_new(jqf_codec_json::ndjson::FORMAT_ID).expect("format");
    let output_dialect = DialectId::try_new(jqf_codec_json::ndjson::STRICT_DIALECT_ID).expect("dialect");
    let mut resources = resources();
    let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    let compiled = try_compile_program(program, policy, &resources).expect("compiles");
    let requirement = compiled.try_requirement(&resources).expect("requirement lowers");
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
    let encode_options = NdjsonEncodeOptions::new(NdjsonTerminator::Lf);
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
    .with_format(format, dialect)
    .with_output_format(output_format, output_dialect)
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
        encode_options: Some(&encode_options),
        cooperative_credits: COOPERATIVE_CREDITS,
        split: None,

        max_iterations: None,
    })
    .with_framing(FacadeFraming::item_suffix(b""))
    .with_resources(&mut resources)
    .with_requirement(&requirement);
    match jqf_sdk::execute(request, &mut sink) {
        Ok(Outcome::Served(Report::Record(_))) => Ok(sink.bytes),
        Ok(other) => Err(format!("unexpected outcome: {other:?}")),
        Err(error) => Err(format!("execute failed: {error:?}")),
    }
}

/// Identity (echo when canonical) vs `[.][0]` (always render). Any mismatch
/// is the kill condition.
fn echo_matches_render(input: &[u8]) -> Result<(), String> {
    let echoed = run_ndjson(input, ".")?;
    let rendered = run_ndjson(input, "[.][0]")?;
    if echoed != rendered {
        return Err(format!(
            "echo-vs-render mismatch: echo={:?} render={:?} input={:?}",
            String::from_utf8_lossy(&echoed),
            String::from_utf8_lossy(&rendered),
            String::from_utf8_lossy(input)
        ));
    }
    Ok(())
}

#[test]
fn canonical_records_echo_equals_render() {
    for input in [
        b"1\n".as_slice(),
        b"true\n",
        b"null\n",
        b"\"a\"\n",
        b"[0.5]\n",
        b"[1.5]\n",
        b"{\"a\":1}\n",
        b"{\"a\":1.5}\n",
        b"1\n2\n3\n",
        b"{\"v\":1}\n{\"v\":2}\n",
        // Absent final terminator is accepted NDJSON; both arms add LF.
        b"{\"a\":1}",
        b"1\n2",
    ] {
        echo_matches_render(input).unwrap_or_else(|error| panic!("{error}"));
    }
}

#[test]
fn non_canonical_records_still_match_render() {
    // Interior whitespace, exponents, non-minimal escapes, duplicate keys:
    // the echo must decline and the compact render must match `[.][0]`.
    for input in [
        // Interior spaces on one physical line (a newline would be a
        // record boundary, not JSON whitespace).
        b"{ \"a\": 1 }\n".as_slice(),
        b"[1e3]\n",
        b"[\"a\\/b\"]\n",
        b"[\"\\u0041\"]\n",
        b"{\"a\":1,\"a\":2}\n",
        b"{ \"a\": 1 }\n{\"b\":2}\n",
        b"{\"a\":1}\n[1e3]\n",
    ] {
        echo_matches_render(input).unwrap_or_else(|error| panic!("{error}"));
    }
}

#[test]
fn mixed_canonical_and_non_canonical_stream_matches_render() {
    echo_matches_render(b"{\"a\":1}\n{ \"b\": 2 }\n3\n[1e3]\n").unwrap_or_else(|error| panic!("{error}"));
}

fn record_stream_corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tools/fuzz/jqf-codec-fuzz/corpus/record_stream")
}

#[test]
#[ignore = "local tools/fuzz corpus"]
fn fuzz_record_corpus_echo_equals_render() {
    let dir = record_stream_corpus_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    let mut files = 0usize;
    let mut compared = 0usize;
    for entry in entries {
        let entry = entry.expect("corpus entry");
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        files += 1;
        let input = fs::read(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        match (run_ndjson(&input, "."), run_ndjson(&input, "[.][0]")) {
            (Ok(echoed), Ok(rendered)) => {
                assert_eq!(
                    echoed,
                    rendered,
                    "echo-vs-render mismatch on corpus file {}",
                    path.display()
                );
                compared += 1;
            }
            (Err(_), Err(_)) => {}
            (Ok(echoed), Err(render_error)) => panic!(
                "echo succeeded and render failed on {}: echo={:?} render={render_error}",
                path.display(),
                String::from_utf8_lossy(&echoed)
            ),
            (Err(echo_error), Ok(rendered)) => panic!(
                "echo failed and render succeeded on {}: echo={echo_error} render={:?}",
                path.display(),
                String::from_utf8_lossy(&rendered)
            ),
        }
    }
    assert!(files >= 6, "corpus at {} produced only {files} files", dir.display());
    assert!(
        compared >= 1,
        "corpus differential compared no successful file (vacuous)"
    );
}
