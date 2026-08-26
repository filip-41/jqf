//! The json-seq OUTPUT vertical end to end, through the real SDK record drive.
//!
//! These pin the RFC 7464 write law as implemented: every published item —
//! including the last, which needs NO terminating RS on the read side —
//! carries the codec-owned RS prefix and LF suffix inside one atomic scope,
//! and a root string the `-r` raw arm prints verbatim gets no RS prefix.
//! Nothing here reaches into the encoder's internals — every assertion is
//! about what a host actually observes. (`seq_records.rs` owns the READ
//! side's framing law; `ndjson_records.rs` is the sibling harness this one
//! mirrors.)

mod common;

fn req_dialect() -> &'static DialectId {
    Box::leak(Box::new(DialectId::try_new("rfc8259").expect("dialect")))
}

use jqf_codec_core::{DecodeRequest, DiagnosticPolicy, PreservationRequest, ValidationMode};
use jqf_codec_json::JsonEncodeOptions;
use jqf_codec_json::seq::{JsonSeqDecodeOptions, JsonSeqEncodeOptions, JsonSeqProfile, JsonSeqSuffix};
use jqf_data::{DialectId, FormatId};
use jqf_sdk::{
    CodecCatalog, CodecRequirementPolicy, EncodedItemReport, FacadeFraming, ItemSink, PipelinePolicy,
    RecordSequenceReport, try_compile_program,
};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

const COOPERATIVE_CREDITS: u32 = 64;
const INPUT_CEILING: u64 = 1 << 20;

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

/// One json-seq-in, json-seq-out request over the SDK record drive: the
/// framer frames the input's RS units, each payload decodes through the
/// strict-JSON ladder, and every encoded item re-publishes framed. The
/// facade suffix is EMPTY because the codec owns BOTH frame halves — the
/// same derivation the CLI's output selection makes for `--seq`.
#[allow(
    clippy::too_many_lines,
    reason = "one linear harness invocation mirroring the CLI's own record branch"
)]
fn run_seq_records(
    input: &[u8],
    sink: &mut CollectingSink,
    profile: JsonSeqProfile,
    json_style: JsonEncodeOptions,
    suffix: JsonSeqSuffix,
    program: &'static str,
) -> Result<RecordSequenceReport, jqf_sdk::PipelineError<String>> {
    let json = jqf_codec_json::registration().expect("json registration is valid");
    let seq = jqf_codec_json::seq::registration().expect("json-seq registration is valid");
    let registrations = [&json, &seq];
    let catalog = CodecCatalog::new(&registrations);
    let mut resources = common::resources();
    let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    let compiled = try_compile_program(program, policy, &resources).expect("compiles");
    let requirement = compiled.try_requirement(&resources).expect("requirement lowers");
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "test.json-seq",
        input,
        0,
    );
    let record_options = JsonSeqDecodeOptions::try_new(None, INPUT_CEILING).expect("record ceiling normalizes");
    let records = jqf_codec_json::seq::create_record_provider(
        source,
        profile,
        record_options,
        DiagnosticPolicy::ErrorsOnly,
        profile.validation(),
        &mut resources,
    )
    .expect("record provider opens");
    // A record PAYLOAD decodes under the payload codec's identity
    // (json/rfc8259), never the framing format's — the same derivation the
    // CLI's record route makes when it opens a json-seq drive.
    let format = FormatId::try_new(jqf_codec_json::FORMAT_ID).expect("format id is valid");
    let dialect = DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect id is valid");
    let output_format = FormatId::try_new(jqf_codec_json::seq::FORMAT_ID).expect("format id is valid");
    let output_dialect = DialectId::try_new(jqf_codec_json::seq::JQF_DIALECT_ID).expect("dialect id is valid");
    let encode_options = JsonSeqEncodeOptions::new(json_style, suffix);
    let opaque = &encode_options as &(dyn core::any::Any + Send + Sync);
    let request = jqf_sdk::Request::new(
        &compiled,
        jqf_sdk::Input::Records {
            source: source.bytes(),
            records,
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
        encode_options: Some(opaque),
        cooperative_credits: COOPERATIVE_CREDITS,
        split: None,
        max_iterations: None,
    })
    .with_framing(FacadeFraming::item_suffix(b""))
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

#[test]
fn every_item_including_the_last_carries_its_rs_frame() {
    // Output framing is per ITEM and codec-owned: RS before, LF after, both
    // joined to the payload inside the encoder's own staging buffer. Unlike
    // the READ side, the WRITE side terminates the last item too — a stream
    // ending without its final LF would be an unterminated unit to the next
    // reader.
    let mut sink = CollectingSink::new();
    let report = run_seq_records(
        b"\x1e{\"v\":1}\n\x1e{\"v\":2}",
        &mut sink,
        JsonSeqProfile::Strict,
        JsonEncodeOptions::default(),
        JsonSeqSuffix::Lf,
        ".v",
    )
    .expect("completes");
    assert_eq!(sink.bytes, b"\x1e1\n\x1e2\n");
    assert_eq!(report.records(), 2);
}

#[test]
fn every_generator_emission_gets_its_own_frame() {
    // Framing counts EMISSIONS, not records: a generator's outputs are
    // separate texts to the next reader — the same law NDJSON's terminator
    // keeps one-per-line.
    let mut sink = CollectingSink::new();
    run_seq_records(
        b"\x1e{\"v\":[1,2]}\n",
        &mut sink,
        JsonSeqProfile::Strict,
        JsonEncodeOptions::default(),
        JsonSeqSuffix::Lf,
        ".v[]",
    )
    .expect("completes");
    assert_eq!(sink.bytes, b"\x1e1\n\x1e2\n");
}

#[test]
fn a_raw_root_string_publishes_no_rs_prefix() {
    // jq's `--seq -r` law: a ROOT string the raw arm writes verbatim gets NO
    // RS prefix (its bytes must never be mistaken for part of the frame),
    // while every non-string item keeps the prefix.
    let mut sink = CollectingSink::new();
    run_seq_records(
        b"\x1e{\"v\":\"hi\"}\n\x1e{\"v\":1}\n",
        &mut sink,
        JsonSeqProfile::Strict,
        JsonEncodeOptions {
            raw_strings: true,
            ..JsonEncodeOptions::default()
        },
        JsonSeqSuffix::Lf,
        ".v",
    )
    .expect("completes");
    assert_eq!(sink.bytes, b"hi\n\x1e1\n");
}

#[test]
fn the_join_law_keeps_the_prefix_and_drops_the_lf() {
    // `-j` under `--seq`: exactly the default framing minus the suffix byte
    // — the RS stays codec-owned, only the LF goes.
    let mut sink = CollectingSink::new();
    run_seq_records(
        b"\x1e{\"v\":1}\n\x1e{\"v\":2}",
        &mut sink,
        JsonSeqProfile::Strict,
        JsonEncodeOptions::default(),
        JsonSeqSuffix::NoSuffix,
        ".v",
    )
    .expect("completes");
    assert_eq!(sink.bytes, b"\x1e1\x1e2");
}
