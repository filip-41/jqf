use std::fmt::Write as _;

use jqf_bench_core::{BenchmarkCase, CaseMetadata, PreflightReceipt};
use jqf_codec_core::{
    AccessAdapter, AccessFootprint, AccessGuarantees, AccessOutcome, AccessRequirement, CodecDemand, DecodeRequest,
    DemandClause, DiagnosticPolicy, EncodeItem, EncodeRequest, ExactPath, ExactSelectionRecord, PreservationRequest,
    ValidationMode,
};
use jqf_data::{Array, DialectId, FormatId, Integer, Number, NumberView, ObjectBuilder, ScalarView, Value};
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

use crate::{competitors, fixtures, semantic};

static BENCH_CONTROL: ContinueControl = ContinueControl;

/// One unlimited ledger for a fixture or preflight value.
///
/// An owned payload takes its ledger residency on the ALLOCATION now, so no
/// `Value` builds and no document materializes without an account. Fixtures and
/// preflights run outside the measured region, so a throwaway ledger is the
/// honest scope: the residency's own refcount keeps the account alive as long as
/// the value does.
pub(crate) fn bench_ledger() -> ResourceContext<'static> {
    let (input, output, memory, spill, depth) = jqf_bench_core::limits::FIXTURE_BUILD;
    let account =
        RequestAccount::try_new(ResourceLimits::new(input, output, memory, spill, depth)).expect("bench account");
    let work = WorkMeter::try_new_v1(1).expect("bench work meter");
    ResourceContext::new(account, &BENCH_CONTROL, work).expect("bench ledger")
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum CaseKind {
    Full,
    Select,
    /// Whole-document decode demanding only mandatory semantics, so the product
    /// omits topology, facts, and provenance — the fair minimal-semantic DOM.
    MinimalSemantic,
}

struct JsonCase {
    name: &'static str,
    source: String,
    kind: CaseKind,
    fixture: fixtures::FixtureEvidence,
}

impl JsonCase {
    #[expect(
        clippy::too_many_lines,
        reason = "timed route setup and outcome ownership remain one visible operation"
    )]
    fn run_once(&self, validate: bool) -> u64 {
        static CONTROL: ContinueControl = ContinueControl;
        let (input_bytes, output_bytes, memory, spill, depth) = jqf_bench_core::limits::MEASURED_REGION;
        let mut resources = ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(input_bytes, output_bytes, memory, spill, depth))
                .expect("account"),
            &CONTROL,
            WorkMeter::try_new_v1(4096).expect("meter"),
        )
        .expect("context");
        let source = ResolvedSource::new(
            SourceRef::new(SourceId::new(91), SourceKind::Input),
            self.name,
            self.source.as_bytes(),
            0,
        );
        if validate {
            fixtures::verify_fixture(self.fixture, self.source.as_bytes());
        }
        let mut provider = jqf_codec_json::registration()
            .expect("registration")
            .decoder()
            .expect("decoder")
            .create_provider(
                source,
                DecodeRequest {
                    validation: ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect: &jqf_data::DialectId::try_new("rfc8259").expect("dialect"),
                    options: None,
                    allow_adjacent_values: false,
                    value_separator: jqf_codec_json::VALUE_SEPARATORS,
                },
                &mut resources,
            )
            .expect("provider");
        let mut demand = CodecDemand::try_new(&resources);
        demand.try_insert(&DemandClause::SemanticRoot).expect("root");
        let footprint = match self.kind {
            CaseKind::Full => AccessFootprint::try_whole(&resources),
            CaseKind::MinimalSemantic => {
                demand.try_insert(&DemandClause::ValueShape).expect("shape");
                AccessFootprint::try_whole(&resources)
            }
            CaseKind::Select => {
                demand.try_insert(&DemandClause::ValueShape).expect("shape");
                let mut path = ExactPath::try_new(&resources);
                path.try_push_semantic_member("catalog", &resources).expect("key");
                path.try_push_semantic_index(511, &resources);
                path.try_push_semantic_member("name", &resources).expect("key");
                AccessFootprint::try_exact(path, &resources)
            }
        };
        let requirement = if footprint.is_whole() {
            AccessRequirement::try_whole(
                demand,
                AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
                &resources,
            )
        } else {
            AccessRequirement::try_exact(
                footprint,
                demand,
                AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
                &resources,
            )
        }
        .expect("requirement")
        // The bench lanes model filter drives: nothing edits, splices, or
        // losslessly re-encodes the decoded document, so the authored-span
        // demand is withdrawn — the C38 hint an SDK filter drive will set.
        .with_authored_spans(false);
        let handle = provider.bind(&requirement).expect("bind");
        let mut session = provider.open(&handle, &mut resources).expect("open");
        let receipt = session.physical_route_receipt().expect("sealed physical route receipt");
        let (expected_route, expected_slot) = match self.kind {
            CaseKind::Full | CaseKind::MinimalSemantic => (jqf_codec_json::FULL_PHYSICAL_ROUTE_ID, 0),
            CaseKind::Select => (jqf_codec_json::SCOPED_PHYSICAL_ROUTE_ID, 1),
        };
        assert_eq!(receipt.route(), expected_route);
        assert_eq!(receipt.slot().get(), expected_slot);
        assert_ne!(receipt.provider_id(), 0);
        {
            let mut run = jqf_codec_core::CodecRunContext::new(&mut resources);
            run.set_cooperative_credits(4096);
            let result = session.decode(&mut run).expect("decode");
            let (outcome, report) = result.into_parts();
            assert_eq!(report.route(), Some(receipt));
            match outcome {
                AccessOutcome::FullDocument(product) => {
                    assert_eq!(report.adapter(), AccessAdapter::None);
                    if validate {
                        self.validate_full(product.document());
                        let value = product
                            .document()
                            .materialize_root(&mut bench_ledger())
                            .expect("materialize semantic preflight value");
                        assert_eq!(
                            semantic::jqf_value(&value),
                            semantic_checksum(&self.source),
                            "{} semantic witness",
                            self.name,
                        );
                    }
                    let stats = product
                        .document()
                        .text_storage_stats()
                        .expect("complete JSON product exposes text statistics");
                    self.source.len() as u64
                        ^ product.document().key().get()
                        ^ stats.decoded_arena_len as u64
                        ^ receipt.route().get()
                        ^ 0xf011
                }
                AccessOutcome::Located(located) => {
                    // The scoped route Direct-binds (no adapter) and
                    // materializes only the selected subtree, so the
                    // located product's root IS the selected value.
                    assert_eq!(report.adapter(), AccessAdapter::None);
                    let ExactSelectionRecord::Node { node, .. } = located.result() else {
                        return 0;
                    };
                    if validate {
                        assert_eq!(
                            string_value(located.product().document().value_view(*node).expect("selected view")),
                            "item-511"
                        );
                        // The subtree product materializes exactly the
                        // selected value, not the whole document.
                        let value = located
                            .product()
                            .document()
                            .materialize_root(&mut bench_ledger())
                            .expect("materialize scoped subtree value");
                        assert!(
                            matches!(&value, jqf_data::Value::String(text) if text == "item-511"),
                            "{} scoped subtree witness",
                            self.name,
                        );
                    }
                    self.source.len() as u64 ^ node.document().get() ^ receipt.route().get() ^ 0x005e_1ec7
                }
            }
        }
    }

    fn validate_full(&self, document: &jqf_data::Document<'_>) {
        let root = document.value_view(document.root_handle()).expect("root view");
        if self.fixture.id == fixtures::ESCAPES.id {
            let array = root.array().expect("array view").expect("array");
            assert_eq!(array.len(), 1_024);
            for index in [0, 1_023] {
                assert_eq!(
                    string_value(array.get(index).expect("present")),
                    "line\nquote\"slash\\music𝄞"
                );
            }
            return;
        }
        if self.fixture.id == fixtures::WIDE_DUPLICATES.id {
            let object = root.object().expect("object view").expect("object");
            assert_eq!(object.len(), 2_048);
            let mut entries = object.iter();
            assert_eq!(
                entries.next().expect("first entry").expect("first key").key(),
                "key-2047"
            );
            assert_eq!(entries.last().expect("last entry").expect("last key").key(), "key-0000");
            for (key, expected) in [("key-0000", "2048"), ("key-2047", "4095")] {
                let scalar = object
                    .get(key)
                    .expect("wide member")
                    .scalar()
                    .expect("wide scalar")
                    .expect("wide value");
                assert!(matches!(scalar, ScalarView::Number(NumberView::Integer(value)) if value == expected));
            }
            return;
        }
        let object = root.object().expect("object view").expect("object");
        assert_eq!(object.len(), 2);
        let catalog = object
            .get("catalog")
            .expect("catalog")
            .array()
            .expect("catalog array view")
            .expect("catalog array");
        assert_eq!(catalog.len(), 512);
        let last = catalog
            .get(511)
            .expect("catalog last")
            .object()
            .expect("item object view")
            .expect("item object");
        assert_eq!(last.len(), 4);
        assert_eq!(string_value(last.get("name").expect("name")), "item-511");
    }
}

fn string_value<'document>(view: jqf_data::ValueView<'document, '_>) -> &'document str {
    match view.scalar().expect("scalar view") {
        Some(ScalarView::String(value)) => value,
        other => panic!("expected string, got {other:?}"),
    }
}

impl BenchmarkCase for JsonCase {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new(self.name, 1, self.source.len() as u64)
    }
    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let physical_checksum = self.run_once(true);
        if physical_checksum == 0 {
            return Err("zero strict-JSON route receipt".into());
        }
        let semantic_checksum = semantic_checksum(&self.source);
        let route = match self.kind {
            CaseKind::Full => "full-document",
            CaseKind::MinimalSemantic => "minimal-semantic-document",
            CaseKind::Select => "scoped-exact-path",
        };
        let (physical_route, slot) = match self.kind {
            CaseKind::Full | CaseKind::MinimalSemantic => (jqf_codec_json::FULL_PHYSICAL_ROUTE_ID, 0),
            CaseKind::Select => (jqf_codec_json::SCOPED_PHYSICAL_ROUTE_ID, 1),
        };
        let fairness = match self.kind {
            CaseKind::Full => "legacy-rich-complete-authority-orientation",
            CaseKind::MinimalSemantic => "construction-comparable-minimal-semantic-dom",
            CaseKind::Select => "scoped-exact-path-subtree-materialization",
        };
        let mut detail = format!(
            "{} fixture_id={} fixture_revision={} fixture_hash={:016x} lane_status=Executable fairness={fairness} route={route} physical_route=0x{:016x} sealed_slot={slot} executed_receipt=true source_hash={:016x} semantic_checksum_schema={} semantic_checksum=0x{semantic_checksum:016x} physical_checksum=0x{physical_checksum:016x}",
            fixtures::provenance(),
            self.fixture.id,
            self.fixture.revision,
            self.fixture.hash,
            physical_route.get(),
            fixtures::verify_fixture(self.fixture, self.source.as_bytes()),
            semantic::SCHEMA,
        );
        if self.kind == CaseKind::MinimalSemantic {
            let materialized_bytes = materialized_semantic_bytes(&self.source);
            write!(
                detail,
                " demand=minimal-semantic omitted=topology,facts,provenance validated_bytes={} materialized_bytes={materialized_bytes}",
                self.source.len(),
            )
            .expect("receipt string append");
        }
        Ok(PreflightReceipt::new(physical_checksum, detail))
    }
    fn run(&mut self) -> u64 {
        self.run_once(false)
    }
}

struct JsonEncodeCase {
    name: &'static str,
    value: Value,
    expected: String,
    semantic_probe: &'static str,
    minimum_offers: u64,
    format: FormatId,
    dialect: DialectId,
    logical_bytes: u64,
    fixture: fixtures::FixtureEvidence,
    fairness: &'static str,
}

/// The encode lane's byte sink: counts the encoder's chunk boundaries (the
/// poll-era `offers` receipt — each bounded flush is one offer), folds the
/// bytes into the lane checksum, appends them to the caller-owned output, and
/// commits the output reservation against request resources.
struct EncodingSink<'a> {
    target: Option<&'a mut Vec<u8>>,
    checksum: &'a mut u64,
    offers: &'a mut u64,
}
impl jqf_codec_core::ByteSink for EncodingSink<'_> {
    fn write(
        &mut self,
        bytes: &[u8],
        resources: &mut ResourceContext<'_>,
    ) -> Result<usize, jqf_codec_core::CodecError> {
        *self.offers += 1;
        let mut running = *self.checksum;
        for byte in bytes {
            running = (running ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3);
        }
        *self.checksum = running;
        if let Some(target) = self.target.as_mut() {
            target.extend_from_slice(bytes);
        }
        let written = u64::try_from(bytes.len())
            .map_err(|_| jqf_codec_core::CodecError::new(jqf_codec_core::CodecFailureKind::Overflow))?;
        resources
            .reserve_output(written)
            .map_err(jqf_codec_core::CodecError::from)?
            .commit(written)
            .map_err(jqf_codec_core::CodecError::from)?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> Result<(), jqf_codec_core::CodecError> {
        Ok(())
    }
}

impl JsonEncodeCase {
    fn run_once(&self, validate: bool) -> u64 {
        static CONTROL: ContinueControl = ContinueControl;
        if validate {
            fixtures::verify_fixture(self.fixture, self.expected.as_bytes());
        }
        let (input_bytes, output_bytes, memory, spill, depth) = jqf_bench_core::limits::MEASURED_REGION;
        let mut resources = ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(input_bytes, output_bytes, memory, spill, depth))
                .expect("account"),
            &CONTROL,
            WorkMeter::try_new_v1(4096).expect("meter"),
        )
        .expect("context");
        let request = EncodeRequest {
            format: &self.format,
            dialect: &self.dialect,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            preservation: PreservationRequest::Report,
            options: None,
        };
        let factory = jqf_codec_json::registration()
            .expect("registration")
            .encoder()
            .expect("encoder")
            .create_factory(request, &mut resources)
            .expect("factory");
        let mut session = factory
            .start(
                EncodeItem::owned(&self.value),
                PreservationRequest::Report,
                &mut resources,
            )
            .expect("session");
        assert_eq!(session.physical_encoder(), jqf_codec_json::ENCODE_PHYSICAL_ROUTE_ID);
        let mut checksum = 0xcbf2_9ce4_8422_2325_u64;
        let mut output = validate.then(Vec::new);
        let mut offers = 0_u64;
        {
            let mut sink = EncodingSink {
                target: output.as_mut(),
                checksum: &mut checksum,
                offers: &mut offers,
            };
            let mut run = jqf_codec_core::CodecRunContext::new(&mut resources);
            run.set_cooperative_credits(4096);
            session.encode(&mut sink, &mut run).expect("encode");
        }
        if let Some(output) = output {
            assert_eq!(output, self.expected.as_bytes());
            assert!(self.expected.contains(self.semantic_probe));
            assert!(offers >= self.minimum_offers);
            assert_eq!(resources.snapshot().output_bytes(), output.len() as u64);
            assert_eq!(output.len() as u64, self.logical_bytes);
        }
        assert!(session.report().is_some());
        checksum ^ session.physical_encoder().get() ^ offers
    }
}

impl BenchmarkCase for JsonEncodeCase {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new(self.name, 1, self.logical_bytes)
    }
    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let physical_checksum = self.run_once(true);
        let semantic_checksum = semantic::jqf_value(&self.value);
        Ok(PreflightReceipt::new(
            physical_checksum,
            format!(
                "{} fixture_id={} fixture_revision={} fixture_hash={:016x} lane_status=Executable fairness={} route=encode-owned physical_encoder=0x{:016x} executed_receipt=true semantic_checksum_schema={} semantic_checksum=0x{semantic_checksum:016x} physical_checksum=0x{physical_checksum:016x}",
                fixtures::provenance(),
                self.fixture.id,
                self.fixture.revision,
                self.fixture.hash,
                self.fairness,
                jqf_codec_json::ENCODE_PHYSICAL_ROUTE_ID.get(),
                semantic::SCHEMA,
            ),
        ))
    }
    fn run(&mut self) -> u64 {
        self.run_once(false)
    }
}

fn semantic_checksum(source: &str) -> u64 {
    let value: serde_json::Value = serde_json::from_str(source).expect("semantic JSON oracle");
    semantic::serde_value(&value)
}

/// Canonical byte length of the materialized semantic value, used by the
/// minimal-semantic receipt to size the fair DOM against a competitor DOM.
fn materialized_semantic_bytes(source: &str) -> usize {
    let value: serde_json::Value = serde_json::from_str(source).expect("semantic JSON oracle");
    serde_json::to_string(&value).expect("re-encode semantic value").len()
}

fn encode_fixture() -> (Value, String) {
    let _resources = bench_ledger();
    let mut values = Vec::with_capacity(1_024);
    let mut expected = String::from(r#"{"catalog":["#);
    for index in 0..1_024 {
        let id = index.to_string();
        let name = format!("item-{index}");
        if index != 0 {
            expected.push(',');
        }
        write!(expected, r#"{{"id":{id},"name":"{name}"}}"#).expect("expected JSON");
        let mut item = ObjectBuilder::try_with_capacity(2).expect("item object");
        item.try_insert_last(
            jqf_data::ObjectKey::try_from_str("id").expect("fixture key"),
            Value::Number(Number::integer(Integer::parse(&id).expect("integer"))),
        )
        .expect("id");
        item.try_insert_last(
            jqf_data::ObjectKey::try_from_str("name").expect("fixture key"),
            Value::try_string(&name).expect("name text"),
        )
        .expect("name");
        values.push(Value::Object(item.try_finish().expect("item finish")));
    }
    let mut root = ObjectBuilder::try_with_capacity(2).expect("root object");
    root.try_insert_last(
        jqf_data::ObjectKey::try_from_str("catalog").expect("fixture key"),
        Value::Array(Array::try_from_vec(values).expect("array")),
    )
    .expect("catalog");
    root.try_insert_last(
        jqf_data::ObjectKey::try_from_str("active").expect("fixture key"),
        Value::Bool(true),
    )
    .expect("active");
    expected.push_str(r#"],"active":true}"#);
    (Value::Object(root.try_finish().expect("root finish")), expected)
}

fn deep_encode_fixture(depth: usize) -> (Value, String) {
    let _resources = bench_ledger();
    let mut value = Value::Null;
    for _ in 0..depth {
        value = Value::Array(Array::try_from_vec(vec![value]).expect("deep array"));
    }
    let mut expected = "[".repeat(depth);
    expected.push_str("null");
    expected.push_str(&"]".repeat(depth));
    (value, expected)
}

/// Dense escapes on the short-string (<= `TEXT_QUANTUM`) path: 1024 small rows
/// whose escapes are frequent, so the encoder's escape scan runs per row.
fn escape_dense_encode_fixture() -> (Value, String) {
    let _resources = bench_ledger();
    let mut values = Vec::with_capacity(1_024);
    let mut expected = String::from(r#"{"rows":["#);
    for index in 0..1_024 {
        let row = format!("line {index}\t\"quoted\"\nnext\tline");
        let escaped = row
            .replace('\\', "\\\\")
            .replace('\t', "\\t")
            .replace('"', "\\\"")
            .replace('\n', "\\n");
        if index != 0 {
            expected.push(',');
        }
        expected.push('"');
        expected.push_str(&escaped);
        expected.push('"');
        values.push(Value::try_string(&row).expect("row text"));
    }
    expected.push_str("]}");
    let mut root = ObjectBuilder::try_with_capacity(1).expect("root");
    root.try_insert_last(
        jqf_data::ObjectKey::try_from_str("rows").expect("rows key"),
        Value::Array(Array::try_from_vec(values).expect("array")),
    )
    .expect("rows");
    (Value::Object(root.try_finish().expect("root finish")), expected)
}

/// Sparse escapes on the long-string (streaming `Phase::Text`) path: 64 rows
/// of ~2 KiB with a handful of escapes each, so the escape scan runs per
/// 256-byte chunk over mostly-plain text.
fn escape_sparse_encode_fixture() -> (Value, String) {
    let _resources = bench_ledger();
    let mut values = Vec::with_capacity(64);
    let mut expected = String::from(r#"{"rows":["#);
    for index in 0..64 {
        let mut row = String::with_capacity(2_048);
        for position in 0..512 {
            row.push(if position % 128 == 0 { '"' } else { 'a' });
        }
        let _ = std::fmt::Write::write_fmt(&mut row, format_args!("\n{index}"));
        let escaped = row.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n");
        if index != 0 {
            expected.push(',');
        }
        expected.push('"');
        expected.push_str(&escaped);
        expected.push('"');
        values.push(Value::try_string(&row).expect("row text"));
    }
    expected.push_str("]}");
    let mut root = ObjectBuilder::try_with_capacity(1).expect("root");
    root.try_insert_last(
        jqf_data::ObjectKey::try_from_str("rows").expect("rows key"),
        Value::Array(Array::try_from_vec(values).expect("array")),
    )
    .expect("rows");
    (Value::Object(root.try_finish().expect("root finish")), expected)
}

#[allow(
    clippy::too_many_lines,
    reason = "the case table is one declaration: each lane's fixture and competitor setup sits               beside the case that consumes it, which splitting would separate"
)]
pub(crate) fn cases() -> Vec<Box<dyn BenchmarkCase>> {
    let (encode_value, expected) = encode_fixture();
    let competitor_encode_fixture = expected.clone();
    let encode_bytes = expected.len() as u64;
    let (deep_64, deep_64_expected) = deep_encode_fixture(64);
    let (deep_256, deep_256_expected) = deep_encode_fixture(256);
    let (escape_dense, escape_dense_expected) = escape_dense_encode_fixture();
    let (escape_sparse, escape_sparse_expected) = escape_sparse_encode_fixture();
    let competitor_deep_64_fixture = deep_64_expected.clone();
    let competitor_deep_256_fixture = deep_256_expected.clone();
    let mut cases: Vec<Box<dyn BenchmarkCase>> = vec![
        Box::new(JsonCase {
            name: "json/full-nested",
            source: fixtures::nested(),
            kind: CaseKind::Full,
            fixture: fixtures::NESTED,
        }),
        Box::new(JsonCase {
            name: "json/select-forward-nested",
            source: fixtures::nested(),
            kind: CaseKind::Select,
            fixture: fixtures::NESTED,
        }),
        Box::new(JsonCase {
            name: "json/full-escape-heavy",
            source: fixtures::escapes(),
            kind: CaseKind::Full,
            fixture: fixtures::ESCAPES,
        }),
        Box::new(JsonCase {
            name: "json/full-wide-4096-duplicates",
            source: fixtures::wide_duplicate_object(),
            kind: CaseKind::Full,
            fixture: fixtures::WIDE_DUPLICATES,
        }),
        Box::new(JsonCase {
            name: "json/minimal-semantic/full-nested",
            source: fixtures::nested(),
            kind: CaseKind::MinimalSemantic,
            fixture: fixtures::NESTED,
        }),
        Box::new(JsonCase {
            name: "json/minimal-semantic/full-escape-heavy",
            source: fixtures::escapes(),
            kind: CaseKind::MinimalSemantic,
            fixture: fixtures::ESCAPES,
        }),
        Box::new(JsonCase {
            name: "json/minimal-semantic/full-wide-4096-duplicates",
            source: fixtures::wide_duplicate_object(),
            kind: CaseKind::MinimalSemantic,
            fixture: fixtures::WIDE_DUPLICATES,
        }),
        Box::new(JsonEncodeCase {
            name: "json/encode-owned-nested",
            value: encode_value,
            expected,
            semantic_probe: r#"{"id":512,"name":"item-512"}"#,
            minimum_offers: 2,
            format: FormatId::try_new(jqf_codec_json::FORMAT_ID).expect("format"),
            dialect: DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect"),
            logical_bytes: encode_bytes,
            fixture: fixtures::ENCODE_NESTED,
            fairness: "owned-semantic-value-encode",
        }),
        Box::new(JsonEncodeCase {
            name: "json/encode-owned-deep-64",
            value: deep_64,
            semantic_probe: "null",
            minimum_offers: 1,
            logical_bytes: deep_64_expected.len() as u64,
            expected: deep_64_expected,
            format: FormatId::try_new(jqf_codec_json::FORMAT_ID).expect("format"),
            dialect: DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect"),
            fixture: fixtures::ENCODE_DEEP_64,
            fairness: "owned-semantic-value-encode-deep",
        }),
        Box::new(JsonEncodeCase {
            name: "json/encode-owned-deep-256",
            value: deep_256,
            semantic_probe: "null",
            minimum_offers: 1,
            logical_bytes: deep_256_expected.len() as u64,
            expected: deep_256_expected,
            format: FormatId::try_new(jqf_codec_json::FORMAT_ID).expect("format"),
            dialect: DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect"),
            fixture: fixtures::ENCODE_DEEP_256,
            fairness: "owned-semantic-value-encode-deep",
        }),
        Box::new(JsonEncodeCase {
            name: "json/encode-owned-escape-dense-short",
            value: escape_dense,
            semantic_probe: r#"{"rows":["line 0\t\"quoted\"\nnext\tline","#,
            minimum_offers: 2,
            logical_bytes: escape_dense_expected.len() as u64,
            expected: escape_dense_expected,
            format: FormatId::try_new(jqf_codec_json::FORMAT_ID).expect("format"),
            dialect: DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect"),
            fixture: fixtures::ENCODE_ESCAPE_DENSE,
            fairness: "owned-semantic-value-encode-escape",
        }),
        Box::new(JsonEncodeCase {
            name: "json/encode-owned-escape-sparse-long",
            value: escape_sparse,
            semantic_probe: r#"{"rows":["\"aaa"#,
            minimum_offers: 2,
            logical_bytes: escape_sparse_expected.len() as u64,
            expected: escape_sparse_expected,
            format: FormatId::try_new(jqf_codec_json::FORMAT_ID).expect("format"),
            dialect: DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect"),
            fixture: fixtures::ENCODE_ESCAPE_SPARSE,
            fairness: "owned-semantic-value-encode-escape",
        }),
    ];
    competitors::extend_cases(
        &mut cases,
        &competitor_encode_fixture,
        &competitor_deep_64_fixture,
        &competitor_deep_256_fixture,
    );
    cases
}
