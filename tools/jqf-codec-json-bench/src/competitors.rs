//! Same-fixture external JSON implementation comparison lanes.

use std::hint::black_box;

use jqf_bench_core::{BenchmarkCase, CaseMetadata, PreflightReceipt};
use jqf_data::{
    AccountedDocumentBuilder, AccountedOccurrenceKey, AccountedSemanticNode, BuilderCoverage, Decimal,
    DocumentCapacity, LocalOwnerRef, NodeId,
};
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use simd_json::prelude::*;
use sonic_rs::{JsonContainerTrait as _, JsonValueTrait as _};

use crate::{fixtures, semantic};

#[derive(Clone, Copy)]
enum Decoder {
    Serde,
    SimdBorrowedImmutable,
    SimdSerdeImmutable,
    Sonic,
}

impl Decoder {
    const fn label(self) -> &'static str {
        match self {
            Self::Serde => "serde-json-value",
            Self::SimdBorrowedImmutable => "simd-json-borrowed-immutable-copy",
            Self::SimdSerdeImmutable => "simd-json-serde-value-immutable-copy",
            Self::Sonic => "sonic-rs-value",
        }
    }
}

#[derive(Clone, Copy)]
enum DecodeOperation {
    Full,
    ForwardSelection,
}

struct DecodeCase {
    name: &'static str,
    source: String,
    decoder: Decoder,
    operation: DecodeOperation,
    expected_semantic: u64,
    fixture: fixtures::FixtureEvidence,
}

impl DecodeCase {
    fn run_once(&self, validate: bool) -> u64 {
        if validate {
            fixtures::verify_fixture(self.fixture, self.source.as_bytes());
        }
        match self.decoder {
            Decoder::Serde => {
                let value: serde_json::Value =
                    serde_json::from_slice(black_box(self.source.as_bytes())).expect("serde JSON");
                let selected =
                    matches!(self.operation, DecodeOperation::ForwardSelection).then(|| selected_serde(&value));
                if validate {
                    assert_eq!(semantic::serde_value(&value), self.expected_semantic);
                    if let Some(selected) = selected {
                        assert_eq!(selected, "item-511");
                    }
                }
                black_box(&value);
                self.source.len() as u64 ^ selected.map_or(0, |value| value.len() as u64) ^ 0x5e_ed
            }
            Decoder::SimdBorrowedImmutable => {
                // simd-json requires mutable input and may unescape in place. The copy is
                // intentionally timed because jqf's public source is immutable.
                let mut input = black_box(self.source.as_bytes().to_vec());
                let value = simd_json::borrowed::to_value(&mut input).expect("simd JSON");
                let selected =
                    matches!(self.operation, DecodeOperation::ForwardSelection).then(|| selected_simd(&value));
                if validate {
                    let projected = serde_json::to_value(&value).expect("simd semantic projection");
                    assert_eq!(
                        semantic::serde_value(&projected),
                        self.expected_semantic,
                        "{} semantic projection",
                        self.name
                    );
                    if let Some(selected) = selected {
                        assert_eq!(selected, "item-511");
                    }
                }
                black_box(&value);
                self.source.len() as u64 ^ selected.map_or(0, |value| value.len() as u64) ^ 0x51_d0
            }
            Decoder::SimdSerdeImmutable => {
                let mut input = black_box(self.source.as_bytes().to_vec());
                let value: serde_json::Value =
                    simd_json::serde::from_slice(&mut input).expect("simd JSON through Serde");
                let selected =
                    matches!(self.operation, DecodeOperation::ForwardSelection).then(|| selected_serde(&value));
                if validate {
                    assert_eq!(
                        semantic::serde_value(&value),
                        self.expected_semantic,
                        "{} semantic projection",
                        self.name
                    );
                    if let Some(selected) = selected {
                        assert_eq!(selected, "item-511");
                    }
                }
                black_box(&value);
                self.source.len() as u64 ^ selected.map_or(0, |value| value.len() as u64) ^ 0x51_d5
            }
            Decoder::Sonic => {
                let value: sonic_rs::Value =
                    sonic_rs::from_slice(black_box(self.source.as_bytes())).expect("Sonic JSON");
                let selected =
                    matches!(self.operation, DecodeOperation::ForwardSelection).then(|| selected_sonic(&value));
                if validate {
                    let projected_bytes = sonic_rs::to_vec(&value).expect("Sonic projection");
                    let projected: serde_json::Value =
                        serde_json::from_slice(&projected_bytes).expect("Sonic semantic JSON");
                    assert_eq!(semantic::serde_value(&projected), self.expected_semantic);
                    if let Some(selected) = selected {
                        assert_eq!(selected, "item-511");
                    }
                }
                black_box(&value);
                self.source.len() as u64 ^ selected.map_or(0, |value| value.len() as u64) ^ 0x50_1c
            }
        }
    }
}

impl BenchmarkCase for DecodeCase {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new(self.name, 1, self.source.len() as u64)
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let physical_checksum = self.run_once(true);
        let operation = match self.operation {
            DecodeOperation::Full => "full-dom",
            DecodeOperation::ForwardSelection => "full-dom-field-access",
        };
        Ok(PreflightReceipt::new(
            physical_checksum,
            format!(
                "{} fixture_id={} fixture_revision={} fixture_hash={:016x} lane_status=Executable fairness={} competitor={} operation={} exact_fixture=true strict_eof=true semantic_projection=last-value-wins simd_input_copy_timed={} source_hash={:016x} semantic_checksum_schema={} semantic_checksum=0x{:016x} physical_checksum=0x{physical_checksum:016x}",
                fixtures::provenance(),
                self.fixture.id,
                self.fixture.revision,
                self.fixture.hash,
                match self.operation {
                    DecodeOperation::Full => "construction-comparable-competitor-semantic-dom",
                    DecodeOperation::ForwardSelection => "use-case-comparable-competitor-full-dom-field-access",
                },
                self.decoder.label(),
                operation,
                matches!(
                    self.decoder,
                    Decoder::SimdBorrowedImmutable | Decoder::SimdSerdeImmutable
                ),
                fixtures::verify_fixture(self.fixture, self.source.as_bytes()),
                semantic::SCHEMA,
                self.expected_semantic,
            ),
        ))
    }

    fn run(&mut self) -> u64 {
        self.run_once(false)
    }
}

/// Same-product frontend lanes: the competitor parses the fixture into its own
/// DOM, then authors the SAME minimal-semantic accounted `Document` the jqf
/// `json/minimal-semantic/*` lane produces in one fused pass, through the same
/// `AccountedDocumentBuilder` the JSON codec drives.
///
/// The trio (competitor full-dom lane, this lane, jqf minimal-semantic lane)
/// decomposes the capability tax empirically: this lane minus the full-dom lane
/// is the authoring/accounting cost priced onto a competitor front-end, and the
/// jqf lane's distance from this one is the fused single-pass advantage or
/// deficit. Differences from jqf's own decode, both inherent to the frontends:
/// strings become stored copies (no source binding, so no blake3 seal — the
/// Stage 5 copy-vs-seal trade), and numbers re-normalize from the competitor's
/// binary form, not the source spelling. The wide-duplicates fixture is
/// deliberately not registered: serde/sonic DOMs collapse duplicate keys at
/// parse, so no frontend can author that fixture's occurrence stream.
#[derive(Clone, Copy)]
enum Frontend {
    Serde,
    Sonic,
}

impl Frontend {
    const fn label(self) -> &'static str {
        match self {
            Self::Serde => "serde-json-value",
            Self::Sonic => "sonic-rs-value",
        }
    }
}

const DOCUMENT_ARRAY_ROLE: &str = "json.array.item@1";
const DOCUMENT_OBJECT_ROLE: &str = "json.object.member@1";

struct ToDocumentCase {
    name: &'static str,
    source: String,
    frontend: Frontend,
    expected_semantic: u64,
    fixture: fixtures::FixtureEvidence,
}

impl ToDocumentCase {
    fn run_once(&self, validate: bool) -> u64 {
        static CONTROL: ContinueControl = ContinueControl;
        if validate {
            fixtures::verify_fixture(self.fixture, self.source.as_bytes());
        }
        let (input, output, memory, spill, depth) = jqf_bench_core::limits::MEASURED_REGION;
        let resources = ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(input, output, memory, spill, depth)).expect("account"),
            &CONTROL,
            WorkMeter::try_new_v1(4096).expect("meter"),
        )
        .expect("context");
        let mut builder = AccountedDocumentBuilder::try_new_with_coverage(
            "json",
            Some("rfc8259"),
            BuilderCoverage::minimal_semantic(),
        )
        .expect("builder");
        // The same /40 whole-source capacity heuristic the JSON parser uses.
        builder
            .try_reserve(
                DocumentCapacity {
                    nodes: self.source.len() / 40,
                    occurrences: self.source.len() / 40,
                    ..DocumentCapacity::default()
                },
                &resources,
            )
            .expect("reserve");
        let document = match self.frontend {
            Frontend::Serde => {
                let value: serde_json::Value =
                    serde_json::from_slice(black_box(self.source.as_bytes())).expect("serde JSON");
                let root = serde_shallow(&mut builder, &value, &resources);
                serde_fill(&mut builder, root, &value, &resources);
                builder.finish(root, &resources).expect("finish")
            }
            Frontend::Sonic => {
                let value: sonic_rs::Value =
                    sonic_rs::from_slice(black_box(self.source.as_bytes())).expect("Sonic JSON");
                let root = sonic_shallow(&mut builder, &value, &resources);
                sonic_fill(&mut builder, root, &value, &resources);
                builder.finish(root, &resources).expect("finish")
            }
        };
        if validate {
            let value = document
                .materialize_root(&mut crate::cases::bench_ledger())
                .expect("materialize same-product witness");
            assert_eq!(
                semantic::jqf_value(&value),
                self.expected_semantic,
                "{} same-product witness",
                self.name,
            );
        }
        let decoded_arena_len = document
            .text_storage_stats()
            .map_or(0, |stats| stats.decoded_arena_len as u64);
        self.source.len() as u64 ^ document.key().get() ^ decoded_arena_len ^ 0xd0c5
    }
}

impl BenchmarkCase for ToDocumentCase {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new(self.name, 1, self.source.len() as u64)
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let physical_checksum = self.run_once(true);
        Ok(PreflightReceipt::new(
            physical_checksum,
            format!(
                "{} fixture_id={} fixture_revision={} fixture_hash={:016x} lane_status=Executable fairness=same-product-competitor-frontend-minimal-semantic-document competitor={} operation=parse-plus-accounted-document-build exact_fixture=true strict_eof=true text=stored-copies source_binding=none semantic_projection=last-value-wins source_hash={:016x} semantic_checksum_schema={} semantic_checksum=0x{:016x} physical_checksum=0x{physical_checksum:016x}",
                fixtures::provenance(),
                self.fixture.id,
                self.fixture.revision,
                self.fixture.hash,
                self.frontend.label(),
                fixtures::verify_fixture(self.fixture, self.source.as_bytes()),
                semantic::SCHEMA,
                self.expected_semantic,
            ),
        ))
    }

    fn run(&mut self) -> u64 {
        self.run_once(false)
    }
}

fn serde_shallow(
    builder: &mut AccountedDocumentBuilder<'static>,
    value: &serde_json::Value,
    resources: &ResourceContext<'_>,
) -> NodeId {
    match value {
        serde_json::Value::Null => builder.add_node("json.null@1", AccountedSemanticNode::Null, None, resources),
        serde_json::Value::Bool(value) => {
            builder.add_node("json.bool@1", AccountedSemanticNode::Bool(*value), None, resources)
        }
        serde_json::Value::Number(value) => {
            return number_node(builder, &value.to_string(), resources);
        }
        serde_json::Value::String(value) => {
            builder.add_node("json.string@1", AccountedSemanticNode::String(value), None, resources)
        }
        serde_json::Value::Array(_) => builder.add_node(
            "json.array@1",
            AccountedSemanticNode::Array {
                item_role: DOCUMENT_ARRAY_ROLE,
            },
            None,
            resources,
        ),
        serde_json::Value::Object(_) => builder.add_node(
            "json.object@1",
            AccountedSemanticNode::Object {
                member_role: DOCUMENT_OBJECT_ROLE,
            },
            None,
            resources,
        ),
    }
    .expect("author node")
}

/// Parser document order: child node, its owner occurrence, then its subtree.
fn serde_fill(
    builder: &mut AccountedDocumentBuilder<'static>,
    node: NodeId,
    value: &serde_json::Value,
    resources: &ResourceContext<'_>,
) {
    match value {
        serde_json::Value::Array(items) => {
            for item in items {
                let child = serde_shallow(builder, item, resources);
                builder
                    .add_occurrence(LocalOwnerRef::Node(node), DOCUMENT_ARRAY_ROLE, None, child, resources)
                    .expect("array item");
                serde_fill(builder, child, item, resources);
            }
        }
        serde_json::Value::Object(members) => {
            for (key, member) in members {
                let child = serde_shallow(builder, member, resources);
                builder
                    .add_occurrence(
                        LocalOwnerRef::Node(node),
                        DOCUMENT_OBJECT_ROLE,
                        Some(AccountedOccurrenceKey::Text(key)),
                        child,
                        resources,
                    )
                    .expect("object member");
                serde_fill(builder, child, member, resources);
            }
        }
        _ => {}
    }
}

fn sonic_shallow(
    builder: &mut AccountedDocumentBuilder<'static>,
    value: &sonic_rs::Value,
    resources: &ResourceContext<'_>,
) -> NodeId {
    if value.is_null() {
        builder.add_node("json.null@1", AccountedSemanticNode::Null, None, resources)
    } else if let Some(value) = value.as_bool() {
        builder.add_node("json.bool@1", AccountedSemanticNode::Bool(value), None, resources)
    } else if let Some(text) = value.as_str() {
        builder.add_node("json.string@1", AccountedSemanticNode::String(text), None, resources)
    } else if value.is_number() {
        return number_node(builder, &sonic_number_spelling(value), resources);
    } else if value.is_array() {
        builder.add_node(
            "json.array@1",
            AccountedSemanticNode::Array {
                item_role: DOCUMENT_ARRAY_ROLE,
            },
            None,
            resources,
        )
    } else {
        builder.add_node(
            "json.object@1",
            AccountedSemanticNode::Object {
                member_role: DOCUMENT_OBJECT_ROLE,
            },
            None,
            resources,
        )
    }
    .expect("author node")
}

fn sonic_fill(
    builder: &mut AccountedDocumentBuilder<'static>,
    node: NodeId,
    value: &sonic_rs::Value,
    resources: &ResourceContext<'_>,
) {
    if let Some(items) = value.as_array() {
        for item in items {
            let child = sonic_shallow(builder, item, resources);
            builder
                .add_occurrence(LocalOwnerRef::Node(node), DOCUMENT_ARRAY_ROLE, None, child, resources)
                .expect("array item");
            sonic_fill(builder, child, item, resources);
        }
    } else if let Some(members) = value.as_object() {
        for (key, member) in members {
            let child = sonic_shallow(builder, member, resources);
            builder
                .add_occurrence(
                    LocalOwnerRef::Node(node),
                    DOCUMENT_OBJECT_ROLE,
                    Some(AccountedOccurrenceKey::Text(key)),
                    child,
                    resources,
                )
                .expect("object member");
            sonic_fill(builder, child, member, resources);
        }
    }
}

fn sonic_number_spelling(value: &sonic_rs::Value) -> String {
    if let Some(value) = value.as_i64() {
        value.to_string()
    } else if let Some(value) = value.as_u64() {
        value.to_string()
    } else {
        serde_json::Number::from_f64(value.as_f64().expect("finite JSON number"))
            .expect("finite JSON number spelling")
            .to_string()
    }
}

/// Integer spellings take the parser's integer arm; everything else
/// re-normalizes to the exact coefficient/scale the parser derives from the
/// source spelling (`Decimal::parse` accepts every JSON number form).
fn number_node(
    builder: &mut AccountedDocumentBuilder<'static>,
    spelling: &str,
    resources: &ResourceContext<'_>,
) -> NodeId {
    if spelling.bytes().all(|byte| byte.is_ascii_digit() || byte == b'-') {
        builder.add_node(
            "json.number@1",
            AccountedSemanticNode::Integer(spelling),
            None,
            resources,
        )
    } else {
        let decimal = Decimal::parse(spelling).expect("exact decimal spelling");
        builder.add_node(
            "json.number@1",
            AccountedSemanticNode::Decimal {
                coefficient: decimal.coefficient().as_str(),
                scale: decimal.scale(),
            },
            None,
            resources,
        )
    }
    .expect("author number node")
}

#[derive(Clone, Copy)]
enum Encoder {
    Serde,
    Simd,
    Sonic,
}

impl Encoder {
    const fn label(self) -> &'static str {
        match self {
            Self::Serde => "serde-json",
            Self::Simd => "simd-json-serde",
            Self::Sonic => "sonic-rs-native",
        }
    }
}

struct EncodeCase {
    name: &'static str,
    encoder: Encoder,
    serde_value: serde_json::Value,
    sonic_value: sonic_rs::Value,
    expected: String,
    fixture: fixtures::FixtureEvidence,
    fairness: &'static str,
}

impl EncodeCase {
    fn run_once(&self, validate: bool) -> u64 {
        if validate {
            fixtures::verify_fixture(self.fixture, self.expected.as_bytes());
        }
        let output = match self.encoder {
            Encoder::Serde => serde_json::to_vec(black_box(&self.serde_value)).expect("serde encode"),
            Encoder::Simd => simd_json::serde::to_vec(black_box(&self.serde_value)).expect("simd-json encode"),
            Encoder::Sonic => sonic_rs::to_vec(black_box(&self.sonic_value)).expect("Sonic encode"),
        };
        if validate {
            assert_eq!(output, self.expected.as_bytes());
        }
        simple_hash(black_box(&output))
    }
}

impl BenchmarkCase for EncodeCase {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new(self.name, 1, self.expected.len() as u64)
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let physical_checksum = self.run_once(true);
        let semantic_checksum = semantic::serde_value(&self.serde_value);
        Ok(PreflightReceipt::new(
            physical_checksum,
            format!(
                "{} fixture_id={} fixture_revision={} fixture_hash={:016x} lane_status=Executable fairness={} competitor={} operation=semantic-value-encode exact_fixture=true exact_bytes=true source_hash={:016x} semantic_checksum_schema={} semantic_checksum=0x{semantic_checksum:016x} physical_checksum=0x{physical_checksum:016x}",
                fixtures::provenance(),
                self.fixture.id,
                self.fixture.revision,
                self.fixture.hash,
                self.fairness,
                self.encoder.label(),
                fixtures::verify_fixture(self.fixture, self.expected.as_bytes()),
                semantic::SCHEMA,
            ),
        ))
    }

    fn run(&mut self) -> u64 {
        self.run_once(false)
    }
}

pub(crate) fn extend_cases(
    cases: &mut Vec<Box<dyn BenchmarkCase>>,
    encoded_fixture: &str,
    deep_64_fixture: &str,
    deep_256_fixture: &str,
) {
    extend_decode_fixture(
        cases,
        &fixtures::nested(),
        fixtures::NESTED,
        [
            "compare/serde-json/full-nested",
            "compare/simd-json/full-nested",
            "compare/sonic-rs/full-nested",
        ],
        DecodeOperation::Full,
        Decoder::SimdBorrowedImmutable,
    );
    extend_decode_fixture(
        cases,
        &fixtures::nested(),
        fixtures::NESTED,
        [
            "compare/serde-json/select-forward-nested",
            "compare/simd-json/select-forward-nested",
            "compare/sonic-rs/select-forward-nested",
        ],
        DecodeOperation::ForwardSelection,
        Decoder::SimdBorrowedImmutable,
    );
    extend_decode_fixture(
        cases,
        &fixtures::escapes(),
        fixtures::ESCAPES,
        [
            "compare/serde-json/full-escape-heavy",
            "compare/simd-json/full-escape-heavy",
            "compare/sonic-rs/full-escape-heavy",
        ],
        DecodeOperation::Full,
        Decoder::SimdBorrowedImmutable,
    );
    extend_decode_fixture(
        cases,
        &fixtures::wide_duplicate_object(),
        fixtures::WIDE_DUPLICATES,
        [
            "compare/serde-json/full-wide-4096-duplicates",
            "compare/simd-json/full-wide-4096-duplicates",
            "compare/sonic-rs/full-wide-4096-duplicates",
        ],
        DecodeOperation::Full,
        Decoder::SimdSerdeImmutable,
    );

    extend_to_document_fixture(
        cases,
        &fixtures::nested(),
        fixtures::NESTED,
        [
            "compare/serde-json/to-document-nested",
            "compare/sonic-rs/to-document-nested",
        ],
    );
    extend_to_document_fixture(
        cases,
        &fixtures::escapes(),
        fixtures::ESCAPES,
        [
            "compare/serde-json/to-document-escape-heavy",
            "compare/sonic-rs/to-document-escape-heavy",
        ],
    );

    extend_encode_fixture(
        cases,
        encoded_fixture,
        fixtures::ENCODE_NESTED,
        [
            "compare/serde-json/encode-owned-nested",
            "compare/simd-json/encode-owned-nested",
            "compare/sonic-rs/encode-owned-nested",
        ],
    );
    extend_deep_encode_fixture(
        cases,
        64,
        deep_64_fixture,
        fixtures::ENCODE_DEEP_64,
        [
            "compare/serde-json/encode-owned-deep-64",
            "compare/simd-json/encode-owned-deep-64",
            "compare/sonic-rs/encode-owned-deep-64",
        ],
    );
    extend_deep_encode_fixture(
        cases,
        256,
        deep_256_fixture,
        fixtures::ENCODE_DEEP_256,
        [
            "compare/serde-json/encode-owned-deep-256",
            "compare/simd-json/encode-owned-deep-256",
            "compare/sonic-rs/encode-owned-deep-256",
        ],
    );
}

fn extend_encode_fixture(
    cases: &mut Vec<Box<dyn BenchmarkCase>>,
    fixture: &str,
    evidence: fixtures::FixtureEvidence,
    names: [&'static str; 3],
) {
    let serde_value: serde_json::Value = serde_json::from_str(fixture).expect("encode fixture as serde value");
    let sonic_value: sonic_rs::Value = sonic_rs::from_str(fixture).expect("encode fixture as Sonic value");
    push_encode_cases(cases, fixture, evidence, names, &serde_value, &sonic_value);
}

fn extend_deep_encode_fixture(
    cases: &mut Vec<Box<dyn BenchmarkCase>>,
    depth: usize,
    fixture: &str,
    evidence: fixtures::FixtureEvidence,
    names: [&'static str; 3],
) {
    let mut serde_value = serde_json::Value::Null;
    let mut sonic_value = sonic_rs::Value::new_null();
    for _ in 0..depth {
        serde_value = serde_json::Value::Array(vec![serde_value]);
        let mut parent = sonic_rs::Value::new_array_with(1);
        parent.append_value(sonic_value);
        sonic_value = parent;
    }
    push_encode_cases(cases, fixture, evidence, names, &serde_value, &sonic_value);
}

fn push_encode_cases(
    cases: &mut Vec<Box<dyn BenchmarkCase>>,
    fixture: &str,
    evidence: fixtures::FixtureEvidence,
    names: [&'static str; 3],
    serde_value: &serde_json::Value,
    sonic_value: &sonic_rs::Value,
) {
    for (name, encoder) in [
        (names[0], Encoder::Serde),
        (names[1], Encoder::Simd),
        (names[2], Encoder::Sonic),
    ] {
        cases.push(Box::new(EncodeCase {
            name,
            encoder,
            serde_value: serde_value.clone(),
            sonic_value: sonic_value.clone(),
            expected: fixture.to_owned(),
            fixture: evidence,
            fairness: if evidence.id == fixtures::ENCODE_NESTED.id {
                "owned-semantic-value-encode-competitor"
            } else {
                "owned-semantic-value-encode-deep-competitor"
            },
        }));
    }
}

fn extend_to_document_fixture(
    cases: &mut Vec<Box<dyn BenchmarkCase>>,
    source: &str,
    fixture: fixtures::FixtureEvidence,
    names: [&'static str; 2],
) {
    let oracle: serde_json::Value = serde_json::from_str(source).expect("comparison oracle");
    let expected_semantic = semantic::serde_value(&oracle);
    for (name, frontend) in [(names[0], Frontend::Serde), (names[1], Frontend::Sonic)] {
        cases.push(Box::new(ToDocumentCase {
            name,
            source: source.to_owned(),
            frontend,
            expected_semantic,
            fixture,
        }));
    }
}

fn extend_decode_fixture(
    cases: &mut Vec<Box<dyn BenchmarkCase>>,
    source: &str,
    fixture: fixtures::FixtureEvidence,
    names: [&'static str; 3],
    operation: DecodeOperation,
    simd_decoder: Decoder,
) {
    let oracle: serde_json::Value = serde_json::from_str(source).expect("comparison oracle");
    let expected_semantic = semantic::serde_value(&oracle);
    for (name, decoder) in [
        (names[0], Decoder::Serde),
        (names[1], simd_decoder),
        (names[2], Decoder::Sonic),
    ] {
        cases.push(Box::new(DecodeCase {
            name,
            source: source.to_owned(),
            decoder,
            operation,
            expected_semantic,
            fixture,
        }));
    }
}

fn selected_serde(value: &serde_json::Value) -> &str {
    value["catalog"][511]["name"].as_str().expect("serde selected string")
}

fn selected_simd<'value>(value: &'value simd_json::BorrowedValue<'_>) -> &'value str {
    value
        .get("catalog")
        .and_then(|value| value.get_idx(511))
        .and_then(|value| value.get("name"))
        .and_then(ValueAsScalar::as_str)
        .expect("simd selected string")
}

fn selected_sonic(value: &sonic_rs::Value) -> &str {
    value
        .pointer(sonic_rs::pointer!["catalog", 511, "name"])
        .and_then(|value| value.as_str())
        .expect("Sonic selected string")
}

fn simple_hash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3)
    })
}
