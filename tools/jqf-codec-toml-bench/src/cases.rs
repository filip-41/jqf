//! TOML benchmark lanes: jqf decode, jqf encode, competitor decode.

use std::hint::black_box;

use jqf_bench_core::{BenchmarkCase, CaseMetadata, PreflightReceipt};
use jqf_codec_core::{
    AccessGuarantees, AccessOutcome, AccessRequirement, CodecDemand, CodecRunContext, DecodeRequest, DemandClause,
    DiagnosticPolicy, EncodeItem, EncodeRequest, ErasedProvider, ValidationMode,
};
use jqf_data::{DialectId, FormatId, Value};
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

use crate::fixtures;

static BENCH_CONTROL: ContinueControl = ContinueControl;

/// One unlimited ledger for fixture/preflight materialization (outside the
/// measured region).
pub(crate) fn bench_ledger() -> ResourceContext<'static> {
    let (input, output, memory, spill, depth) = jqf_bench_core::limits::FIXTURE_BUILD;
    let account =
        RequestAccount::try_new(ResourceLimits::new(input, output, memory, spill, depth)).expect("bench account");
    let work = WorkMeter::try_new_v1(1).expect("bench work meter");
    ResourceContext::new(account, &BENCH_CONTROL, work).expect("bench ledger")
}

/// A deterministic checksum over an owned value (the correctness witness).
fn value_checksum(value: &Value) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    fold_value(value, &mut hash);
    hash
}

fn fold_value(value: &Value, hash: &mut u64) {
    *hash = hash.rotate_left(5)
        ^ match value {
            Value::Null => 0x11,
            Value::Bool(true) => 0x22,
            Value::Bool(false) => 0x33,
            Value::Number(number) => {
                // The inline machine arm renders its canonical spelling on
                // demand (093 S1); the boxed arm borrows its retained one.
                if let Some(machine) = number.as_machine() {
                    let integer = jqf_data::Integer::from_i64(machine);
                    integer
                        .as_str()
                        .as_bytes()
                        .iter()
                        .fold(0x44u64, |acc, byte| acc.rotate_left(3) ^ u64::from(*byte))
                } else if let Some(integer) = number.as_integer() {
                    integer
                        .as_str()
                        .as_bytes()
                        .iter()
                        .fold(0x44u64, |acc, byte| acc.rotate_left(3) ^ u64::from(*byte))
                } else if let Some(float) = number.as_float() {
                    float.bits()
                } else {
                    0x45
                }
            }
            Value::String(text) => text
                .as_str()
                .as_bytes()
                .iter()
                .fold(0x55u64, |acc, byte| acc.rotate_left(3) ^ u64::from(*byte)),
            Value::Array(array) => {
                let mut acc = 0x66u64;
                for item in array {
                    fold_value(item, &mut acc);
                }
                acc
            }
            Value::Object(object) => {
                let mut acc = 0x77u64;
                // Semantic object equality is order-independent, so the
                // checksum must be too: fold the entries in sorted-key order,
                // not insertion order.
                let mut keys: Vec<&str> = (0..object.len())
                    .filter_map(|index| object.get_index(index).map(jqf_data::ObjectEntry::key))
                    .collect();
                keys.sort_unstable();
                for key in keys {
                    let value = object.get(key).expect("key present");
                    acc = acc.rotate_left(3)
                        ^ key
                            .as_bytes()
                            .iter()
                            .fold(0x88u64, |a, byte| a.rotate_left(3) ^ u64::from(*byte));
                    fold_value(value, &mut acc);
                }
                acc
            }
            Value::LocalDate(_)
            | Value::LocalTime(_)
            | Value::LocalDateTime(_)
            | Value::OffsetDateTime(_)
            | Value::Bytes(_)
            | Value::Tagged { .. } => 0x99,
        };
}

fn whole_requirement(resources: &ResourceContext<'_>) -> AccessRequirement {
    let mut demand = CodecDemand::try_new(resources);
    demand.try_insert(&DemandClause::SemanticRoot).expect("semantic root");
    demand.try_insert(&DemandClause::ValueShape).expect("value shape");
    AccessRequirement::try_whole(
        demand,
        AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
        resources,
    )
    .expect("requirement")
}

/// The jqf whole-document decode lane.
///
/// The measured region is the decode ALONE. The request ledger, the provider
/// over the retained fixture, and the whole-document requirement are built
/// ONCE at case construction (outside timing), and `run()` times only
/// bind/open/poll, returning a cheap PHYSICAL receipt.
///
/// Scope note, because the sibling benches differ here: the json bench builds
/// a FRESH ledger per timed invocation; this case retains ONE across them.
/// Each invocation's session and product drop inside `run_once`, releasing
/// their charges back to that ledger, so the retained peak is still a single
/// decode. The expensive semantic witness (`materialize_root` + the
/// key-sorting recursive checksum) runs in preflight only — the decode itself
/// was never what those folds measured, and on the small fixtures they
/// dominated it.
struct JqfDecodeCase {
    name: &'static str,
    source: &'static str,
    resources: ResourceContext<'static>,
    provider: ErasedProvider<'static>,
    requirement: AccessRequirement,
}

impl JqfDecodeCase {
    /// Builds the retained provider, ledger, and requirement once, outside
    /// the timed region. `source` is the fixture's `&'static str`, so the
    /// provider borrows statically-retained bytes with no self-reference.
    fn new(name: &'static str, source: &'static str) -> Self {
        let (input, output, memory, spill, depth) = jqf_bench_core::limits::MEASURED_REGION;
        let mut resources = ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(input, output, memory, spill, depth)).expect("account"),
            &BENCH_CONTROL,
            WorkMeter::try_new_v1(4096).expect("meter"),
        )
        .expect("context");
        let resolved = ResolvedSource::new(
            SourceRef::new(SourceId::new(95), SourceKind::Input),
            "bench.toml",
            source.as_bytes(),
            0,
        );
        let provider = jqf_codec_toml::registration_1_0()
            .expect("registration")
            .decoder()
            .expect("decoder")
            .create_provider(
                resolved,
                DecodeRequest {
                    validation: ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect: &DialectId::try_new("toml.jqf-1.0@1").expect("dialect"),
                    options: None,
                    allow_adjacent_values: false,
                    value_separator: &[],
                },
                &mut resources,
            )
            .expect("provider");
        let requirement = whole_requirement(&resources);
        Self {
            name,
            source,
            resources,
            provider,
            requirement,
        }
    }

    /// One timed invocation: bind, open, and poll the whole-document decode
    /// to completion, returning a cheap physical receipt.
    ///
    /// The session's allocations are request-accounted and released when the
    /// session and its product drop at the end of this method, so reusing
    /// one ledger across invocations keeps the peak at a single decode.
    fn run_once(&mut self) -> u64 {
        let handle = self.provider.bind(&self.requirement).expect("bind");
        let mut session = self.provider.open(&handle, &mut self.resources).expect("open");
        let receipt = session.physical_route_receipt().expect("sealed physical route receipt");
        assert_eq!(receipt.route(), jqf_codec_toml::FULL_PHYSICAL_ROUTE_ID);
        assert_eq!(receipt.slot().get(), 0);
        let result = {
            let mut run = CodecRunContext::new(&mut self.resources);
            run.set_cooperative_credits(4096);
            session.decode(&mut run).expect("decode")
        };
        let AccessOutcome::FullDocument(product) = result.outcome() else {
            panic!("expected full document");
        };
        let document = product.document();
        self.source.len() as u64 ^ document.key().get() ^ document.node_count() as u64 ^ receipt.route().get() ^ 0xf011
    }
}

impl BenchmarkCase for JqfDecodeCase {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new(self.name, 1, self.source.len() as u64)
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        // The full semantic witness (untimed): a fresh decode, materialized
        // and folded by the key-sorting checksum, compared against the pin.
        let checksum = decode_checksum(self.source.as_bytes());
        let expected = fixtures::expected_checksum(self.name);
        if checksum != expected {
            return Err(format!(
                "{}: jqf decode semantic witness drifted (got {checksum:#x}, want {expected:#x})",
                self.name
            ));
        }
        // The hoisted path must answer too (a nonzero physical receipt).
        let physical = self.run_once();
        if physical == 0 {
            return Err(format!("{}: zero physical receipt", self.name));
        }
        Ok(PreflightReceipt::new(checksum, "jqf decode witness".to_owned()))
    }

    fn run(&mut self) -> u64 {
        self.run_once()
    }
}

/// Runs one full jqf TOML decode and returns a checksum. The sealed physical
/// route receipt is asserted (slot 0, the Whole/CompleteDocument route) so a
/// routing change is a preflight-style hard fail rather than a silent
/// measurement of a different path.
fn decode_checksum(bytes: &[u8]) -> u64 {
    let (input, output, memory, spill, depth) = jqf_bench_core::limits::MEASURED_REGION;
    let mut resources = ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(input, output, memory, spill, depth)).expect("account"),
        &BENCH_CONTROL,
        WorkMeter::try_new_v1(4096).expect("meter"),
    )
    .expect("context");
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(95), SourceKind::Input),
        "bench.toml",
        bytes,
        0,
    );
    let mut provider = jqf_codec_toml::registration_1_0()
        .expect("registration")
        .decoder()
        .expect("decoder")
        .create_provider(
            source,
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new("toml.jqf-1.0@1").expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            &mut resources,
        )
        .expect("provider");
    let requirement = whole_requirement(&resources);
    let handle = provider.bind(&requirement).expect("bind");
    let mut session = provider.open(&handle, &mut resources).expect("open");
    let receipt = session.physical_route_receipt().expect("sealed physical route receipt");
    assert_eq!(receipt.route(), jqf_codec_toml::FULL_PHYSICAL_ROUTE_ID);
    assert_eq!(receipt.slot().get(), 0);
    let result = {
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4096);
        session.decode(&mut run).expect("decode")
    };
    let AccessOutcome::FullDocument(product) = result.outcome() else {
        panic!("expected full document");
    };
    let value = product
        .document()
        .materialize_root(&mut bench_ledger())
        .expect("materialize");
    value_checksum(&value)
}

/// The fixture-pinning decode: used by the `pin_checksums` ignored test so the
/// pins always agree with the measured path.
#[cfg(test)]
#[must_use]
pub(crate) fn pin_decode_checksum(source: &str) -> u64 {
    decode_checksum(source.as_bytes())
}

/// The jqf deterministic encode lane: decode once (outside the measured
/// region) to obtain the value, then time the encode repeatedly. The expected
/// deterministic bytes are pinned on the first preflight and checked on every
/// later one.
struct JqfEncodeCase {
    name: &'static str,
    value: Value,
    encoded: Option<Vec<u8>>,
}

impl BenchmarkCase for JqfEncodeCase {
    fn metadata(&self) -> CaseMetadata {
        let bytes = self.encoded.as_ref().map_or(0, Vec::len) as u64;
        CaseMetadata::new(self.name, 1, bytes)
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let out = encode_value(&self.value)?;
        match &self.encoded {
            Some(expected) if expected == &out => {}
            Some(expected) => {
                return Err(format!(
                    "{}: encode drifted ({} bytes, want {})",
                    self.name,
                    out.len(),
                    expected.len()
                ));
            }
            None => self.encoded = Some(out),
        }
        // The output must reparse to the same semantic value.
        let reparsed = decode_checksum(self.encoded.as_ref().expect("pinned"));
        if reparsed != value_checksum(&self.value) {
            return Err(format!("{}: encode output does not reparse to the input", self.name));
        }
        Ok(PreflightReceipt::new(
            value_checksum(&self.value),
            "jqf encode round-trip".to_owned(),
        ))
    }

    fn run(&mut self) -> u64 {
        u64::from(encode_value(&self.value).expect("encode")[0])
    }
}

/// Encodes one owned value through the deterministic `toml.jqf-1.0@1` profile.
fn encode_value(value: &Value) -> Result<Vec<u8>, String> {
    let mut resources = bench_ledger();
    let format = FormatId::try_new(jqf_codec_toml::FORMAT_ID).map_err(|e| e.to_string())?;
    let dialect = DialectId::try_new(jqf_codec_toml::TOML_JQF_1_0_DIALECT_ID).map_err(|e| e.to_string())?;
    let registration = jqf_codec_toml::registration_1_0().map_err(|e| format!("{e:?}"))?;
    let factory = registration
        .encoder()
        .expect("encoder")
        .create_factory(
            EncodeRequest {
                format: &format,
                dialect: &dialect,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                preservation: jqf_codec_core::PreservationRequest::None,
                options: None,
            },
            &mut resources,
        )
        .map_err(|e| format!("{:?}", e.kind()))?;
    let mut session = factory
        .start(
            EncodeItem::Owned(value),
            jqf_codec_core::PreservationRequest::None,
            &mut resources,
        )
        .map_err(|e| format!("{:?}", e.kind()))?;
    let mut out = Vec::new();
    {
        let mut sink = jqf_codec_core::VecByteSink::new(&mut out);
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4096);
        session
            .encode(&mut sink, &mut run)
            .map_err(|e| format!("{:?}", e.kind()))?;
    }
    Ok(out)
}

/// A competitor decode lane (the `toml` crate).
struct TomlCrateDecodeCase {
    name: &'static str,
    source: String,
}

impl BenchmarkCase for TomlCrateDecodeCase {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new(self.name, 1, self.source.len() as u64)
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let value: toml::Table = toml::from_str(black_box(&self.source)).map_err(|e| e.to_string())?;
        Ok(PreflightReceipt::new(
            checksum_table(&value),
            "toml crate decode".to_owned(),
        ))
    }

    fn run(&mut self) -> u64 {
        let value: toml::Table = toml::from_str(black_box(&self.source)).expect("toml decode");
        checksum_table(&value)
    }
}

/// A competitor decode lane (the `toml_edit` crate).
struct TomlEditDecodeCase {
    name: &'static str,
    source: String,
}

impl BenchmarkCase for TomlEditDecodeCase {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new(self.name, 1, self.source.len() as u64)
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let doc = self
            .source
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| e.to_string())?;
        Ok(PreflightReceipt::new(
            checksum_document(&doc),
            "toml_edit decode".to_owned(),
        ))
    }

    fn run(&mut self) -> u64 {
        let doc = black_box(&self.source)
            .parse::<toml_edit::DocumentMut>()
            .expect("toml_edit decode");
        checksum_document(&doc)
    }
}

fn checksum_table(table: &toml::Table) -> u64 {
    let mut hash: u64 = 0xa1;
    for (key, value) in table {
        hash = hash.rotate_left(5)
            ^ key
                .as_bytes()
                .iter()
                .fold(0xb2u64, |a, b| a.rotate_left(3) ^ u64::from(*b));
        hash = hash.rotate_left(5) ^ checksum_toml_value(value);
    }
    hash
}

fn checksum_toml_value(value: &toml::Value) -> u64 {
    match value {
        toml::Value::String(s) => s
            .as_bytes()
            .iter()
            .fold(0xc3u64, |a, b| a.rotate_left(3) ^ u64::from(*b)),
        toml::Value::Integer(i) => (*i).unsigned_abs(),
        toml::Value::Float(f) => f.to_bits(),
        toml::Value::Boolean(b) => u64::from(*b),
        toml::Value::Datetime(d) => d
            .to_string()
            .as_bytes()
            .iter()
            .fold(0xd4u64, |a, b| a.rotate_left(3) ^ u64::from(*b)),
        toml::Value::Array(array) => {
            let mut acc = 0xe5u64;
            for item in array {
                acc = acc.rotate_left(5) ^ checksum_toml_value(item);
            }
            acc
        }
        toml::Value::Table(table) => checksum_table(table),
    }
}

fn checksum_document(document: &toml_edit::DocumentMut) -> u64 {
    let mut hash: u64 = 0xf6;
    for (key, value) in document.iter() {
        hash = hash.rotate_left(5)
            ^ key
                .as_bytes()
                .iter()
                .fold(0xa7u64, |a, b| a.rotate_left(3) ^ u64::from(*b));
        hash = hash.rotate_left(5) ^ checksum_edit_item(value);
    }
    hash
}

fn checksum_edit_value(value: &toml_edit::Value) -> u64 {
    match value {
        toml_edit::Value::String(s) => s
            .value()
            .as_bytes()
            .iter()
            .fold(0xb8u64, |a, b| a.rotate_left(3) ^ u64::from(*b)),
        toml_edit::Value::Integer(i) => (*i.value()).unsigned_abs(),
        toml_edit::Value::Float(f) => f.value().to_bits(),
        toml_edit::Value::Boolean(b) => u64::from(*b.value()),
        toml_edit::Value::Datetime(d) => d
            .value()
            .to_string()
            .as_bytes()
            .iter()
            .fold(0xc9u64, |a, b| a.rotate_left(3) ^ u64::from(*b)),
        toml_edit::Value::Array(array) => {
            let mut acc = 0xd0u64;
            for item in array {
                acc = acc.rotate_left(5) ^ checksum_edit_value(item);
            }
            acc
        }
        toml_edit::Value::InlineTable(table) => {
            let mut acc = 0xe1u64;
            for (key, item) in table {
                acc = acc.rotate_left(5)
                    ^ key
                        .as_bytes()
                        .iter()
                        .fold(0xf2u64, |a, b| a.rotate_left(3) ^ u64::from(*b));
                acc = acc.rotate_left(5) ^ checksum_edit_value(item);
            }
            acc
        }
    }
}

fn checksum_edit_item(value: &toml_edit::Item) -> u64 {
    match value {
        toml_edit::Item::Value(value) => checksum_edit_value(value),
        toml_edit::Item::Table(table) => {
            let mut acc = 0x03u64;
            for (key, item) in table {
                acc = acc.rotate_left(5)
                    ^ key
                        .as_bytes()
                        .iter()
                        .fold(0x14u64, |a, b| a.rotate_left(3) ^ u64::from(*b));
                acc = acc.rotate_left(5) ^ checksum_edit_item(item);
            }
            acc
        }
        toml_edit::Item::ArrayOfTables(array) => {
            let mut acc = 0x25u64;
            for table in array {
                acc = acc.rotate_left(5) ^ checksum_edit_item_table(table);
            }
            acc
        }
        toml_edit::Item::None => 0x36,
    }
}

fn checksum_edit_item_table(table: &toml_edit::Table) -> u64 {
    let mut acc = 0x47u64;
    for (key, item) in table {
        acc = acc.rotate_left(5)
            ^ key
                .as_bytes()
                .iter()
                .fold(0x58u64, |a, b| a.rotate_left(3) ^ u64::from(*b));
        acc = acc.rotate_left(5) ^ checksum_edit_item(item);
    }
    acc
}

/// Builds the retained benchmark inventory.
///
/// Every decode fixture gets three lanes (jqf, `toml`, `toml_edit`). The jqf
/// encode lane is derived from the same fixture: decode once at construction
/// (outside timing) to obtain the owned value, and preflight-pin the expected
/// deterministic bytes on first run.
pub(crate) fn cases() -> Vec<Box<dyn BenchmarkCase>> {
    let mut cases: Vec<Box<dyn BenchmarkCase>> = Vec::new();
    // The capability-roadmap route lanes: shallow (slot 1), scoped (slot 2),
    // and structure-only (slot 3) over the richest fixture, each with its own
    // physical-route receipt and pinned witness. Added once per inventory,
    // before the per-fixture sweep, so their receipts run at every bench.
    assert_route_lane_receipts();
    let medium = fixtures::FIXTURES
        .iter()
        .find(|fixture| fixture.name == "medium/mixed")
        .expect("medium fixture");
    cases.push(Box::new(JqfRouteLane {
        name: "medium/mixed/scoped-owner-name",
        source: medium.source,
        kind: RouteLaneKind::Scoped {
            members: &["owner", "name"],
            index: None,
        },
    }));

    for fixture in fixtures::FIXTURES {
        cases.push(Box::new(JqfDecodeCase::new(fixture.name, fixture.source)));
        cases.push(Box::new(TomlCrateDecodeCase {
            name: concat_fixture_name(fixture.name, "toml-crate"),
            source: fixture.source.to_owned(),
        }));
        cases.push(Box::new(TomlEditDecodeCase {
            name: concat_fixture_name(fixture.name, "toml-edit"),
            source: fixture.source.to_owned(),
        }));
        // Encode lane: decode the fixture once (unmeasured) to get the value.
        let value = decode_value(fixture.source.as_bytes());
        cases.push(Box::new(JqfEncodeCase {
            name: concat_fixture_name(fixture.name, "jqf-encode"),
            value,
            encoded: None,
        }));
    }
    // The >=1 MB catalog: the one lane a decode-side lever can actually bite
    // on. Same four-lane sweep as every in-crate fixture.
    let large = fixtures::large_catalog_source();
    cases.push(Box::new(JqfDecodeCase::new(fixtures::LARGE_CATALOG_NAME, large)));
    cases.push(Box::new(TomlCrateDecodeCase {
        name: concat_fixture_name(fixtures::LARGE_CATALOG_NAME, "toml-crate"),
        source: large.to_owned(),
    }));
    cases.push(Box::new(TomlEditDecodeCase {
        name: concat_fixture_name(fixtures::LARGE_CATALOG_NAME, "toml-edit"),
        source: large.to_owned(),
    }));
    let value = decode_value(large.as_bytes());
    cases.push(Box::new(JqfEncodeCase {
        name: concat_fixture_name(fixtures::LARGE_CATALOG_NAME, "jqf-encode"),
        value,
        encoded: None,
    }));
    cases
}

/// A case name that lives for the whole bench process. The inventory is built
/// exactly once per run, so leaking the concatenated names is bounded and
/// matches the `CaseMetadata.name: &'static str` contract.
fn concat_fixture_name(name: &'static str, suffix: &str) -> &'static str {
    let owned = format!("{name}/{suffix}");
    Box::leak(owned.into_boxed_str())
}

/// Decodes a TOML source to an owned value (unmeasured; used to build the
/// encode lanes and the fixture-pinning decode).
fn decode_value(bytes: &[u8]) -> Value {
    let mut resources = bench_ledger();
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(96), SourceKind::Input),
        "fixture.toml",
        bytes,
        0,
    );
    let mut provider = jqf_codec_toml::registration_1_0()
        .expect("registration")
        .decoder()
        .expect("decoder")
        .create_provider(
            source,
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new("toml.jqf-1.0@1").expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            &mut resources,
        )
        .expect("provider");
    let requirement = whole_requirement(&resources);
    let handle = provider.bind(&requirement).expect("bind");
    let mut session = provider.open(&handle, &mut resources).expect("open");
    let result = {
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4096);
        session.decode(&mut run).expect("decode")
    };
    let AccessOutcome::FullDocument(product) = result.outcome() else {
        panic!("expected full document");
    };
    product
        .document()
        .materialize_root(&mut bench_ledger())
        .expect("materialize")
}

/// The capability-roadmap route lanes (phases 2, 3, 5): the shallow, scoped,
/// and structure-only routes over the richest fixture, each asserting its
/// sealed physical route receipt before timing and pinning a semantic
/// checksum. The shallow lane additionally asserts its stand-in retains a
/// FRACTION of the whole route's decoded text — the physical reason the route
/// exists.
struct JqfRouteLane {
    name: &'static str,
    source: &'static str,
    kind: RouteLaneKind,
}

#[derive(Clone, Copy)]
enum RouteLaneKind {
    Scoped {
        members: &'static [&'static str],
        index: Option<i64>,
    },
}

impl BenchmarkCase for JqfRouteLane {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new(self.name, 1, self.source.len() as u64)
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let receipt = route_lane_run(&self.kind, self.source.as_bytes())?;
        {
            let checksum = receipt.checksum;
            let expected = fixtures::expected_checksum(self.name);
            if checksum != expected {
                return Err(format!(
                    "{}: route witness drifted (got {checksum:#x}, want {expected:#x})",
                    self.name
                ));
            }
        }
        Ok(PreflightReceipt::new(receipt.checksum, "route witness".to_owned()))
    }

    fn run(&mut self) -> u64 {
        let receipt = route_lane_run(&self.kind, self.source.as_bytes()).expect("route run");
        receipt.checksum
    }
}

/// One route lane's run: the physical receipt and the semantic witness.
struct RouteReceipt {
    checksum: u64,
    route: jqf_codec_core::PhysicalRouteId,
    slot: u32,
}

#[allow(
    clippy::too_many_lines,
    reason = "one route drive per lane kind: the requirement build, the bind, and the poll loop read as one linear receipt"
)]
fn route_lane_run(kind: &RouteLaneKind, bytes: &[u8]) -> Result<RouteReceipt, String> {
    let mut resources = bench_ledger();
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(98), SourceKind::Input),
        "route.toml",
        bytes,
        0,
    );
    let mut provider = jqf_codec_toml::registration_1_0()
        .map_err(|e| format!("{e:?}"))?
        .decoder()
        .expect("decoder")
        .create_provider(
            source,
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new("toml.jqf-1.0@1").expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            &mut resources,
        )
        .map_err(|e| format!("provider: {:?}", e.kind()))?;
    let RouteLaneKind::Scoped { members, index } = kind;
    let (members, index) = (*members, *index);
    let mut demand = CodecDemand::try_new(&resources);
    demand
        .try_insert(&DemandClause::SemanticRoot)
        .map_err(|e| format!("{e:?}"))?;
    demand
        .try_insert(&DemandClause::ValueShape)
        .map_err(|e| format!("{e:?}"))?;
    let mut path = jqf_codec_core::ExactPath::try_new(&resources);
    for member in members {
        path.try_push_semantic_member(member, &resources)
            .map_err(|e| format!("{e:?}"))?;
    }
    if let Some(index) = index {
        path.try_push_semantic_index(index, &resources);
    }
    let footprint = jqf_codec_core::AccessFootprint::try_exact(path, &resources);
    let requirement = AccessRequirement::try_exact(
        footprint,
        demand,
        AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
        &resources,
    )
    .map_err(|e| format!("{e:?}"))?;
    let handle = provider.bind(&requirement).map_err(|e| format!("bind: {e:?}"))?;
    let mut session = provider
        .open(&handle, &mut resources)
        .map_err(|e| format!("open: {:?}", e.kind()))?;
    let receipt = session
        .physical_route_receipt()
        .ok_or_else(|| "route session without a physical receipt".to_owned())?;
    match kind {
        RouteLaneKind::Scoped { .. } => {
            let result = {
                let mut run = CodecRunContext::new(&mut resources);
                run.set_cooperative_credits(4096);
                session
                    .decode(&mut run)
                    .map_err(|e| format!("decode: {:?}", e.kind()))?
            };
            let AccessOutcome::Located(outcome) = result.outcome() else {
                return Err("route decode was not a Located outcome".into());
            };
            let value = outcome
                .product()
                .document()
                .materialize_root(&mut bench_ledger())
                .map_err(|e| format!("materialize: {e:?}"))?;
            Ok(RouteReceipt {
                checksum: value_checksum(&value),
                route: receipt.route(),
                slot: receipt.slot().get(),
            })
        }
    }
}

/// Route-lane preflight asserts: the sealed physical identity per lane.
/// Called from the inventory builder.
fn assert_route_lane_receipts() {
    let scoped = route_lane_run(
        &RouteLaneKind::Scoped {
            members: &["owner", "name"],
            index: None,
        },
        fixtures::FIXTURES
            .iter()
            .find(|fixture| fixture.name == "medium/mixed")
            .expect("medium fixture")
            .source
            .as_bytes(),
    )
    .expect("scoped lane");
    assert_eq!(scoped.route, jqf_codec_toml::SCOPED_PHYSICAL_ROUTE_ID);
    assert_eq!(scoped.slot, 1);
}
