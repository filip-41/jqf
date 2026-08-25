//! CSV benchmark lanes: record-route decode (whole), scoped column, shallow
//! stand-in, and deterministic encode.

use std::hint::black_box;

use jqf_bench_core::{BenchmarkCase, CaseMetadata, PreflightReceipt};
use jqf_codec_json::ndjson::NdjsonTerminator;
use jqf_codec_json::{JsonEncodeOptions, JsonIndent};
use jqf_engine::{CodecRequirementPolicy, CompiledProgram, try_compile_program};
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_runtime::records::{
    OutputTarget, ParallelPlan, PlanDecision, RecordDriveSpec, RecordInputKind, RecordOutputSpec, RecordRunModel,
    execute_record_request,
};
use jqf_sdk::{CodecCatalog, EncodedItemReport, ItemSink};

use crate::fixtures;

static BENCH_CONTROL: ContinueControl = ContinueControl;

struct ChecksumSink {
    checksum: u64,
    items: u64,
}

impl ChecksumSink {
    fn new() -> Self {
        Self {
            checksum: 0xcbf2_9ce4_8422_2325,
            items: 0,
        }
    }

    fn absorb(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.checksum = self.checksum.rotate_left(5) ^ u64::from(byte);
        }
    }
}

impl ItemSink for ChecksumSink {
    type Error = &'static str;

    fn begin_item(&mut self, _index: u64) -> Result<(), Self::Error> {
        self.items = self.items.saturating_add(1);
        Ok(())
    }

    fn write(&mut self, bytes: &[u8]) -> Result<usize, Self::Error> {
        self.absorb(bytes);
        Ok(bytes.len())
    }

    fn finish_item(&mut self, _index: u64, _report: EncodedItemReport) -> Result<(), Self::Error> {
        Ok(())
    }
}

fn run_context() -> ResourceContext<'static> {
    let (input, output, memory, spill, depth) = jqf_bench_core::limits::MEASURED_REGION;
    ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(input, output, memory, spill, depth)).expect("account"),
        &BENCH_CONTROL,
        WorkMeter::try_new_v1(4096).expect("meter"),
    )
    .expect("context")
}

fn record_catalog() -> CodecCatalog<'static, 'static> {
    jqf_runtime::records::install_record_catalog(
        jqf_codec_json::registration().expect("json"),
        jqf_codec_json::ndjson::registration().expect("ndjson"),
        jqf_codec_json::seq::registration().expect("json-seq"),
        jqf_codec_delimited::registration().expect("csv"),
        jqf_codec_delimited::registration_tsv().expect("tsv"),
        jqf_codec_render::registration().expect("render"),
        jqf_codec_yaml::registration().expect("yaml"),
        jqf_codec_xml::registration().expect("xml"),
        jqf_codec_html::registration().expect("html"),
    )
}

fn record_checksum(source: &[u8], compiled: &CompiledProgram) -> u64 {
    let mut resources = run_context();
    let spec = RecordDriveSpec {
        input: source,
        source_name: "bench.csv",
        files: None,
        kind: RecordInputKind::Csv {
            header: false,
            tsv: false,
        },
        profile: jqf_codec_json::ndjson::NdjsonProfile::Strict,
        json_seq_profile: jqf_codec_json::seq::JsonSeqProfile::Strict,
        csv_delimiter: None,
        csv_textdata: false,
        max_record_bytes: u64::MAX,
        max_iterations: None,
        catalog: record_catalog(),
        output: RecordOutputSpec {
            target: OutputTarget::Json,
            terminator: NdjsonTerminator::Lf,
            json: JsonEncodeOptions {
                indent: JsonIndent::Compact,
                raw_strings: false,
                sort_keys: false,
                ascii_output: false,
                raw_output_nul: false,
            },
            no_newline: false,
        },
        model: RecordRunModel::PerRecord,
        edit: false,
        cooperative_credits: 4096,
    };
    let plan = ParallelPlan::serial(
        jqf_runtime::records::WorkerRequest::Explicit(1),
        PlanDecision::SwitchedOff,
        source.len() as u64,
    );
    let mut sink = ChecksumSink::new();
    execute_record_request(spec, plan, compiled, &mut resources, &mut sink, None).expect("record drive completes");
    sink.checksum
}

struct DecodeCase {
    name: &'static str,
    fixture_name: &'static str,
    source: String,
    program: &'static str,
    witness: WitnessKind,
    compiled: Option<CompiledProgram>,
}

#[derive(Clone, Copy)]
enum WitnessKind {
    Whole,
    Scoped,
    Shallow,
}

impl DecodeCase {
    fn expected(&self, expected: &fixtures::Expected) -> u64 {
        match self.witness {
            WitnessKind::Whole => expected.whole,
            WitnessKind::Scoped => expected.scoped,
            WitnessKind::Shallow => expected.shallow,
        }
    }

    fn witness_name(&self) -> &'static str {
        match self.witness {
            WitnessKind::Whole => "whole decode witness",
            WitnessKind::Scoped => "scoped decode witness",
            WitnessKind::Shallow => "shallow decode witness",
        }
    }
}

impl BenchmarkCase for DecodeCase {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new(self.name, 1, self.source.len() as u64)
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let resources = run_context();
        let policy = CodecRequirementPolicy::new(
            jqf_codec_core::ValidationMode::Strict,
            jqf_codec_core::DiagnosticPolicy::ErrorsOnly,
        );
        let compiled = try_compile_program(self.program, policy, &resources).expect("compiles");
        let checksum = record_checksum(self.source.as_bytes(), &compiled);
        let expected = self.expected(&fixtures::expected(self.fixture_name));
        if checksum != expected {
            return Err(format!(
                "{}: {} drifted (got {checksum:#x}, want {expected:#x})",
                self.name,
                self.witness_name(),
            ));
        }
        self.compiled = Some(compiled);
        Ok(PreflightReceipt::new(checksum, self.witness_name().to_owned()))
    }

    fn run(&mut self) -> u64 {
        let compiled = self.compiled.as_ref().expect("preflight compiled the program");
        record_checksum(black_box(self.source.as_bytes()), compiled)
    }
}

struct EncodeCase {
    name: &'static str,
    fixture_name: &'static str,
    source: String,
    encoded: Option<Vec<u8>>,
    compiled: Option<CompiledProgram>,
}

impl BenchmarkCase for EncodeCase {
    fn metadata(&self) -> CaseMetadata {
        let bytes = self.encoded.as_ref().map_or(0, Vec::len) as u64;
        CaseMetadata::new(self.name, 1, bytes)
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let resources = run_context();
        let policy = CodecRequirementPolicy::new(
            jqf_codec_core::ValidationMode::Strict,
            jqf_codec_core::DiagnosticPolicy::ErrorsOnly,
        );
        let compiled = try_compile_program(".", policy, &resources).expect("compiles");
        let out = encode_checksum_bytes(self.source.as_bytes(), &compiled);
        let checksum = out.iter().fold(0xcbf2_9ce4_8422_2325u64, |acc, byte| {
            acc.rotate_left(5) ^ u64::from(*byte)
        });
        let expected = fixtures::expected(self.fixture_name).encoded;
        if checksum != expected {
            return Err(format!(
                "{}: encode witness drifted (got {checksum:#x}, want {expected:#x})",
                self.name
            ));
        }
        self.encoded = Some(out);
        self.compiled = Some(compiled);
        Ok(PreflightReceipt::new(checksum, "pinned encode witness".to_owned()))
    }

    fn run(&mut self) -> u64 {
        let compiled = self.compiled.as_ref().expect("preflight compiled the program");
        let bytes = encode_checksum_bytes(black_box(self.source.as_bytes()), compiled);
        bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |acc, byte| {
            acc.rotate_left(5) ^ u64::from(*byte)
        })
    }
}

fn encode_checksum_bytes(source: &[u8], compiled: &CompiledProgram) -> Vec<u8> {
    let mut resources = run_context();
    let spec = RecordDriveSpec {
        input: source,
        source_name: "bench.csv",
        files: None,
        kind: RecordInputKind::Csv {
            header: false,
            tsv: false,
        },
        profile: jqf_codec_json::ndjson::NdjsonProfile::Strict,
        json_seq_profile: jqf_codec_json::seq::JsonSeqProfile::Strict,
        csv_delimiter: None,
        csv_textdata: false,
        max_record_bytes: u64::MAX,
        max_iterations: None,
        catalog: record_catalog(),
        output: RecordOutputSpec {
            target: OutputTarget::Csv {
                header: false,
                utf8: false,
            },
            terminator: NdjsonTerminator::Lf,
            json: JsonEncodeOptions {
                indent: JsonIndent::Compact,
                raw_strings: false,
                sort_keys: false,
                ascii_output: false,
                raw_output_nul: false,
            },
            no_newline: false,
        },
        model: RecordRunModel::PerRecord,
        edit: false,
        cooperative_credits: 4096,
    };
    let plan = ParallelPlan::serial(
        jqf_runtime::records::WorkerRequest::Explicit(1),
        PlanDecision::SwitchedOff,
        source.len() as u64,
    );
    let mut sink = CollectSink::default();
    execute_record_request(spec, plan, compiled, &mut resources, &mut sink, None).expect("record drive completes");
    sink.bytes
}

#[derive(Default)]
struct CollectSink {
    bytes: Vec<u8>,
}

impl ItemSink for CollectSink {
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

pub(crate) fn cases() -> Vec<Box<dyn BenchmarkCase>> {
    let mut out: Vec<Box<dyn BenchmarkCase>> = Vec::new();
    for fixture in fixtures::fixtures() {
        let source = String::from(fixture.source);
        for (suffix, program, witness) in [
            ("whole", ".", WitnessKind::Whole),
            ("scoped-1", ".[1]", WitnessKind::Scoped),
            ("shallow-keys", "keys", WitnessKind::Shallow),
        ] {
            out.push(Box::new(DecodeCase {
                name: Box::leak(format!("decode/{}/{suffix}", fixture.name).into_boxed_str()),
                fixture_name: fixture.name,
                source: source.clone(),
                program,
                witness,
                compiled: None,
            }));
        }
        out.push(Box::new(EncodeCase {
            name: Box::leak(format!("encode/{}", fixture.name).into_boxed_str()),
            fixture_name: fixture.name,
            source,
            encoded: None,
            compiled: None,
        }));
    }
    out
}

#[cfg(test)]
fn compiled_for(program: &str) -> CompiledProgram {
    let resources = run_context();
    let policy = CodecRequirementPolicy::new(
        jqf_codec_core::ValidationMode::Strict,
        jqf_codec_core::DiagnosticPolicy::ErrorsOnly,
    );
    try_compile_program(program, policy, &resources).expect("compiles")
}

#[cfg(test)]
#[must_use]
pub(crate) fn pin_decode_checksum(source: &str) -> u64 {
    let compiled = compiled_for(".");
    record_checksum(source.as_bytes(), &compiled)
}

#[cfg(test)]
#[must_use]
pub(crate) fn pin_scoped_checksum(source: &str) -> u64 {
    let compiled = compiled_for(".[1]");
    record_checksum(source.as_bytes(), &compiled)
}

#[cfg(test)]
#[must_use]
pub(crate) fn pin_shallow_checksum(source: &str) -> u64 {
    let compiled = compiled_for("keys");
    record_checksum(source.as_bytes(), &compiled)
}

#[cfg(test)]
#[must_use]
pub(crate) fn pin_encode_checksum(source: &str) -> u64 {
    let compiled = compiled_for(".");
    let bytes = encode_checksum_bytes(source.as_bytes(), &compiled);
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |acc, byte| {
        acc.rotate_left(5) ^ u64::from(*byte)
    })
}
