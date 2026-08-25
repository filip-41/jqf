//! Engine-level benchmark cases for the jqf-extension and parity builtins.
//!
//! Each case runs the complete product path (strict-JSON decode -> engine ->
//! encode) through the SDK over an in-memory fixture. The broad multi-tool
//! harness cannot host these programs because jq has no oracle spelling for
//! them; the engine worker is their tracked home. Preflights are exact where
//! the law is deterministic and structural where the law is not (UUID
//! generation).

fn engine_bench_dialect() -> &'static DialectId {
    Box::leak(Box::new(
        DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect"),
    ))
}

use std::fmt::Write as _;

use jqf_bench_core::{BenchmarkCase, CaseMetadata, PreflightReceipt};
use jqf_codec_core::{
    AccessRequirement, CodecRegistration, DecodeRequest, DiagnosticPolicy, PreservationRequest, ValidationMode,
};
use jqf_data::{DialectId, FormatId, Object, ObjectBuilder, ObjectKey, Value};
use jqf_engine::{CodecRequirementPolicy, CompiledProgram, try_compile_program};
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_sdk::{CodecCatalog, EncodedItemReport, FacadeFraming, ItemSink, PipelinePolicy};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

static CONTROL: ContinueControl = ContinueControl;

/// Exact or structural preflight check for one case.
enum Check {
    /// The sink bytes must equal this exact text.
    Exact(&'static str),
    /// Every line is a generated UUID with the expected version nibble.
    UuidVersion(u8),
    /// The `truncate_stream` case: expected output is built from the program.
    TruncateStream,
}

#[derive(Default)]
struct CollectSink {
    bytes: Vec<u8>,
    items: u64,
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
        self.items += 1;
        Ok(())
    }
}

struct EngineCase {
    name: &'static str,
    operations: u64,
    timed_bytes: Vec<u8>,
    check_input: Vec<u8>,
    check: Check,
    program: String,
    compiled: CompiledProgram,
    requirement: AccessRequirement,
    registration: Box<CodecRegistration<'static>>,
    format: FormatId,
    dialect: DialectId,
    resources: ResourceContext<'static>,
}

fn resources() -> ResourceContext<'static> {
    // The nesting ceiling matches the CLI's MAX_NESTING_DEPTH: the bench
    // harness must accept any program the real CLI accepts. A lower cap here
    // silently breaks the truncate-1k lane's 1000-item comma chain, so the
    // bench suite panics on every run instead of measuring.
    ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, 64 << 20, 0, 10_000)).expect("account"),
        &CONTROL,
        WorkMeter::try_new_v1(7).expect("work meter"),
    )
    .expect("resources")
}

fn compile(source: &str, resources: &ResourceContext<'_>) -> Result<CompiledProgram, String> {
    let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    try_compile_program(source, policy, resources).map_err(|error| format!("{source:?}: {error}"))
}

impl EngineCase {
    fn new(
        name: &'static str,
        operations: u64,
        timed_bytes: Vec<u8>,
        check_input: Vec<u8>,
        check: Check,
        program: String,
        _timed_bytes_len: usize,
    ) -> Result<Self, String> {
        let resources = resources();
        let compiled = compile(&program, &resources)?;
        let requirement = compiled
            .try_requirement(&resources)
            .map_err(|error| format!("requirement {name:?}: {error:?}"))?;
        let registration =
            Box::new(jqf_codec_json::registration().map_err(|error| format!("json registration: {error:?}"))?);
        let format = FormatId::try_new(jqf_codec_json::FORMAT_ID).map_err(|error| format!("format: {error}"))?;
        let dialect =
            DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).map_err(|error| format!("dialect: {error}"))?;
        Ok(Self {
            name,
            operations,
            timed_bytes,
            check_input,
            check,
            program,
            compiled,
            requirement,
            registration,
            format,
            dialect,
            resources,
        })
    }
}

/// Runs one case body through the engine and captures its sink bytes.
///
/// Eight parameters mirror the engine SDK call's own parameter list; the
/// seam is deliberately explicit so the bench never hides which request it
/// times.
#[expect(
    clippy::too_many_arguments,
    reason = "mirrors the engine execute call's parameter list; grouping would hide the request being timed"
)]
fn execute_with(
    resources: &mut ResourceContext<'static>,
    registration: &CodecRegistration<'static>,
    format: &FormatId,
    dialect: &DialectId,
    requirement: &AccessRequirement,
    program: &CompiledProgram,
    input: &[u8],
    sink: &mut CollectSink,
) -> Result<(), String> {
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "engine-bench.json",
        input,
        0,
    );
    let registrations = [registration];
    let catalog = CodecCatalog::new(&registrations);
    let request = jqf_sdk::Request::new(program, jqf_sdk::Input::Whole(input))
        .with_catalog(catalog)
        .with_source(source)
        .with_format(
            jqf_data::FormatId::try_new(format.as_str()).expect("built-in format identity"),
            jqf_data::DialectId::try_new(dialect.as_str()).expect("built-in dialect identity"),
        )
        .with_output_format(
            jqf_data::FormatId::try_new(format.as_str()).expect("built-in format identity"),
            jqf_data::DialectId::try_new(dialect.as_str()).expect("built-in dialect identity"),
        )
        .with_policy(PipelinePolicy {
            decode: DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: engine_bench_dialect(),
                options: None,
                allow_adjacent_values: false,
                value_separator: jqf_codec_json::VALUE_SEPARATORS,
            },
            encode_diagnostics: DiagnosticPolicy::ErrorsOnly,
            preservation: PreservationRequest::Report,
            encode_options: None,
            cooperative_credits: 7,
            split: None,

            max_iterations: None,
        })
        .with_framing(FacadeFraming::item_suffix(b"\n"))
        .with_resources(resources)
        .with_requirement(requirement);
    jqf_sdk::execute(request, sink)
        .map(|_| ())
        .map_err(|error| format!("engine execute: {:?}", error.pipeline_failure()))
}

impl BenchmarkCase for EngineCase {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new(
            self.name,
            self.operations,
            u64::try_from(self.timed_bytes.len()).unwrap_or(u64::MAX),
        )
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let mut sink = CollectSink::default();
        let Self {
            resources,
            registration,
            format,
            dialect,
            requirement,
            compiled,
            check_input,
            ..
        } = self;
        execute_with(
            resources,
            registration.as_ref(),
            format,
            dialect,
            requirement,
            compiled,
            check_input,
            &mut sink,
        )?;
        match &self.check {
            Check::Exact(expected) => {
                if sink.bytes != expected.as_bytes() {
                    return Err(format!(
                        "expected {:?}, got {:?}",
                        expected,
                        String::from_utf8_lossy(&sink.bytes)
                    ));
                }
                Ok(PreflightReceipt::new(
                    fnv1a(&sink.bytes),
                    format!("bytes={} items={}", sink.bytes.len(), sink.items),
                ))
            }
            Check::UuidVersion(version) => {
                let mut count = 0u64;
                for line in sink.bytes.split(|b| *b == b'\n') {
                    if line.is_empty() {
                        continue;
                    }
                    if line.len() != 38
                        || line.first() != Some(&b'"')
                        || line.last() != Some(&b'"')
                        || line[9] != b'-'
                        || line[14] != b'-'
                        || line[19] != b'-'
                        || line[24] != b'-'
                        || line[15] != b'0' + version
                    {
                        return Err(format!(
                            "uuid v{version} shape violated by {:?}",
                            String::from_utf8_lossy(line)
                        ));
                    }
                    count += 1;
                }
                Ok(PreflightReceipt::new(
                    fnv1a(&sink.bytes),
                    format!("uuids={count} bytes={}", sink.bytes.len()),
                ))
            }
            Check::TruncateStream => {
                let expected = truncate_expected(&self.program);
                if sink.bytes != expected.as_bytes() {
                    return Err(format!(
                        "truncate_stream mismatch: expected {:?}, got {:?}",
                        expected,
                        String::from_utf8_lossy(&sink.bytes)
                    ));
                }
                Ok(PreflightReceipt::new(
                    fnv1a(&sink.bytes),
                    format!("bytes={} items={}", sink.bytes.len(), sink.items),
                ))
            }
        }
    }

    fn run(&mut self) -> u64 {
        let mut sink = CollectSink::default();
        let Self {
            resources,
            registration,
            format,
            dialect,
            requirement,
            compiled,
            timed_bytes,
            ..
        } = self;
        execute_with(
            resources,
            registration.as_ref(),
            format,
            dialect,
            requirement,
            compiled,
            timed_bytes,
            &mut sink,
        )
        .expect("preflighted engine case failed during timing");
        fnv1a(&sink.bytes)
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn json_object(entries: &[(&str, String)]) -> String {
    let mut out = String::from("{");
    for (index, (key, value)) in entries.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let _ = write!(out, "\"{key}\":{value}");
    }
    out.push('}');
    out
}

fn json_array(values: &[String]) -> String {
    let mut out = String::from("[");
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(value);
    }
    out.push(']');
    out
}

fn ints(start: i64, count: usize) -> String {
    let values: Vec<String> = (start..start + i64::try_from(count).unwrap_or(i64::MAX))
        .map(|value| value.to_string())
        .collect();
    json_array(&values)
}

fn floats(start: f64, step: f64, count: usize) -> String {
    let values: Vec<String> = (0..count)
        .map(|index| {
            // Bench fixture counters never approach 2^52, so the cast is exact.
            #[expect(
                clippy::cast_precision_loss,
                reason = "bench fixture counter far below f64's 52-bit mantissa"
            )]
            let index = index as f64;
            (start + step * index).to_string()
        })
        .collect();
    json_array(&values)
}

fn repeated(text: &str, count: usize) -> String {
    text.repeat(count)
}

fn truncate_program(items: usize) -> String {
    let stream: Vec<String> = (0..items).map(|index| format!("[[\"a\",0,0,0],{index}]")).collect();
    format!("3 | truncate_stream(({}))", stream.join(", "))
}

fn truncate_expected(program: &str) -> String {
    // Depth 3 removes the first three path segments: [\"a\",0,0,0] -> [0].
    let items = program.matches("[\"a\",0,0,0],").count();
    let mut out = String::new();
    for index in 0..items {
        let _ = writeln!(out, "[[0],{index}]");
    }
    out
}

fn scores_case(
    name: &'static str,
    program: &'static str,
    check_input: &'static str,
    check_expected: &'static str,
    timed_count: usize,
) -> Result<EngineCase, String> {
    let timed = json_object(&[("scores", ints(0, timed_count))]);
    EngineCase::new(
        name,
        u64::try_from(timed_count).unwrap_or(u64::MAX),
        timed.into_bytes(),
        check_input.as_bytes().to_vec(),
        Check::Exact(check_expected),
        program.to_owned(),
        0,
    )
}

/// (a) Key construction: builds one object from a key set, allocating every
/// key fresh and charging the entries through `try_finish` — the exact
/// record-decode path the per-record cost pool names.
struct KeyConstructionCase {
    name: &'static str,
    keys: &'static [&'static str],
    objects: u64,
    resources: ResourceContext<'static>,
}

impl KeyConstructionCase {
    fn build_object(keys: &[&str], _resources: &ResourceContext<'_>) -> Result<Object, String> {
        let mut builder =
            ObjectBuilder::try_with_capacity(keys.len()).map_err(|error| format!("key bench builder: {error:?}"))?;
        for key in keys {
            let key = ObjectKey::try_from_str(key).map_err(|error| format!("key bench key: {error:?}"))?;
            builder
                .try_insert_last(key, Value::Null)
                .map_err(|error| format!("key bench insert: {error:?}"))?;
        }
        builder
            .try_finish()
            .map_err(|error| format!("key bench finish: {error:?}"))
    }
}

impl BenchmarkCase for KeyConstructionCase {
    fn metadata(&self) -> CaseMetadata {
        let key_bytes = self.keys.iter().map(|key| key.len()).sum::<usize>() as u64;
        CaseMetadata::new(self.name, self.objects, self.objects.saturating_mul(key_bytes))
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let object = Self::build_object(self.keys, &self.resources)?;
        if object.len() != self.keys.len() {
            return Err(format!(
                "{}: expected {} entries, got {}",
                self.name,
                self.keys.len(),
                object.len()
            ));
        }
        for (index, entry) in object.iter().enumerate() {
            if entry.key() != self.keys[index] {
                return Err(format!(
                    "{}: entry {index} key {:?} != {:?}",
                    self.name,
                    entry.key(),
                    self.keys[index]
                ));
            }
        }
        Ok(PreflightReceipt::new(
            fnv1a(self.keys.join("\0").as_bytes()),
            format!("keys={} objects={}", self.keys.len(), self.objects),
        ))
    }

    fn run(&mut self) -> u64 {
        let mut hash = fnv1a(b"key-bench");
        for index in 0..self.objects {
            let object = Self::build_object(self.keys, &self.resources)
                .expect("preflighted key-construction case failed during timing");
            for entry in &object {
                hash ^= entry.key().len() as u64;
                hash = hash.rotate_left(13);
            }
            hash ^= index;
        }
        hash
    }
}

/// (b) Reader byte throughput: decodes one byte profile through the engine
/// reader (`json_sequence`), the same reader the `inputs` lanes drive.
struct ReaderThroughputCase {
    name: &'static str,
    input: String,
    /// A small known-shape document the preflight decodes first: proof the
    /// reader answers a legible input before the timed bytes are trusted.
    check_input: String,
    /// Pinned [`checksum_values`] digest over the TIMED input's decoded
    /// values. The timed run's own digest is only black-boxed, so without
    /// this pin a decoder change that altered what the lane read would move
    /// the number silently instead of failing preflight.
    expected_digest: u64,
    expected_count: u64,
    resources: ResourceContext<'static>,
}

/// Pinned timed-input digests for the reader lanes, in lane order
/// (`string-1m`, `number-1m`, `whitespace-1m`). Re-pin with the lane name and
/// the drift cause whenever the reader's value projection legitimately
/// changes.
const READER_LANE_DIGESTS: [u64; 3] = [0x2171_2355_2ad9_b8ed, 0x4892_3ca9_cc08_cbed, 0x4892_3ca9_cc08_cbed];

/// Deterministic digest over a decoded value list: mixes each value's kind
/// and, for strings, its byte length. Cheap, and it genuinely consumes the
/// decoded values so the timed decode cannot be elided.
fn checksum_values(values: &[Value]) -> u64 {
    let mut hash = fnv1a(b"reader-bench");
    for value in values {
        hash ^= u64::from(value.kind() as u8);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        if let Value::String(text) = value.untagged() {
            hash ^= text.as_str().len() as u64;
        }
    }
    hash
}

impl ReaderThroughputCase {
    fn new(
        name: &'static str,
        check_text: &str,
        timed_text: &str,
        operations: u64,
        resources: ResourceContext<'static>,
        expected_digest: u64,
    ) -> Result<Self, String> {
        // The small input must decode cleanly, or the lane is mis-authored.
        let _ = decode_sequence(check_text, &resources)?;
        Ok(Self {
            name,
            input: timed_text.to_owned(),
            check_input: check_text.to_owned(),
            expected_digest,
            expected_count: operations,
            resources,
        })
    }
}

fn decode_sequence(text: &str, resources: &ResourceContext<'_>) -> Result<Vec<Value>, String> {
    jqf_engine::decode_json_sequence(text, resources).map_err(|error| format!("reader bench decode: {error:?}"))
}

impl BenchmarkCase for ReaderThroughputCase {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new(self.name, self.expected_count, self.input.len() as u64)
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let _ = decode_sequence(&self.check_input, &self.resources)?;
        let values = decode_sequence(&self.input, &self.resources)?;
        if values.len() as u64 != self.expected_count {
            return Err(format!(
                "{}: decoded {} values, want {}",
                self.name,
                values.len(),
                self.expected_count
            ));
        }
        let checksum = checksum_values(&values);
        if checksum != self.expected_digest {
            return Err(format!(
                "{}: timed decode digest drifted (got {checksum:#x}, want {:#x})",
                self.name, self.expected_digest
            ));
        }
        Ok(PreflightReceipt::new(
            checksum,
            format!("values={} bytes={}", values.len(), self.input.len()),
        ))
    }

    fn run(&mut self) -> u64 {
        let values =
            decode_sequence(&self.input, &self.resources).expect("preflighted reader case failed during timing");
        checksum_values(&values)
    }
}

/// The lane inventory is a flat declarative table: every lane's registration
/// stays on view in one place, which is worth more than the line count.
#[expect(
    clippy::too_many_lines,
    clippy::vec_init_then_push,
    reason = "flat lane-inventory table; splitting it would obscure the closed case list"
)]
pub(crate) fn cases() -> Vec<Box<dyn BenchmarkCase>> {
    let mut out: Vec<Box<dyn BenchmarkCase>> = Vec::new();

    // Stats batch 2 (jqf extension laws; exact small-fixture preflights).
    out.push(Box::new(
        scores_case(
            "stats/median-10k",
            "median(.scores)",
            "{\"scores\":[1,2,9]}",
            "2\n",
            10_000,
        )
        .expect("stats/median-10k"),
    ));
    out.push(Box::new(
        scores_case(
            "stats/quantile-10k",
            "quantile(.scores; 0.25)",
            "{\"scores\":[1,2,3,4]}",
            "1.75\n",
            10_000,
        )
        .expect("stats/quantile-10k"),
    ));
    out.push(Box::new(
        scores_case(
            "stats/stddev-10k",
            "stddev(.scores)",
            "{\"scores\":[2,4]}",
            "1\n",
            10_000,
        )
        .expect("stats/stddev-10k"),
    ));
    out.push(Box::new(
        scores_case(
            "stats/variance-10k",
            "variance(.scores)",
            "{\"scores\":[2,4]}",
            "1\n",
            10_000,
        )
        .expect("stats/variance-10k"),
    ));
    out.push(Box::new(
        scores_case(
            "stats/count-10k",
            "count(.scores)",
            "{\"scores\":[1,2,3]}",
            "3\n",
            10_000,
        )
        .expect("stats/count-10k"),
    ));

    // Set algebra (jqf extension laws).
    // a = odd 1..999, b = even 2..998: disjoint, so union is 1..999,
    // intersect is empty, and except is the odds.
    let a_odd: Vec<String> = (1..=999).step_by(2).map(|v| v.to_string()).collect();
    let b_even: Vec<String> = (2..=998).step_by(2).map(|v| v.to_string()).collect();
    let set_timed = json_object(&[("a", json_array(&a_odd)), ("b", json_array(&b_even))]);
    out.push(Box::new(
        EngineCase::new(
            "set/union-1k",
            1_000,
            set_timed.clone().into_bytes(),
            b"{\"a\":[1,3,5],\"b\":[2,3,6]}".to_vec(),
            Check::Exact("[1,2,3,5,6]\n"),
            "union(.a; .b)".to_owned(),
            0,
        )
        .expect("set/union-1k"),
    ));
    out.push(Box::new(
        EngineCase::new(
            "set/intersect-1k",
            1_000,
            set_timed.clone().into_bytes(),
            b"{\"a\":[1,3,5],\"b\":[2,3,6]}".to_vec(),
            Check::Exact("[3]\n"),
            "intersect(.a; .b)".to_owned(),
            0,
        )
        .expect("set/intersect-1k"),
    ));
    out.push(Box::new(
        EngineCase::new(
            "set/except-1k",
            1_000,
            set_timed.into_bytes(),
            b"{\"a\":[1,3,5],\"b\":[2,3,6]}".to_vec(),
            Check::Exact("[1,5]\n"),
            "except(.a; .b)".to_owned(),
            0,
        )
        .expect("set/except-1k"),
    ));

    // UUID laws: parse is exact; v4/v7 generation is structural.
    let uuid = "123e4567-e89b-12d3-a456-426614174000";
    let ids: Vec<String> = (0..1_000).map(|_| format!("\"{uuid}\"")).collect();
    let ids_timed = json_object(&[("ids", json_array(&ids))]);
    out.push(Box::new(
        EngineCase::new(
            "uuid/parse-1k",
            1_000,
            ids_timed.into_bytes(),
            b"{\"ids\":[\"123e4567-e89b-12d3-a456-426614174000\"]}".to_vec(),
            Check::Exact("[\"123e4567-e89b-12d3-a456-426614174000\"]\n"),
            "[.ids[] | uuid]".to_owned(),
            0,
        )
        .expect("uuid/parse-1k"),
    ));
    let v_timed = json_object(&[("v", ints(0, 1_000))]);
    let v4 = v_timed.clone();
    out.push(Box::new(
        EngineCase::new(
            "uuid/v4-1k",
            1_000,
            v4.clone().into_bytes(),
            v4.into_bytes(),
            Check::UuidVersion(4),
            ".v[] | uuid_v4".to_owned(),
            0,
        )
        .expect("uuid/v4-1k"),
    ));
    let v7 = v_timed.clone();
    out.push(Box::new(
        EngineCase::new(
            "uuid/v7-1k",
            1_000,
            v7.clone().into_bytes(),
            v7.into_bytes(),
            Check::UuidVersion(7),
            ".v[] | uuid_v7".to_owned(),
            0,
        )
        .expect("uuid/v7-1k"),
    ));

    // Hashing and hex laws over a 256 KiB text.
    let text = repeated("a", 262_144);
    let hash_timed = json_object(&[("text", format!("\"{text}\""))]);
    let hashes: &[(&str, &str, &str)] = &[
        ("hash/md5-256k", "md5", "[\"900150983cd24fb0d6963f7d28e17f72\"]\n"),
        (
            "hash/sha1-256k",
            "sha1",
            "[\"a9993e364706816aba3e25717850c26c9cd0d89d\"]\n",
        ),
        (
            "hash/sha256-256k",
            "sha256",
            "[\"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad\"]\n",
        ),
        (
            "hash/sha512-256k",
            "sha512",
            "[\"ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f\"]\n",
        ),
        ("hash/xxhash-256k", "xxhash", "[\"78af5f94892f3950\"]\n"),
    ];
    for (name, builtin, expected) in hashes {
        out.push(Box::new(
            EngineCase::new(
                name,
                1,
                hash_timed.clone().into_bytes(),
                b"{\"text\":\"abc\"}".to_vec(),
                Check::Exact(expected),
                format!("[.text | {builtin}]"),
                0,
            )
            .expect(name),
        ));
    }
    let hex_text = repeated("61", 262_144);
    out.push(Box::new(
        EngineCase::new(
            "hex/encode-256k",
            1,
            hash_timed.into_bytes(),
            b"{\"text\":\"hello\"}".to_vec(),
            Check::Exact("[\"68656c6c6f\"]\n"),
            "[.text | hex_encode]".to_owned(),
            0,
        )
        .expect("hex/encode-256k"),
    ));
    let hex_timed = json_object(&[("hex", format!("\"{hex_text}\""))]);
    out.push(Box::new(
        EngineCase::new(
            "hex/decode-512k",
            1,
            hex_timed.into_bytes(),
            b"{\"hex\":\"68656c6c6f\"}".to_vec(),
            Check::Exact("[\"hello\"]\n"),
            "[.hex | hex_decode]".to_owned(),
            0,
        )
        .expect("hex/decode-512k"),
    ));

    // Bessel laws over 10k floats (yn avoids zero, where the platform boundary
    // value is not part of the measured lane).
    let bessel_timed = json_object(&[("v", floats(0.5, 0.01, 10_000))]);
    let bessel_checks: &[(&str, &str, &str)] = &[
        ("bessel/j0-10k", "j0", "[1,0.9384698072408129,0.7651976865579666]\n"),
        ("bessel/j1-10k", "j1", "[0,0.2422684576748739,0.4400505857449335]\n"),
        (
            "bessel/jn-10k",
            "jn(2; .)",
            "[0,0.03060402345868264,0.11490348493190049]\n",
        ),
        (
            "bessel/yn-10k",
            "yn(2; .)",
            "[-1.7976931348623157e+308,-5.441370837174267,-1.6506826068162543]\n",
        ),
    ];
    for (name, law, expected) in bessel_checks {
        out.push(Box::new(
            EngineCase::new(
                name,
                10_000,
                bessel_timed.clone().into_bytes(),
                b"{\"v\":[0.0,0.5,1.0]}".to_vec(),
                Check::Exact(expected),
                format!("[.v[] | {law}]"),
                0,
            )
            .expect(name),
        ));
    }
    // y0/y1 avoid zero, where the platform boundary value is not part of the
    // measured lane (the shared bessel check input starts at 0.0, so these two
    // carry their own check fixture; expected vectors verified byte-identical
    // against jq 1.8.2 on the same libm).
    let bessel_y_checks: &[(&str, &str, &str)] = &[
        (
            "bessel/y0-10k",
            "y0",
            "[-0.44451873350670656,0.08825696421567697,0.510375672649745]\n",
        ),
        (
            "bessel/y1-10k",
            "y1",
            "[-1.4714723926702433,-0.7812128213002887,-0.10703243154093756]\n",
        ),
    ];
    for (name, law, expected) in bessel_y_checks {
        out.push(Box::new(
            EngineCase::new(
                name,
                10_000,
                bessel_timed.clone().into_bytes(),
                b"{\"v\":[0.5,1.0,2.0]}".to_vec(),
                Check::Exact(expected),
                format!("[.v[] | {law}]"),
                0,
            )
            .expect(name),
        ));
    }

    // Math extras over 10k values.
    let math_timed = json_object(&[("v", floats(-2.5, 0.001, 10_000))]);
    let math_checks: &[(&str, &str, &str, &str)] = &[
        (
            "math/round-even-10k",
            "round_even",
            "{\"v\":[-2.5,-1.5,-0.5,0.5,1.5,2.5]}",
            "[-2,-2,-0,0,2,2]\n",
        ),
        ("math/signum-10k", "signum", "{\"v\":[-3.0,0.0,4.0]}", "[-1,0,1]\n"),
        (
            "math/fract-10k",
            "fract",
            "{\"v\":[-1.75,0.25,2.5]}",
            "[-0.75,0.25,0.5]\n",
        ),
        ("math/pow10-10k", "pow10", "{\"v\":[0.0,1.0,2.0]}", "[1,10,100]\n"),
        ("math/recip-10k", "recip", "{\"v\":[1.0,2.0,4.0]}", "[1,0.5,0.25]\n"),
        (
            "math/degrees-10k",
            "degrees",
            "{\"v\":[0.0,90.0,180.0]}",
            "[0,5156.620156177409,10313.240312354817]\n",
        ),
        (
            "math/radians-10k",
            "radians",
            "{\"v\":[0.0,1.5707963267948966,3.141592653589793]}",
            "[0,0.027415567780803774,0.05483113556160755]\n",
        ),
        (
            "math/constants-pi-10k",
            "pi",
            "{\"v\":[0,1]}",
            "[3.141592653589793,3.141592653589793]\n",
        ),
    ];
    for (name, law, check_input, expected) in math_checks {
        let timed = if *name == "math/recip-10k" {
            json_object(&[("v", floats(0.5, 0.001, 10_000))])
        } else {
            math_timed.clone()
        };
        out.push(Box::new(
            EngineCase::new(
                name,
                10_000,
                timed.into_bytes(),
                check_input.as_bytes().to_vec(),
                Check::Exact(expected),
                format!("[.v[] | {law}]"),
                0,
            )
            .expect(name),
        ));
    }

    // ------------------------------------------------------------------
    // Computed-integer churn instrument. The two-arm Number's target pool:
    // a tight arithmetic loop constructs and drops one number per step
    // (the pre-S1 Arc alloc/free), and the result is rendered once at the
    // end — isolating construct/drop from spelling. The LANES remain the
    // acceptance metric.
    // ------------------------------------------------------------------
    out.push(Box::new(
        EngineCase::new(
            "numbers/counter-100k",
            100_000,
            b"null".to_vec(),
            b"null".to_vec(),
            Check::Exact("100000\n"),
            "reduce range(0; 100000) as $i (0; . + 1)".to_owned(),
            0,
        )
        .expect("numbers/counter-100k"),
    ));
    out.push(Box::new(
        EngineCase::new(
            "numbers/add-loop-100k",
            100_000,
            b"null".to_vec(),
            b"null".to_vec(),
            Check::Exact("4999950000\n"),
            "reduce range(0; 100000) as $i (0; . + $i)".to_owned(),
            0,
        )
        .expect("numbers/add-loop-100k"),
    ));

    // Attribution instruments — key construction and reader throughput.
    // These are MICROBENCHMARKS, not lanes: each isolates one of the two
    // per-record cost pools (key allocation; byte-at-a-time reading), so a
    // later change can attribute a lane improvement to a pool. The LANES are
    // the acceptance metric; these instruments only say WHERE a change
    // worked.
    // ------------------------------------------------------------------

    // (a) Key construction: `objects` objects over the SAME small key set,
    // every key allocated fresh per object — the pool
    // `ObjectKey::try_from_str` pays in a record stream that re-reads one
    // key set forever. Values are `null`: the instrument isolates the KEY
    // cost; value bodies are S3 territory.
    let key_build = KeyConstructionCase {
        name: "key-construction/13-keys-100k",
        // The users_ndjson_medium record's top-level key set, verbatim from
        // the generated fixture — the key allocations one record pays.
        keys: &[
            "id",
            "name",
            "email",
            "age",
            "active",
            "score",
            "tier",
            "country",
            "balance",
            "signup_days_ago",
            "tags",
            "profile",
            "metrics",
        ],
        objects: 100_000,
        resources: resources(),
    };
    out.push(Box::new(key_build));

    // (b) Reader byte throughput: the engine reader over the three byte
    // profiles — string bodies, digit runs, separator skipping. Each is
    // ~1 MiB / 256 values so the three shapes cost the same bytes and differ
    // only in what the reader does per byte.
    let string_value = format!("\"{}\"", "a".repeat(4096));
    let string_timed = vec![string_value.as_str(); 256].join("\n");
    out.push(Box::new(
        ReaderThroughputCase::new(
            "reader/string-1m",
            "\"aa\"\n\"bbb\"\n\"cccc\"",
            &string_timed,
            256,
            resources(),
            READER_LANE_DIGESTS[0],
        )
        .expect("reader/string-1m"),
    ));
    let number_timed = vec!["9".repeat(4096).as_str(); 256].join("\n");
    out.push(Box::new(
        ReaderThroughputCase::new(
            "reader/number-1m",
            "12\n345\n6789",
            &number_timed,
            256,
            resources(),
            READER_LANE_DIGESTS[1],
        )
        .expect("reader/number-1m"),
    ));
    let whitespace_timed = vec!["1"; 256].join(&" ".repeat(4096));
    out.push(Box::new(
        ReaderThroughputCase::new(
            "reader/whitespace-1m",
            "1    2      3",
            &whitespace_timed,
            256,
            resources(),
            READER_LANE_DIGESTS[2],
        )
        .expect("reader/whitespace-1m"),
    ));

    // Streams: tostream/fromstream over a generated 1k-row document, and
    // truncate_stream over a generated 1k-item literal stream.
    let rows: Vec<String> = (0..1_000)
        .map(|index| {
            format!(
                "{{\"id\":{index},\"name\":\"n-{index}\",\"tags\":[\"t{}\",\"t{}\"]}}",
                index % 3,
                (index + 1) % 3
            )
        })
        .collect();
    let stream_timed = json_object(&[("rows", json_array(&rows))]);
    out.push(Box::new(
        EngineCase::new(
            "streams/tostream-1k",
            1,
            stream_timed.clone().into_bytes(),
            b"{\"a\":[1,2],\"b\":3}".to_vec(),
            Check::Exact("[[\"a\",0],1]\n[[\"a\",1],2]\n[[\"a\",1]]\n[[\"b\"],3]\n[[\"b\"]]\n"),
            "tostream".to_owned(),
            0,
        )
        .expect("streams/tostream-1k"),
    ));
    out.push(Box::new(
        EngineCase::new(
            "streams/fromstream-1k",
            1,
            stream_timed.into_bytes(),
            b"{\"a\":[1,2],\"b\":3}".to_vec(),
            Check::Exact("{\"a\":[1,2],\"b\":3}\n"),
            "fromstream(tostream)".to_owned(),
            0,
        )
        .expect("streams/fromstream-1k"),
    ));
    let truncate = truncate_program(1_000);
    out.push(Box::new(
        EngineCase::new(
            "streams/truncate-1k",
            1_000,
            b"null".to_vec(),
            b"null".to_vec(),
            Check::TruncateStream,
            truncate,
            0,
        )
        .expect("streams/truncate-1k"),
    ));

    // Item 99: any/all isempty/first/label expansion vs a reduce that
    // computes the same boolean. Extra wall on the `all` lane over the
    // reduce twin is the Quantify go-ahead (a 2 ms no-op is a skip).
    let quantify_timed = ints(1, 50_000);
    out.push(Box::new(
        EngineCase::new(
            "any-all/all-true-50k",
            50_000,
            quantify_timed.clone().into_bytes(),
            b"[1,2,3]".to_vec(),
            Check::Exact("true\n"),
            "all(. > 0)".to_owned(),
            0,
        )
        .expect("any-all/all-true-50k"),
    ));
    out.push(Box::new(
        EngineCase::new(
            "any-all/any-false-50k",
            50_000,
            quantify_timed.clone().into_bytes(),
            b"[1,2,3]".to_vec(),
            Check::Exact("false\n"),
            "any(. < 0)".to_owned(),
            0,
        )
        .expect("any-all/any-false-50k"),
    ));
    out.push(Box::new(
        EngineCase::new(
            "any-all/reduce-true-50k",
            50_000,
            quantify_timed.into_bytes(),
            b"[1,2,3]".to_vec(),
            Check::Exact("true\n"),
            "reduce .[] as $x (true; . and ($x > 0))".to_owned(),
            0,
        )
        .expect("any-all/reduce-true-50k"),
    ));

    out
}
