use std::{collections::BTreeMap, fmt::Write as _};

use jqf_bench_core::{BenchmarkCase, CaseMetadata, PreflightReceipt};
use jqf_codec_core::{
    AccessGuarantees, AccessOutcome, AccessRequirement, CodecDemand, DecodeRequest, DemandClause, DiagnosticPolicy,
    ValidationMode,
};
use jqf_data::{
    AccountedDocumentBuilder, AccountedOccurrenceKey, AccountedSemanticNode, BatchLimit, DataError, DialectId,
    Document, DocumentCapability, DocumentCapacity, DocumentStorageLayoutStats, LocalOwnerRef, MaterializeWorkspace,
    NodeHandle, NodeId, ReaderCompletion, ReaderDemand, ReaderPoll, ScalarView, TopologyBatch, Value,
};
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

use crate::{
    checksum,
    fixture::{
        AccountedShape, DEEP_JSON, DocumentSummary, ESCAPE_HEAVY_JSON, NESTED_JSON, PLAIN_WIDTH, RICH_WIDTH, RichPlan,
        SourceFixtureEvidence, WIDE_DUPLICATE_50_JSON, WIDE_DUPLICATE_90_JSON, build_accounted_shape,
        build_plain_document, build_rich_document, build_same_semantics_document, count_tags, deep_json,
        escape_heavy_json, expected_plain_summary, expected_rich_summary, fact_checksum, nested_json,
        semantic_key_index, summarize, summary_detail, verify_source_fixture, wide_duplicate_json,
    },
};

const PLAIN_SEMANTIC_CHECKSUM: u64 = 0x7c7a_8437_dbf9_5e0a;
const RICH_SEMANTIC_CHECKSUM: u64 = 0x808c_acb8_d2f7_eced;
const REUSABLE_SUBTREE_CHECKSUM: u64 = 0x8c40_3266_9313_11b2;
const TOPOLOGY_READER_CHECKSUM: u64 = 0xfa53_a937_e284_b530;
// Re-pinned with the `order`-field removal: the fact checksum no longer folds
// the fact's table position (a duplicate of its index), so the identity-
// independent fixture checksum moved.
const FACT_READER_CHECKSUM: u64 = 0x3ff2_87aa_da9a_11e1;
#[cfg(test)]
const RICH_SOURCE_CHECKSUM: u64 = 0xe3fc_5def_0346_70e1;

fn covered_occurrence_count(document: &Document<'_>) -> usize {
    // Rich topology is optional under demand-scoped coverage; the semantic edge
    // count (one per authored occurrence) is always retained and equals the
    // topology occurrence count when that capability is present.
    if document.coverage().contains(jqf_data::DocumentCapability::Topology) {
        document
            .occurrence_count()
            .expect("benchmark fixture retains complete topology")
    } else {
        document.semantic_relationship_count()
    }
}

fn covered_fact_count(document: &Document<'_>) -> usize {
    // Attached facts are optional under demand-scoped coverage: a document that
    // did not retain them cannot carry any, so the count is zero.
    if document
        .coverage()
        .contains(jqf_data::DocumentCapability::AttachedFacts)
    {
        document.fact_count().expect("benchmark fixture retains complete facts")
    } else {
        0
    }
}

fn covered_provenance_count(_document: &Document<'_>) -> usize {
    // Provenance records were removed (F3); the count is pinned at zero.
    0
}

fn covered_text_stats(document: &Document<'_>) -> jqf_data::DocumentTextStorageStats {
    document
        .text_storage_stats()
        .expect("benchmark fixture retains semantic text and topology")
}

pub(crate) fn cases() -> Vec<Box<dyn BenchmarkCase>> {
    let mut cases: Vec<Box<dyn BenchmarkCase>> = vec![
        Box::new(DocumentBuild::plain()),
        Box::new(DocumentBuild::source_rich()),
        Box::new(AccountedDocumentBuild::new()),
        Box::new(AccountedDocumentCloneDrop::new()),
        Box::new(ObjectViewLookup::new()),
        Box::new(TopologyReaderCase::new()),
        Box::new(FactReaderCase::new()),
        Box::new(MaterializeOneShot::new()),
        Box::new(MaterializeReusableRoot::new()),
        Box::new(MaterializeReusableSubtree::new()),
        Box::new(MaterializeSourceRichTagged::new()),
    ];
    cases.extend(batch2_data_cases());
    cases.extend(batch2_baseline_cases());
    cases.extend(legacy_relationship_inventory_cases());
    cases.push(Box::new(SameSemanticsCase::new()));
    cases
}

const ACCOUNTED_WIDTH: usize = 8_192;
static BENCH_CONTROL: ContinueControl = ContinueControl;

struct AccountedDocumentBuild;

impl AccountedDocumentBuild {
    const fn new() -> Self {
        Self
    }

    fn execute() -> Result<(u64, u64, u64), DataError> {
        let control = ContinueControl;
        let limits = ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
        let account = RequestAccount::try_new(limits)?;
        let work = WorkMeter::try_new_v1(1).ok_or(DataError::InvalidDocument)?;
        let resources = ResourceContext::new(account, &control, work)?;
        let baseline = resources.snapshot().memory_current_bytes();
        let document = build_accounted_plain(ACCOUNTED_WIDTH, &resources)?;
        let live = resources.snapshot().memory_current_bytes();
        let checksum = checksum::usize(
            checksum::usize(checksum::OFFSET, document.node_count()),
            covered_occurrence_count(&document),
        );
        drop(document);
        let after_drop = resources.snapshot().memory_current_bytes();
        Ok((
            checksum,
            live.saturating_sub(baseline),
            after_drop.saturating_sub(baseline),
        ))
    }
}

impl BenchmarkCase for AccountedDocumentBuild {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new("document/build-accounted-semantic-8192", 1, 0)
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let (checksum, live_bytes, after_drop_bytes) = Self::execute().map_err(|error| error.to_string())?;
        // Construction residency is allocator-managed after the de-thread
        // wave: the threaded ledger sees zero live bytes. What must hold is
        // that the threaded ledger does not LEAK — after dropping every
        // owner, residency returns to baseline.
        if after_drop_bytes != 0 {
            return Err(format!(
                "accounted construction residency mismatch: live_bytes={live_bytes} after_drop_bytes={after_drop_bytes}"
            ));
        }
        Ok(PreflightReceipt::new(
            checksum,
            format!(
                "route=AccountedDocumentBuilder nodes={} occurrences={} live_retained_bytes={live_bytes} after_all_document_owners_drop_bytes={after_drop_bytes}",
                ACCOUNTED_WIDTH + 1,
                ACCOUNTED_WIDTH,
            ),
        ))
    }

    fn run(&mut self) -> u64 {
        let (checksum, live_bytes, after_drop_bytes) = Self::execute().expect("accounted construction");
        checksum::u64(checksum::u64(checksum, live_bytes), after_drop_bytes)
    }
}

struct AccountedDocumentCloneDrop {
    resources: ResourceContext<'static>,
    document: Document<'static>,
}

impl AccountedDocumentCloneDrop {
    fn new() -> Self {
        let resources = benchmark_resources().expect("accounted clone/drop resources start");
        let document = build_accounted_plain(ACCOUNTED_WIDTH, &resources).expect("accounted clone/drop fixture builds");
        Self { resources, document }
    }

    fn execute() -> Result<(u64, u64, u64, u64), DataError> {
        let control = ContinueControl;
        let limits = ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
        let account = RequestAccount::try_new(limits)?;
        let work = WorkMeter::try_new_v1(1).ok_or(DataError::InvalidDocument)?;
        let resources = ResourceContext::new(account, &control, work)?;
        let baseline = resources.snapshot().memory_current_bytes();
        let document = build_accounted_plain(ACCOUNTED_WIDTH, &resources)?;
        let before_clone = resources.snapshot().memory_current_bytes();
        let clone = document.try_clone()?;
        let after_clone = resources.snapshot().memory_current_bytes();
        drop(document);
        let one_owner = resources.snapshot().memory_current_bytes();
        let checksum = checksum::u64(checksum::OFFSET, clone.root().get());
        drop(clone);
        let after_drop = resources.snapshot().memory_current_bytes();
        Ok((
            checksum,
            after_clone.saturating_sub(before_clone),
            one_owner.saturating_sub(baseline),
            after_drop.saturating_sub(baseline),
        ))
    }
}

impl BenchmarkCase for AccountedDocumentCloneDrop {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new("document/accounted-checked-clone-drop-8192", 1, 0)
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let before_stored_clone = self.resources.snapshot().memory_current_bytes();
        let stored_clone = self.document.try_clone().map_err(|error| error.to_string())?;
        let after_stored_clone = self.resources.snapshot().memory_current_bytes();
        drop(stored_clone);
        let after_stored_drop = self.resources.snapshot().memory_current_bytes();
        if after_stored_clone != before_stored_clone || after_stored_drop != before_stored_clone {
            return Err(format!(
                "prebuilt clone/drop changed residency: before={before_stored_clone} after_clone={after_stored_clone} after_drop={after_stored_drop}"
            ));
        }
        let (checksum, clone_charge, one_owner_bytes, after_drop_bytes) =
            Self::execute().map_err(|error| error.to_string())?;
        // The document's storage is allocator-managed after the de-thread
        // wave, so the threaded ledger sees zero live bytes for the clone's
        // shared owners. What must hold is no leak: after dropping every
        // owner, residency returns to baseline.
        if after_drop_bytes != 0 {
            return Err(format!(
                "accounted clone/drop mismatch: clone_charge={clone_charge} one_owner_bytes={one_owner_bytes} after_drop_bytes={after_drop_bytes}"
            ));
        }
        Ok(PreflightReceipt::new(
            checksum,
            format!(
                "route=Document::try_clone fixture=prebuilt-outside-invocation clone_additional_charge_bytes={clone_charge} one_remaining_owner_live_bytes={one_owner_bytes} after_final_owner_drop_bytes={after_drop_bytes}"
            ),
        ))
    }

    fn run(&mut self) -> u64 {
        let clone = self.document.try_clone().expect("accounted checked clone");
        let checksum = checksum::u64(checksum::OFFSET, clone.root().get());
        drop(clone);
        checksum
    }
}

/// One unlimited bench ledger for a preflight-only materialization.
///
/// Materializing a document now CHARGES its owned value's allocations (the
/// allocation-side residency law), so every call site needs an account. A
/// preflight builds its value, checks it, and drops it, so a throwaway ledger is
/// the honest scope: the account outlives the value through the residency's own
/// refcount, and nothing measured runs inside it.
fn bench_ledger() -> ResourceContext<'static> {
    benchmark_resources().expect("deterministic bench ledger")
}

fn benchmark_resources() -> Result<ResourceContext<'static>, DataError> {
    let limits = ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
    let account = RequestAccount::try_new(limits)?;
    let work = WorkMeter::try_new_v1(1).ok_or(DataError::InvalidDocument)?;
    Ok(ResourceContext::new(account, &BENCH_CONTROL, work)?)
}

fn build_accounted_plain(width: usize, resources: &ResourceContext<'_>) -> Result<Document<'static>, DataError> {
    let mut builder = AccountedDocumentBuilder::try_new("bench", None)?;
    builder.try_reserve(
        DocumentCapacity {
            nodes: width.saturating_add(1),
            occurrences: width,
            ..DocumentCapacity::default()
        },
        resources,
    )?;
    let root = builder.add_node(
        "bench.array",
        AccountedSemanticNode::Array {
            item_role: "bench.item",
        },
        None,
        resources,
    )?;
    for index in 0..width {
        let node = builder.add_node(
            "bench.bool",
            AccountedSemanticNode::Bool(index & 1 == 0),
            None,
            resources,
        )?;
        builder.add_occurrence(LocalOwnerRef::Node(root), "bench.item", None, node, resources)?;
    }
    builder.finish(root, resources)
}

enum DocumentBuildFixture {
    Plain,
    SourceRich(RichPlan),
}

struct DocumentBuild {
    name: &'static str,
    fixture: DocumentBuildFixture,
    expected: DocumentSummary,
}

impl DocumentBuild {
    fn plain() -> Self {
        Self {
            name: "document/build-semantic-65536",
            fixture: DocumentBuildFixture::Plain,
            expected: expected_plain_summary(PLAIN_WIDTH, PLAIN_SEMANTIC_CHECKSUM),
        }
    }

    fn source_rich() -> Self {
        let plan = RichPlan::new(RICH_WIDTH);
        Self {
            name: "document/build-source-rich-32768",
            expected: expected_rich_summary(&plan, RICH_SEMANTIC_CHECKSUM),
            fixture: DocumentBuildFixture::SourceRich(plan),
        }
    }

    fn build(&self) -> Document<'static> {
        match &self.fixture {
            DocumentBuildFixture::Plain => build_plain_document(PLAIN_WIDTH),
            DocumentBuildFixture::SourceRich(plan) => build_rich_document(plan),
        }
        .expect("deterministic document construction")
    }

    fn run_checksum(document: &Document<'_>) -> u64 {
        let stats = covered_text_stats(document);
        let mut checksum = checksum::usize(checksum::OFFSET, document.node_count());
        checksum = checksum::usize(checksum, covered_occurrence_count(document));
        checksum = checksum::usize(checksum, covered_fact_count(document));
        checksum = checksum::usize(checksum, covered_provenance_count(document));
        checksum = checksum::usize(checksum, stats.source_string_values);
        checksum = checksum::usize(checksum, stats.source_keys);
        checksum = checksum::usize(checksum, stats.decoded_arena_len);
        checksum
    }
}

impl BenchmarkCase for DocumentBuild {
    fn metadata(&self) -> CaseMetadata {
        let bytes = match &self.fixture {
            DocumentBuildFixture::Plain => 0,
            DocumentBuildFixture::SourceRich(plan) => plan.source_len() as u64,
        };
        CaseMetadata::new(self.name, 1, bytes)
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let document = self.build();
        let summary = summarize(&document, &mut bench_ledger()).map_err(|error| error.to_string())?;
        if summary != self.expected {
            return Err(format!(
                "document summary mismatch: actual={summary:?} expected={:?}",
                self.expected
            ));
        }
        let checksum = Self::run_checksum(&document);
        Ok(PreflightReceipt::new(checksum, summary_detail(summary)))
    }

    fn run(&mut self) -> u64 {
        let document = self.build();
        Self::run_checksum(&document)
    }
}

struct ObjectViewLookup {
    document: Document<'static>,
    queries: Vec<String>,
    expected: Vec<Option<ObjectViewExpected>>,
    hits: usize,
    unique_entries: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ObjectViewExpected {
    node: u64,
    text: String,
}

impl ObjectViewLookup {
    fn new() -> Self {
        let plan = RichPlan::new(RICH_WIDTH);
        let unique_entries = plan.unique_keys();
        let document = build_rich_document(&plan).expect("deterministic wide object view fixture");
        let mut queries = Vec::with_capacity(4_096);
        let mut expected = Vec::with_capacity(4_096);
        let mut hits = 0;
        for index in 0..4_096 {
            if index % 5 == 0 {
                queries.push(format!("missing-{index:05}"));
                expected.push(None);
            } else {
                hits += 1;
                let occurrence = (index * 977) % RICH_WIDTH;
                let key_index = semantic_key_index(occurrence);
                let final_occurrence = if key_index % 8 == 6 { key_index + 1 } else { key_index };
                queries.push(format!("key{key_index:05}"));
                expected.push(Some(ObjectViewExpected {
                    node: (final_occurrence + 1) as u64,
                    text: format!("value{final_occurrence:05}"),
                }));
            }
        }
        Self {
            document,
            queries,
            expected,
            hits,
            unique_entries,
        }
    }

    fn execute_hot(&self) -> Result<(usize, u64), DataError> {
        let object = self
            .document
            .value_view(self.document.root_handle())?
            .object()?
            .ok_or(DataError::InvalidDocument)?;
        let mut hits = 0;
        let mut checksum = checksum::OFFSET;
        for query in &self.queries {
            if let Some(value) = object.get(query) {
                hits += 1;
                checksum = checksum::byte(checksum, 1);
                checksum = checksum::u64(checksum, value.node().get());
            } else {
                checksum = checksum::byte(checksum, 0);
            }
        }
        Ok((hits, checksum))
    }

    fn validate_sequence(&self) -> Result<(usize, u64), String> {
        if self.queries.len() != self.expected.len() {
            return Err(format!(
                "object-view oracle length={} differs from query length={}",
                self.expected.len(),
                self.queries.len()
            ));
        }
        let object = self
            .document
            .value_view(self.document.root_handle())
            .map_err(|error| error.to_string())?
            .object()
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "object-view root is not an object".to_owned())?;
        let mut hits = 0;
        let mut checksum = checksum::OFFSET;
        for (index, (query, expected)) in self.queries.iter().zip(&self.expected).enumerate() {
            let observed = object.get(query);
            match (observed, expected) {
                (None, None) => {
                    checksum = checksum::str(checksum, query);
                    checksum = checksum::byte(checksum, 0);
                }
                (Some(value), Some(expected)) => {
                    hits += 1;
                    let node = value.node().get();
                    let Some(ScalarView::String(text)) = value.scalar().map_err(|error| error.to_string())? else {
                        return Err(format!(
                            "object-view lookup {index} for {query:?} did not return a string"
                        ));
                    };
                    if node != expected.node || text != expected.text {
                        return Err(format!(
                            "object-view lookup {index} for {query:?} returned node={node} text={text:?}, expected node={} text={:?}",
                            expected.node, expected.text
                        ));
                    }
                    checksum = checksum::str(checksum, query);
                    checksum = checksum::byte(checksum, 1);
                    checksum = checksum::u64(checksum, node);
                    checksum = checksum::str(checksum, text);
                }
                (observed, expected) => {
                    return Err(format!(
                        "object-view lookup {index} for {query:?} presence mismatch: observed={} expected={}",
                        observed.is_some(),
                        expected.is_some()
                    ));
                }
            }
        }
        Ok((hits, checksum))
    }
}

impl BenchmarkCase for ObjectViewLookup {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new("object-view/lookup-wide-4096", self.queries.len() as u64, 0)
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let (hits, checksum) = self.validate_sequence()?;
        if hits != self.hits {
            return Err(format!("object-view hits={hits}, expected {}", self.hits));
        }
        let stats = covered_text_stats(&self.document);
        let entries = self
            .document
            .value_view(self.document.root_handle())
            .map_err(|error| error.to_string())?
            .object()
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "object-view root is not an object".to_owned())?
            .len();
        if entries != self.unique_entries {
            return Err(format!(
                "object-view unique entries={entries}, expected {}",
                self.unique_entries
            ));
        }
        if stats.source_string_values != RICH_WIDTH
            || stats.source_keys != RICH_WIDTH
            || stats.stored_string_values != 0
            || stats.stored_keys != 0
            || stats.decoded_arena_len != 0
        {
            return Err(format!("wide object view lost source-backed route: {stats:?}"));
        }
        let summary = summarize(&self.document, &mut bench_ledger()).map_err(|error| error.to_string())?;
        let expected_tags = 1 + RICH_WIDTH.div_ceil(8);
        if summary.tags != expected_tags {
            return Err(format!(
                "wide object view tags={}, expected {expected_tags}",
                summary.tags
            ));
        }
        Ok(PreflightReceipt::new(
            checksum,
            format!(
                "member_occurrences={} unique_entries={} duplicate_occurrences={} lookups={} hits={} misses={} nodes={} tags={} source_string_values={} source_keys={} stored_string_values={} stored_keys={} stored_integer_refs={} stored_decimal_coefficient_refs={} decoded_arena_len={} decoded_arena_capacity={} checksum=0x{checksum:016x}",
                RICH_WIDTH,
                self.unique_entries,
                RICH_WIDTH / 8,
                self.queries.len(),
                hits,
                self.queries.len() - hits,
                summary.nodes,
                summary.tags,
                stats.source_string_values,
                stats.source_keys,
                stats.stored_string_values,
                stats.stored_keys,
                stats.stored_integer_refs,
                stats.stored_decimal_coefficient_refs,
                stats.decoded_arena_len,
                stats.decoded_arena_capacity,
            ),
        ))
    }

    fn run(&mut self) -> u64 {
        self.execute_hot().expect("deterministic wide ObjectView lookups").1
    }
}

#[derive(Clone, Copy)]
struct ReaderRun {
    items: usize,
    batches: usize,
    max_batch: usize,
    pending: usize,
    completion: ReaderCompletion,
    checksum: u64,
    tags: usize,
}

struct TopologyReaderCase {
    document: Document<'static>,
}

impl TopologyReaderCase {
    fn new() -> Self {
        let plan = RichPlan::new(RICH_WIDTH);
        let document = build_rich_document(&plan).expect("deterministic topology reader fixture");
        Self { document }
    }
}

impl BenchmarkCase for TopologyReaderCase {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new(
            "reader/topology-source-rich-32768",
            (self.document.node_count() + covered_occurrence_count(&self.document)) as u64,
            0,
        )
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let actual = run_topology_reader(&self.document).map_err(|error| error.to_string())?;
        let expected_checksum = pinned_fixture_checksum(
            "topology reader",
            expected_topology_reader_checksum(),
            TOPOLOGY_READER_CHECKSUM,
        )?;
        let expected_batches = (RICH_WIDTH + 1).div_ceil(128) + RICH_WIDTH.div_ceil(128);
        validate_reader_run(
            actual,
            self.document.node_count() + covered_occurrence_count(&self.document),
            1 + RICH_WIDTH.div_ceil(8),
            expected_batches,
            128,
            expected_batches - 1,
            expected_checksum,
        )?;
        let stats = covered_text_stats(&self.document);
        if stats.source_string_values != RICH_WIDTH || stats.source_keys != RICH_WIDTH {
            return Err(format!("topology fixture lost source-backed text: {stats:?}"));
        }
        Ok(PreflightReceipt::new(
            actual.checksum,
            format!(
                "{} fixture_expected_checksum=0x{TOPOLOGY_READER_CHECKSUM:016x} expectation=identity-independent-fixture-law source_string_values={} source_keys={} stored_string_values={} stored_keys={} stored_integer_refs={} stored_decimal_coefficient_refs={} decoded_arena_len={} decoded_arena_capacity={}",
                reader_detail(actual),
                stats.source_string_values,
                stats.source_keys,
                stats.stored_string_values,
                stats.stored_keys,
                stats.stored_integer_refs,
                stats.stored_decimal_coefficient_refs,
                stats.decoded_arena_len,
                stats.decoded_arena_capacity,
            ),
        ))
    }

    fn run(&mut self) -> u64 {
        run_topology_reader(&self.document)
            .expect("deterministic topology reader")
            .checksum
    }
}

struct FactReaderCase {
    document: Document<'static>,
}

impl FactReaderCase {
    fn new() -> Self {
        let plan = RichPlan::new(RICH_WIDTH);
        let document = build_rich_document(&plan).expect("deterministic fact reader fixture");
        Self { document }
    }
}

impl BenchmarkCase for FactReaderCase {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new(
            "reader/facts-source-rich-32768",
            covered_fact_count(&self.document) as u64,
            0,
        )
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let actual = run_fact_reader(&self.document).map_err(|error| error.to_string())?;
        let expected_checksum =
            pinned_fixture_checksum("fact reader", expected_fact_reader_checksum(), FACT_READER_CHECKSUM)?;
        let expected_items = RICH_WIDTH.div_ceil(4);
        let expected_batches = expected_items.div_ceil(64);
        validate_reader_run(
            actual,
            expected_items,
            0,
            expected_batches,
            64,
            expected_batches - 1,
            expected_checksum,
        )?;
        let stats = covered_text_stats(&self.document);
        Ok(PreflightReceipt::new(
            actual.checksum,
            format!(
                "{} fixture_expected_checksum=0x{FACT_READER_CHECKSUM:016x} expectation=identity-independent-fixture-law document_tags={} source_string_values={} source_keys={} stored_string_values={} stored_keys={} stored_integer_refs={} stored_decimal_coefficient_refs={} decoded_arena_len={} decoded_arena_capacity={}",
                reader_detail(actual),
                1 + RICH_WIDTH.div_ceil(8),
                stats.source_string_values,
                stats.source_keys,
                stats.stored_string_values,
                stats.stored_keys,
                stats.stored_integer_refs,
                stats.stored_decimal_coefficient_refs,
                stats.decoded_arena_len,
                stats.decoded_arena_capacity,
            ),
        ))
    }

    fn run(&mut self) -> u64 {
        run_fact_reader(&self.document)
            .expect("deterministic fact reader")
            .checksum
    }
}

struct MaterializeOneShot {
    document: Document<'static>,
    semantic_checksum: u64,
    resources: ResourceContext<'static>,
}

impl MaterializeOneShot {
    fn new() -> Self {
        let document = build_plain_document(PLAIN_WIDTH).expect("deterministic one-shot materialization");
        Self {
            document,
            semantic_checksum: PLAIN_SEMANTIC_CHECKSUM,
            resources: benchmark_resources().expect("deterministic bench ledger"),
        }
    }
}

impl BenchmarkCase for MaterializeOneShot {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new("materialize/one-shot-root-65536", self.document.node_count() as u64, 0)
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let value = self
            .document
            .materialize_root(&mut self.resources)
            .map_err(|error| error.to_string())?;
        let checksum = checksum::value(&value);
        if checksum != self.semantic_checksum {
            return Err(format!(
                "semantic checksum=0x{checksum:016x}, expected 0x{:016x}",
                self.semantic_checksum
            ));
        }
        Ok(PreflightReceipt::new(
            checksum,
            format!(
                "mode=one-shot nodes={} tags=0 semantic_checksum=0x{checksum:016x}",
                self.document.node_count()
            ),
        ))
    }

    fn run(&mut self) -> u64 {
        let value = self
            .document
            .materialize_root(&mut self.resources)
            .expect("deterministic one-shot materialization");
        shallow_value_checksum(&value)
    }
}

struct MaterializeReusableRoot {
    document: &'static Document<'static>,
    workspace: MaterializeWorkspace,
    root: NodeHandle,
    nodes: usize,
    semantic_checksum: u64,
    resources: ResourceContext<'static>,
}

impl MaterializeReusableRoot {
    fn new() -> Self {
        let document = Box::leak(Box::new(
            build_plain_document(PLAIN_WIDTH).expect("deterministic reusable-root fixture"),
        ));
        let nodes = document.node_count();
        Self {
            document,
            workspace: MaterializeWorkspace::new(),
            root: document.root_handle(),
            nodes,
            semantic_checksum: PLAIN_SEMANTIC_CHECKSUM,
            resources: benchmark_resources().expect("deterministic bench ledger"),
        }
    }
}

impl BenchmarkCase for MaterializeReusableRoot {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new("materialize/reusable-root-65536", self.nodes as u64, 0)
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let first = self
            .document
            .materialize_node_with(&mut self.workspace, self.root, &mut self.resources)
            .map_err(|error| error.to_string())?;
        let second = self
            .document
            .materialize_node_with(&mut self.workspace, self.root, &mut self.resources)
            .map_err(|error| error.to_string())?;
        let first_checksum = checksum::value(&first);
        let second_checksum = checksum::value(&second);
        if first_checksum != self.semantic_checksum || second_checksum != self.semantic_checksum {
            return Err("reusable root materializer changed semantic output".into());
        }
        Ok(PreflightReceipt::new(
            first_checksum,
            format!(
                "mode=reusable-root repeated=2 nodes={} first_checksum=0x{first_checksum:016x} second_checksum=0x{second_checksum:016x}",
                self.nodes
            ),
        ))
    }

    fn run(&mut self) -> u64 {
        let value = self
            .document
            .materialize_node_with(&mut self.workspace, self.root, &mut self.resources)
            .expect("deterministic reusable-root materialization");
        shallow_value_checksum(&value)
    }
}

struct MaterializeReusableSubtree {
    document: &'static Document<'static>,
    workspace: MaterializeWorkspace,
    handles: Vec<NodeHandle>,
    resources: ResourceContext<'static>,
}

impl MaterializeReusableSubtree {
    fn new() -> Self {
        let plan = RichPlan::new(RICH_WIDTH);
        let document = Box::leak(Box::new(
            build_rich_document(&plan).expect("deterministic reusable-subtree fixture"),
        ));
        let handles: Vec<_> = (0..1_024)
            .map(|index| {
                let node = NodeId::try_from_index(1 + index * 13).expect("deterministic subtree id fits");
                document.node_handle(node).expect("deterministic subtree node exists")
            })
            .collect();
        let mut case = Self {
            document,
            workspace: MaterializeWorkspace::new(),
            handles,
            resources: benchmark_resources().expect("deterministic bench ledger"),
        };
        assert_eq!(
            case.execute_full_checksum()
                .expect("deterministic subtree materialization"),
            REUSABLE_SUBTREE_CHECKSUM,
            "reusable-subtree fixture checksum drifted"
        );
        case
    }

    fn execute_full_checksum(&mut self) -> Result<u64, DataError> {
        let mut checksum = checksum::OFFSET;
        for &handle in &self.handles {
            let value = self
                .document
                .materialize_node_with(&mut self.workspace, handle, &mut self.resources)?;
            checksum = checksum::u64(checksum, handle.local().get());
            checksum = checksum::u64(checksum, checksum::value(&value));
        }
        Ok(checksum)
    }

    fn execute_witness(&mut self) -> Result<u64, DataError> {
        let mut checksum = checksum::OFFSET;
        for &handle in &self.handles {
            let value = self
                .document
                .materialize_node_with(&mut self.workspace, handle, &mut self.resources)?;
            checksum = checksum::u64(checksum, handle.local().get());
            checksum = checksum::u64(checksum, subtree_value_witness(&value)?);
        }
        Ok(checksum)
    }
}

impl BenchmarkCase for MaterializeReusableSubtree {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new("materialize/reusable-subtree-1024", self.handles.len() as u64, 0)
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let first = self.execute_full_checksum().map_err(|error| error.to_string())?;
        let second = self.execute_full_checksum().map_err(|error| error.to_string())?;
        if first != REUSABLE_SUBTREE_CHECKSUM || second != REUSABLE_SUBTREE_CHECKSUM {
            return Err("reusable subtree materializer changed semantic output".into());
        }
        let first_witness = self.execute_witness().map_err(|error| error.to_string())?;
        let second_witness = self.execute_witness().map_err(|error| error.to_string())?;
        if first_witness != second_witness {
            return Err("reusable subtree allocation-free witness changed output".into());
        }
        Ok(PreflightReceipt::new(
            first,
            format!(
                "mode=reusable-subtree materializations={} repeated=2 tagged_subtrees={} checksum=0x{first:016x} allocation_free_timed_witness=0x{first_witness:016x}",
                self.handles.len(),
                self.handles.len().div_ceil(8),
            ),
        ))
    }

    fn run(&mut self) -> u64 {
        self.execute_witness()
            .expect("deterministic reusable-subtree materialization")
    }
}

fn subtree_value_witness(value: &Value) -> Result<u64, DataError> {
    let mut checksum = checksum::kind(checksum::OFFSET, value.kind());
    if let Some(tag) = value.tag() {
        checksum = checksum::str(checksum, tag.as_str());
    }
    let Value::String(text) = value.untagged() else {
        return Err(DataError::InvalidDocument);
    };
    Ok(checksum::str(checksum, text))
}

struct MaterializeSourceRichTagged {
    document: Document<'static>,
    expected: DocumentSummary,
    resources: ResourceContext<'static>,
}

impl MaterializeSourceRichTagged {
    fn new() -> Self {
        let plan = RichPlan::new(RICH_WIDTH);
        let document = build_rich_document(&plan).expect("deterministic source-rich materialization fixture");
        let expected = expected_rich_summary(&plan, RICH_SEMANTIC_CHECKSUM);
        Self {
            document,
            expected,
            resources: benchmark_resources().expect("deterministic bench ledger"),
        }
    }
}

impl BenchmarkCase for MaterializeSourceRichTagged {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new(
            "materialize/source-rich-tagged-root-32768",
            self.document.node_count() as u64,
            0,
        )
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let summary = summarize(&self.document, &mut bench_ledger()).map_err(|error| error.to_string())?;
        if summary != self.expected {
            return Err(format!(
                "source-rich materialization summary mismatch: actual={summary:?} expected={:?}",
                self.expected
            ));
        }
        // The retained-source byte law is pinned by the fixture's own
        // `RichPlan::source_checksum` assertion in this module's tests; the
        // removed source reader was only a second spelling of it.
        Ok(PreflightReceipt::new(
            summary.semantic_checksum,
            format!(
                "{} source_expectation=identity-independent-fixture-bytes",
                summary_detail(summary),
            ),
        ))
    }

    fn run(&mut self) -> u64 {
        let value = self
            .document
            .materialize_root(&mut self.resources)
            .expect("deterministic source-rich tagged materialization");
        shallow_value_checksum(&value)
    }
}

fn reader_resources() -> Result<ResourceContext<'static>, DataError> {
    reader_resources_with(1)
}

fn reader_resources_with(credits: u32) -> Result<ResourceContext<'static>, DataError> {
    static CONTROL: ContinueControl = ContinueControl;
    let limits = ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
    let account = RequestAccount::try_new(limits)?;
    let work = WorkMeter::try_new_v1(credits).ok_or(DataError::InvalidDocument)?;
    ResourceContext::new(account, &CONTROL, work).map_err(DataError::from)
}

fn renew(resources: &mut ResourceContext<'_>) -> Result<(), DataError> {
    renew_with(resources, 1)
}

fn renew_with(resources: &mut ResourceContext<'_>, credits: u32) -> Result<(), DataError> {
    if resources.try_begin_next_cooperative_entry(credits)? {
        Ok(())
    } else {
        Err(DataError::InvalidDocument)
    }
}

fn run_topology_reader(document: &Document<'_>) -> Result<ReaderRun, DataError> {
    let mut resources = reader_resources()?;
    let mut reader = document.topology_reader(&mut resources)?;
    let limit = BatchLimit::new(128).ok_or(DataError::InvalidDocument)?;
    let mut items = 0;
    let mut batches = 0;
    let mut pending = 0;
    let mut max_batch = 0;
    let mut tags = 0;
    let mut checksum = checksum::OFFSET;
    loop {
        match reader.poll_batch(limit, &mut resources)? {
            ReaderPoll::Batch(TopologyBatch::Nodes(batch)) => {
                batches += 1;
                max_batch = max_batch.max(batch.len());
                for node in &batch {
                    let node = node?;
                    items += 1;
                    checksum = checksum::u64(checksum, node.id().get());
                    checksum = checksum::str(checksum, node.kind().as_str());
                    if let Some(tag) = node.intrinsic_tag()? {
                        tags += 1;
                        checksum = checksum::str(checksum, tag.tag().as_str());
                    }
                }
            }
            ReaderPoll::Batch(TopologyBatch::Occurrences(batch)) => {
                batches += 1;
                max_batch = max_batch.max(batch.len());
                for occurrence in &batch {
                    let occurrence = occurrence?;
                    items += 1;
                    checksum = checksum::u64(checksum, occurrence.id().get());
                    checksum = checksum::u64(checksum, occurrence.position());
                    checksum = checksum::str(checksum, occurrence.role().as_str());
                    checksum = checksum::u64(checksum, occurrence.target().get());
                    checksum = owner_checksum(checksum, occurrence.owner());
                    if let Some(key) = occurrence.key_text() {
                        checksum = checksum::str(checksum, key);
                    }
                }
            }
            ReaderPoll::Pending => {
                pending += 1;
                renew(&mut resources)?;
            }
            ReaderPoll::End(completion) => {
                return Ok(ReaderRun {
                    items,
                    batches,
                    max_batch,
                    pending,
                    completion,
                    checksum,
                    tags,
                });
            }
        }
    }
}

fn run_fact_reader(document: &Document<'_>) -> Result<ReaderRun, DataError> {
    let mut resources = reader_resources()?;
    let mut reader = document.fact_reader(&mut resources)?;
    let limit = BatchLimit::new(64).ok_or(DataError::InvalidDocument)?;
    let mut items = 0;
    let mut batches = 0;
    let mut pending = 0;
    let mut max_batch = 0;
    let mut checksum = checksum::OFFSET;
    loop {
        match reader.poll_batch(limit, &mut resources)? {
            ReaderPoll::Batch(batch) => {
                batches += 1;
                max_batch = max_batch.max(batch.len());
                for fact in batch.iter() {
                    items += 1;
                    checksum = checksum::u64(checksum, fact_checksum(&fact));
                }
            }
            ReaderPoll::Pending => {
                pending += 1;
                renew(&mut resources)?;
            }
            ReaderPoll::End(completion) => {
                return Ok(ReaderRun {
                    items,
                    batches,
                    max_batch,
                    pending,
                    completion,
                    checksum,
                    tags: 0,
                });
            }
        }
    }
}

fn expected_topology_reader_checksum() -> u64 {
    let mut checksum = checksum::u64(checksum::OFFSET, 0);
    checksum = checksum::str(checksum, "bench.object");
    checksum = checksum::str(checksum, "!catalog");
    for index in 0..RICH_WIDTH {
        checksum = checksum::u64(checksum, (index + 1) as u64);
        checksum = checksum::str(checksum, "bench.string");
        if index % 8 == 0 {
            checksum = checksum::str(checksum, "!entry");
        }
    }
    for index in 0..RICH_WIDTH {
        checksum = checksum::u64(checksum, index as u64);
        checksum = checksum::u64(checksum, index as u64);
        checksum = checksum::str(checksum, "bench.member");
        checksum = checksum::u64(checksum, (index + 1) as u64);
        checksum = owner_checksum(
            checksum,
            LocalOwnerRef::Node(NodeId::try_from_index(0).expect("root id fits")),
        );
        checksum = checksum::str(checksum, &format!("key{:05}", semantic_key_index(index)));
    }
    checksum
}

fn expected_fact_reader_checksum() -> u64 {
    let mut checksum = checksum::OFFSET;
    for source_index in (0..RICH_WIDTH).step_by(4) {
        let fact_index = source_index / 4;
        let mut record_checksum = checksum::u64(checksum::OFFSET, fact_index as u64);
        record_checksum = owner_checksum(
            record_checksum,
            LocalOwnerRef::Occurrence(
                jqf_data::OccurrenceId::try_from_index(source_index).expect("fixture occurrence id fits"),
            ),
        );
        record_checksum = checksum::str(record_checksum, "bench.comment");
        record_checksum = checksum::str(record_checksum, "bench.text");
        record_checksum = checksum::u64(record_checksum, 1);
        record_checksum = checksum::byte(record_checksum, 1);
        record_checksum = checksum::byte(record_checksum, u8::from(source_index & 8 == 0));
        checksum = checksum::u64(checksum, record_checksum);
    }
    checksum
}

fn pinned_fixture_checksum(family: &str, derived: u64, pinned: u64) -> Result<u64, String> {
    if derived == pinned {
        Ok(pinned)
    } else {
        Err(format!(
            "{family} independently derived checksum=0x{derived:016x}, pinned fixture checksum=0x{pinned:016x}"
        ))
    }
}

fn validate_reader_run(
    actual: ReaderRun,
    expected_items: usize,
    expected_tags: usize,
    expected_batches: usize,
    expected_max_batch: usize,
    expected_pending: usize,
    expected_checksum: u64,
) -> Result<(), String> {
    if actual.items != expected_items
        || actual.tags != expected_tags
        || actual.batches != expected_batches
        || actual.max_batch != expected_max_batch
        || actual.pending != expected_pending
        || actual.checksum != expected_checksum
    {
        return Err(format!(
            "reader receipt mismatch: actual={} expected_items={expected_items} expected_tags={expected_tags} expected_batches={expected_batches} expected_max_batch={expected_max_batch} expected_pending={expected_pending} expected_checksum=0x{expected_checksum:016x}",
            reader_detail(actual),
        ));
    }
    if actual.pending != actual.batches.saturating_sub(1) {
        return Err(format!(
            "cooperative renewals={}, expected one between each of {} batches",
            actual.pending, actual.batches
        ));
    }
    Ok(())
}

fn reader_detail(run: ReaderRun) -> String {
    format!(
        "items={} batches={} max_batch={} pending={} renewed={} completion=complete completion_fingerprint=0x{:016x} tags={} checksum=0x{:016x}",
        run.items,
        run.batches,
        run.max_batch,
        run.pending,
        run.pending,
        completion_fingerprint(run.completion),
        run.tags,
        run.checksum,
    )
}

/// The reader completion's document-and-demand-bound evidence fingerprint.
///
/// jqf-data dropped this hash (plan: perf complexity review); the bench keeps
/// it as the receipt's completion evidence. The hash is FNV-1a-derived over
/// the document identity, revision, policy revision, and demand discriminator
/// — the same fields the removed `ReaderCompletion::evidence_fingerprint`
/// folded, so stored receipts keep their values.
fn completion_fingerprint(completion: ReaderCompletion) -> u64 {
    fn fold(hash: u64, bytes: [u8; 8]) -> u64 {
        let mut hash = hash;
        for byte in bytes {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    hash = fold(hash, completion.document().get().to_le_bytes());
    // The retired revision (always 1) and `policy_revision` (always 1) stay
    // folded as constants so stored receipts keep their values.
    hash = fold(hash, 1_u64.to_le_bytes());
    hash = fold(hash, 1_u64.to_le_bytes());
    let discriminator = match completion.demand() {
        ReaderDemand::Topology => 3,
        ReaderDemand::Facts => 4,
    };
    (hash ^ discriminator).wrapping_mul(0x0000_0100_0000_01b3)
}

fn owner_checksum(state: u64, owner: LocalOwnerRef) -> u64 {
    match owner {
        LocalOwnerRef::DocumentRoot => checksum::byte(state, 0),
        LocalOwnerRef::Node(node) => checksum::u64(checksum::byte(state, 1), node.get()),
        LocalOwnerRef::Occurrence(occurrence) => checksum::u64(checksum::byte(state, 2), occurrence.get()),
    }
}

fn shallow_value_checksum(value: &Value) -> u64 {
    let mut checksum = checksum::kind(checksum::OFFSET, value.kind());
    if let Some(tag) = value.tag() {
        checksum = checksum::str(checksum, tag.as_str());
    }
    match value.untagged() {
        Value::Array(array) => {
            checksum = checksum::usize(checksum, array.len());
            if let Some(first) = array.get(0) {
                checksum = checksum::kind(checksum, first.kind());
            }
            if let Some(last) = array.get(array.len().saturating_sub(1)) {
                checksum = checksum::kind(checksum, last.kind());
            }
        }
        Value::Object(object) => {
            checksum = checksum::usize(checksum, object.len());
            if let Some(first) = object.get_index(0) {
                checksum = checksum::str(checksum, first.key());
                checksum = checksum::kind(checksum, first.value().kind());
            }
            if let Some(last) = object.get_index(object.len().saturating_sub(1)) {
                checksum = checksum::str(checksum, last.key());
                checksum = checksum::kind(checksum, last.value().kind());
            }
        }
        other => checksum = checksum::kind(checksum, other.kind()),
    }
    checksum
}

struct SameSemanticsCase {
    plan: RichPlan,
}

impl SameSemanticsCase {
    fn new() -> Self {
        Self {
            plan: RichPlan::new(2_048),
        }
    }

    fn build_pair(&self) -> Result<(Document<'static>, Document<'static>), DataError> {
        Ok((
            build_same_semantics_document(&self.plan, false)?,
            build_same_semantics_document(&self.plan, true)?,
        ))
    }
}

impl BenchmarkCase for SameSemanticsCase {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new(
            "document/compare-minimal-rich-same-semantics-2048",
            1,
            self.plan.source_len() as u64,
        )
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let (minimal, rich) = self.build_pair().map_err(|error| error.to_string())?;
        let minimal_summary = summarize(&minimal, &mut bench_ledger()).map_err(|error| error.to_string())?;
        let rich_summary = summarize(&rich, &mut bench_ledger()).map_err(|error| error.to_string())?;
        if minimal_summary.semantic_checksum != rich_summary.semantic_checksum
            || minimal.node_count() != rich.node_count()
            || covered_occurrence_count(&minimal) != covered_occurrence_count(&rich)
        {
            return Err(format!(
                "minimal/rich semantic mismatch: minimal={minimal_summary:?} rich={rich_summary:?}"
            ));
        }
        if minimal_summary.facts != 0
            || minimal_summary.provenance != 0
            || rich_summary.facts != 512
            // Provenance records were removed; nothing produces them.
            || rich_summary.provenance != 0
        {
            return Err(format!(
                "optional side-data inventory drifted: minimal={minimal_summary:?} rich={rich_summary:?}"
            ));
        }
        Ok(PreflightReceipt::new(
            minimal_summary.semantic_checksum,
            format!(
                "fixture_id=minimal-rich-same-semantics-v1 logical_nodes=2049 semantic_relationships=2048 authored_occurrences=2048 unique_keys=1792 duplicate_occurrences=256 minimal_facts=0 minimal_provenance=0 rich_facts=512 rich_provenance=0 rich_source_string_values=2048 rich_source_keys=2048 rich_stored_string_values=0 rich_stored_keys=0 rich_stored_integer_refs=0 rich_stored_decimal_coefficient_refs=0 rich_decoded_arena_len=0 rich_decoded_arena_capacity=0 semantic_checksum=0x{:016x} equal_semantic_projection=true externally_pinned_source_bytes=NotYetImplemented",
                minimal_summary.semantic_checksum,
            ),
        ))
    }

    fn run(&mut self) -> u64 {
        let (minimal, rich) = self.build_pair().expect("same-semantics fixtures build");
        let minimal = summarize(&minimal, &mut bench_ledger()).expect("minimal summary");
        let rich = summarize(&rich, &mut bench_ledger()).expect("rich summary");
        minimal.semantic_checksum ^ rich.semantic_checksum ^ rich.facts as u64
    }
}

const LEGACY_ROUTE_WIDTH: usize = 4_096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LegacyRelationshipFixture {
    ArrayDirect,
    ArrayOwnerReuse,
    ArrayIrregularIndexed,
    Object { passes: usize },
    ObjectSubtree { passes: usize },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LegacyRelationshipOperation {
    TopologyTargetOnly,
    TopologyUnkeyedFull,
    TopologyKeyedFull,
    ArrayTraversal,
    ObjectLookupIteration,
    MaterializeRoot,
    MaterializeSubtree,
}

struct LegacyRelationshipCase {
    name: &'static str,
    fixture_id: &'static str,
    fixture_recipe: &'static str,
    fixture_hash: u64,
    role: &'static str,
    fixture: LegacyRelationshipFixture,
    operation: LegacyRelationshipOperation,
    operations: u64,
    resources: ResourceContext<'static>,
    document: Document<'static>,
    lookup_keys: Option<Vec<String>>,
}

const LEGACY_RECEIPT_FIELD_ORDER: &[&str] = &[
    "receipt_schema",
    "lane",
    "evidence_role",
    "alias_of",
    "independent_evidence",
    "fixture_id",
    "fixture_revision",
    "fixture_recipe_hash",
    "operations_per_invocation",
    "work_items",
    "operation",
    "fixture_shape",
    "logical_nodes",
    "authored_occurrences",
    "semantic_relationships",
    "keyed_occurrences",
    "operation_key_count",
    "materialized_relationship_count",
    "timed_lookup_key_construction",
    "precomputed_lookup_key_count",
    "physical_authority",
    "occurrence_authority",
    "array_authority",
    "object_authority",
    "node_record_bytes",
    "occurrence_record_bytes",
    "array_projection_record_bytes",
    "object_projection_record_bytes",
    "object_projection_index_record_bytes",
    "node_len",
    "node_capacity",
    "occurrence_len",
    "occurrence_capacity",
    "array_projection_len",
    "array_projection_capacity",
    "object_projection_len",
    "object_projection_capacity",
    "object_projection_index_len",
    "object_projection_index_capacity",
    "node_table_capacity_bytes",
    "occurrence_table_capacity_bytes",
    "array_projection_capacity_bytes",
    "object_projection_capacity_bytes",
    "object_projection_index_capacity_bytes",
    "relationship_capacity_bytes",
    "decoded_text_arena_capacity_bytes",
    "fact_table_capacity_bytes",
    "identity_table_shallow_bytes",
    "unchanged_shallow_backing_capacity_bytes",
    "legacy_document_storage_inline_bytes",
    "legacy_relationship_owner_inline_bytes",
    "legacy_fixed_nonowner_inline_bytes",
    "legacy_whole_shallow_capacity_bytes",
    "shallow_table_capacity_bytes",
    "semantic_checksum_schema",
    "semantic_checksum",
    "operation_checksum",
    "schema5_claims",
    "cache_observation_eligible",
    "layout_profile_eligible",
];

fn legacy_operation_token(operation: LegacyRelationshipOperation) -> &'static str {
    match operation {
        LegacyRelationshipOperation::TopologyTargetOnly => "topology-target-only",
        LegacyRelationshipOperation::TopologyUnkeyedFull => "topology-unkeyed-full",
        LegacyRelationshipOperation::TopologyKeyedFull => "topology-keyed-full",
        LegacyRelationshipOperation::ArrayTraversal => "array-traversal",
        LegacyRelationshipOperation::ObjectLookupIteration => "object-lookup-iteration",
        LegacyRelationshipOperation::MaterializeRoot => "materialize-root",
        LegacyRelationshipOperation::MaterializeSubtree => "materialize-subtree",
    }
}

fn legacy_fixture_token(fixture: LegacyRelationshipFixture) -> &'static str {
    match fixture {
        LegacyRelationshipFixture::ArrayDirect => "array-direct",
        LegacyRelationshipFixture::ArrayOwnerReuse => "array-owner-reuse",
        LegacyRelationshipFixture::ArrayIrregularIndexed => "array-irregular-indexed",
        LegacyRelationshipFixture::Object { passes: 1 } => "object-unique",
        LegacyRelationshipFixture::Object { passes: 2 } => "object-duplicate-50",
        LegacyRelationshipFixture::Object { passes: 10 } => "object-duplicate-90",
        LegacyRelationshipFixture::ObjectSubtree { passes: 2 } => "object-subtree-duplicate-50",
        LegacyRelationshipFixture::Object { .. } | LegacyRelationshipFixture::ObjectSubtree { .. } => "invalid",
    }
}

fn legacy_receipt_checksum(fields: &[(&str, String)]) -> u64 {
    fields.iter().fold(checksum::OFFSET, |value, (key, field)| {
        let value = checksum::usize(value, key.len());
        let value = checksum::str(value, key);
        let value = checksum::usize(value, field.len());
        checksum::str(value, field)
    })
}

fn parse_decimal_field(fields: &BTreeMap<&str, &str>, key: &str) -> Result<usize, String> {
    fields
        .get(key)
        .ok_or_else(|| format!("legacy receipt missing {key}"))?
        .parse::<usize>()
        .map_err(|_| format!("legacy receipt {key} is not an unsigned decimal"))
}

fn parse_hex_field(fields: &BTreeMap<&str, &str>, key: &str) -> Result<u64, String> {
    let value = fields.get(key).ok_or_else(|| format!("legacy receipt missing {key}"))?;
    let digits = value
        .strip_prefix("0x")
        .filter(|digits| digits.len() == 16)
        .ok_or_else(|| format!("legacy receipt {key} is not exact 16-digit hex"))?;
    u64::from_str_radix(digits, 16).map_err(|_| format!("legacy receipt {key} is not exact 16-digit hex"))
}

fn checked_capacity_bytes(
    fields: &BTreeMap<&str, &str>,
    capacity: &str,
    width: &str,
    bytes: &str,
) -> Result<(), String> {
    let expected = parse_decimal_field(fields, capacity)?
        .checked_mul(parse_decimal_field(fields, width)?)
        .ok_or_else(|| format!("legacy receipt {bytes} multiplication overflow"))?;
    if parse_decimal_field(fields, bytes)? != expected {
        return Err(format!("legacy receipt {bytes} capacity equation mismatch"));
    }
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "one closed parser keeps the complete schema-4 field and equation contract auditable"
)]
pub(crate) fn validate_legacy_schema4_receipt(
    detail: &str,
    preflight_checksum: u64,
    expected_lane: &str,
) -> Result<(), String> {
    let tokens: Vec<_> = detail.split_ascii_whitespace().collect();
    if tokens.len() != LEGACY_RECEIPT_FIELD_ORDER.len() + 1 {
        return Err(format!(
            "legacy receipt field count={} expected {}",
            tokens.len(),
            LEGACY_RECEIPT_FIELD_ORDER.len() + 1
        ));
    }
    let mut fields = BTreeMap::new();
    let mut checksum_fields = Vec::with_capacity(LEGACY_RECEIPT_FIELD_ORDER.len());
    for (position, token) in tokens.iter().enumerate() {
        let (key, value) = token
            .split_once('=')
            .filter(|(key, value)| !key.is_empty() && !value.is_empty() && !value.contains('='))
            .ok_or_else(|| format!("legacy receipt token {position} is malformed"))?;
        let expected_key = if position < LEGACY_RECEIPT_FIELD_ORDER.len() {
            LEGACY_RECEIPT_FIELD_ORDER[position]
        } else {
            "physical_checksum"
        };
        if key != expected_key {
            return Err(format!(
                "legacy receipt field {position} is {key:?}, expected {expected_key:?}"
            ));
        }
        if fields.insert(key, value).is_some() {
            return Err(format!("legacy receipt duplicates field {key}"));
        }
        if key != "physical_checksum" {
            checksum_fields.push((key, value.to_owned()));
        }
    }
    let spec = LEGACY_RELATIONSHIP_SPECS
        .iter()
        .find(|spec| spec.name == expected_lane)
        .ok_or_else(|| format!("unknown legacy receipt lane {expected_lane:?}"))?;
    for (key, expected) in [
        ("receipt_schema", "4"),
        ("lane", expected_lane),
        ("evidence_role", "primary"),
        ("alias_of", "none"),
        ("independent_evidence", "true"),
        ("fixture_id", spec.fixture_id),
        ("fixture_revision", "1"),
        ("operation", legacy_operation_token(spec.operation)),
        ("fixture_shape", legacy_fixture_token(spec.fixture)),
        ("timed_lookup_key_construction", "false"),
        ("physical_authority", "legacy-table-layout-v1"),
        ("occurrence_authority", "legacy-occurrence-record-v1"),
        ("array_authority", "legacy-copied-node-id-projection-v1"),
        ("object_authority", "legacy-copied-target-key-projection-v1"),
        ("semantic_checksum_schema", "jqf-value-fnv1a64-v1"),
        ("schema5_claims", "false"),
        ("cache_observation_eligible", "true"),
        ("layout_profile_eligible", "true"),
    ] {
        if fields.get(key).copied() != Some(expected) {
            return Err(format!("legacy receipt {key} identity mismatch"));
        }
    }
    for (key, expected) in [
        // RE-PINNED (108, 2026-08-10): the unaccounted-document extraction
        // removed the plain `NodeSemantic::Number` variant, shrinking the node
        // header 48 -> 40 back to its pre-093 width.
        // RE-PINNED (AB campaign): the per-occurrence `position` field moved
        // u64 -> u32 (it is minted by a u32 counter and read only widened),
        // shrinking `OccurrenceRecord` 40 -> 32 and the rich sidecar 24 -> 20.
        ("node_record_bytes", 40usize),
        ("occurrence_record_bytes", 32),
        ("array_projection_record_bytes", 4),
        ("object_projection_record_bytes", 16),
        ("object_projection_index_record_bytes", 4),
    ] {
        if parse_decimal_field(&fields, key)? != expected {
            return Err(format!("legacy receipt {key} width mismatch"));
        }
    }
    let expected_operations =
        usize::try_from(spec.operations).map_err(|_| "legacy operations do not fit usize".to_owned())?;
    if parse_hex_field(&fields, "fixture_recipe_hash")? != spec.fixture_hash
        || parse_decimal_field(&fields, "operations_per_invocation")? != expected_operations
        || parse_decimal_field(&fields, "work_items")? != expected_operations
    {
        return Err("legacy receipt frozen fixture/work identity mismatch".to_owned());
    }
    let expected_lookup_keys = usize::from(matches!(
        spec.operation,
        LegacyRelationshipOperation::ObjectLookupIteration
    ))
    .checked_mul(2_048)
    .ok_or_else(|| "legacy lookup key count overflow".to_owned())?;
    let expected_operation_keys = match spec.operation {
        LegacyRelationshipOperation::TopologyKeyedFull => 4_096,
        LegacyRelationshipOperation::ObjectLookupIteration => 2_048,
        _ => 0,
    };
    let expected_materialized_relationships = match spec.operation {
        LegacyRelationshipOperation::MaterializeRoot => 4_096,
        LegacyRelationshipOperation::MaterializeSubtree => 2_048,
        _ => 0,
    };
    if parse_decimal_field(&fields, "precomputed_lookup_key_count")? != expected_lookup_keys
        || parse_decimal_field(&fields, "operation_key_count")? != expected_operation_keys
        || parse_decimal_field(&fields, "materialized_relationship_count")? != expected_materialized_relationships
    {
        return Err("legacy receipt operation attribution mismatch".to_owned());
    }
    let (expected_nodes, expected_occurrences, expected_relationships, expected_keyed, expected_array, expected_object) =
        match spec.fixture {
            LegacyRelationshipFixture::ArrayDirect => (4_099, 4_097, 4_096, 0, 4_096, 0),
            LegacyRelationshipFixture::ArrayOwnerReuse | LegacyRelationshipFixture::ArrayIrregularIndexed => {
                (8_194, 8_192, 4_096, 0, 4_096, 0)
            }
            LegacyRelationshipFixture::Object { passes } => {
                let occurrences = 2_048usize
                    .checked_mul(passes)
                    .ok_or_else(|| "legacy object occurrence count overflow".to_owned())?;
                (
                    occurrences
                        .checked_add(1)
                        .ok_or_else(|| "legacy object node count overflow".to_owned())?,
                    occurrences,
                    2_048,
                    occurrences,
                    0,
                    2_048,
                )
            }
            LegacyRelationshipFixture::ObjectSubtree { passes: 2 } => (4_098, 4_097, 2_049, 4_097, 0, 2_049),
            LegacyRelationshipFixture::ObjectSubtree { .. } => {
                return Err("unsupported legacy subtree fixture".to_owned());
            }
        };
    for (key, expected) in [
        ("logical_nodes", expected_nodes),
        ("authored_occurrences", expected_occurrences),
        ("semantic_relationships", expected_relationships),
        ("keyed_occurrences", expected_keyed),
        ("array_projection_len", expected_array),
        ("object_projection_len", expected_object),
        ("object_projection_index_len", expected_object),
    ] {
        if parse_decimal_field(&fields, key)? != expected {
            return Err(format!("legacy receipt {key} fixture equation mismatch"));
        }
    }
    for (len, capacity) in [
        ("node_len", "node_capacity"),
        ("occurrence_len", "occurrence_capacity"),
        ("array_projection_len", "array_projection_capacity"),
        ("object_projection_len", "object_projection_capacity"),
        ("object_projection_index_len", "object_projection_index_capacity"),
    ] {
        if parse_decimal_field(&fields, len)? > parse_decimal_field(&fields, capacity)? {
            return Err(format!("legacy receipt {len} exceeds {capacity}"));
        }
    }
    checked_capacity_bytes(
        &fields,
        "node_capacity",
        "node_record_bytes",
        "node_table_capacity_bytes",
    )?;
    checked_capacity_bytes(
        &fields,
        "occurrence_capacity",
        "occurrence_record_bytes",
        "occurrence_table_capacity_bytes",
    )?;
    checked_capacity_bytes(
        &fields,
        "array_projection_capacity",
        "array_projection_record_bytes",
        "array_projection_capacity_bytes",
    )?;
    checked_capacity_bytes(
        &fields,
        "object_projection_capacity",
        "object_projection_record_bytes",
        "object_projection_capacity_bytes",
    )?;
    checked_capacity_bytes(
        &fields,
        "object_projection_index_capacity",
        "object_projection_index_record_bytes",
        "object_projection_index_capacity_bytes",
    )?;
    let checked_sum = |keys: &[&str]| -> Result<usize, String> {
        keys.iter().try_fold(0usize, |total, key| {
            total
                .checked_add(parse_decimal_field(&fields, key)?)
                .ok_or_else(|| format!("legacy receipt sum overflow at {key}"))
        })
    };
    if parse_decimal_field(&fields, "logical_nodes")? != parse_decimal_field(&fields, "node_len")?
        || parse_decimal_field(&fields, "authored_occurrences")? != parse_decimal_field(&fields, "occurrence_len")?
        || parse_decimal_field(&fields, "keyed_occurrences")? > parse_decimal_field(&fields, "authored_occurrences")?
        || parse_decimal_field(&fields, "object_projection_len")?
            != parse_decimal_field(&fields, "object_projection_index_len")?
    {
        return Err("legacy receipt logical/table count equation mismatch".to_owned());
    }
    if parse_decimal_field(&fields, "relationship_capacity_bytes")?
        != checked_sum(&[
            "occurrence_table_capacity_bytes",
            "array_projection_capacity_bytes",
            "object_projection_capacity_bytes",
            "object_projection_index_capacity_bytes",
        ])?
    {
        return Err("legacy receipt relationship capacity equation mismatch".to_owned());
    }
    if parse_decimal_field(&fields, "unchanged_shallow_backing_capacity_bytes")?
        != checked_sum(&[
            "decoded_text_arena_capacity_bytes",
            "fact_table_capacity_bytes",
            "identity_table_shallow_bytes",
        ])?
    {
        return Err("legacy receipt unchanged backing equation mismatch".to_owned());
    }
    if parse_decimal_field(&fields, "legacy_document_storage_inline_bytes")? != 1_384
        || parse_decimal_field(&fields, "legacy_relationship_owner_inline_bytes")? != 320
        || parse_decimal_field(&fields, "legacy_fixed_nonowner_inline_bytes")? != 1_064
    {
        return Err("legacy receipt fixed inline constants mismatch".to_owned());
    }
    if parse_decimal_field(&fields, "shallow_table_capacity_bytes")?
        != checked_sum(&[
            "node_table_capacity_bytes",
            "relationship_capacity_bytes",
            "decoded_text_arena_capacity_bytes",
            "fact_table_capacity_bytes",
        ])?
    {
        return Err("legacy receipt shallow table equation mismatch".to_owned());
    }
    if parse_decimal_field(&fields, "legacy_whole_shallow_capacity_bytes")?
        != checked_sum(&[
            "legacy_document_storage_inline_bytes",
            "node_table_capacity_bytes",
            "relationship_capacity_bytes",
            "unchanged_shallow_backing_capacity_bytes",
        ])?
    {
        return Err("legacy receipt whole-shallow equation mismatch".to_owned());
    }
    let physical_checksum = parse_hex_field(&fields, "physical_checksum")?;
    if physical_checksum != preflight_checksum || physical_checksum != legacy_receipt_checksum(&checksum_fields) {
        return Err("legacy receipt physical checksum mismatch".to_owned());
    }
    if parse_hex_field(&fields, "semantic_checksum")? != spec.semantic_checksum
        || parse_hex_field(&fields, "operation_checksum")? != spec.operation_checksum
    {
        return Err("legacy receipt frozen semantic/operation checksum mismatch".to_owned());
    }
    Ok(())
}

pub(crate) fn validate_data_evidence_role_receipt(lane: &str, detail: &str) -> Result<bool, String> {
    let expected = match lane {
        "json/decode/rich/nested" => ("alias", "json/decode/full/nested", "false"),
        "json/decode/rich/escape-heavy" => ("alias", "json/decode/full/escape-heavy", "false"),
        "json/decode/rich/wide-duplicate" => ("alias", "json/decode/full/wide-duplicate", "false"),
        "json/decode/rich/deep" => ("alias", "json/decode/full/deep", "false"),
        "json/decode/full/nested"
        | "json/decode/full/escape-heavy"
        | "json/decode/full/wide-duplicate"
        | "json/decode/full/deep"
        | "legacy/topology/target-only-4097"
        | "legacy/topology/unkeyed-full-8192"
        | "legacy/topology/keyed-full-4096"
        | "legacy/array/direct-traversal-4096"
        | "legacy/array/owner-reuse-traversal-4096"
        | "legacy/array/irregular-indexed-traversal-4096"
        | "legacy/object/unique-lookup-iteration-2048"
        | "legacy/object/duplicate-50-lookup-iteration-2048"
        | "legacy/object/duplicate-90-lookup-iteration-2048"
        | "legacy/materialize/root-array-direct-4096"
        | "legacy/materialize/subtree-object-duplicate-50" => ("primary", "none", "true"),
        _ => return Ok(false),
    };
    let mut observed = BTreeMap::new();
    for token in detail.split_ascii_whitespace() {
        let Some((key, value)) = token.split_once('=') else {
            continue;
        };
        if matches!(key, "evidence_role" | "alias_of" | "independent_evidence") && observed.insert(key, value).is_some()
        {
            return Err(format!("{lane} duplicates role field {key}"));
        }
    }
    for (key, value) in [
        ("evidence_role", expected.0),
        ("alias_of", expected.1),
        ("independent_evidence", expected.2),
    ] {
        if observed.get(key).copied() != Some(value) {
            return Err(format!("{lane} has mismatched or missing {key}"));
        }
    }
    Ok(true)
}

impl LegacyRelationshipCase {
    fn new(spec: LegacyRelationshipSpec) -> Self {
        let resources = benchmark_resources().expect("legacy inventory resources");
        let document =
            build_legacy_relationship_fixture(spec.fixture, &resources).expect("legacy inventory fixture builds");
        let lookup_keys = (spec.operation == LegacyRelationshipOperation::ObjectLookupIteration)
            .then(|| (0..2_048).map(|index| format!("key-{index:04}")).collect());
        Self {
            name: spec.name,
            fixture_id: spec.fixture_id,
            fixture_recipe: spec.fixture_recipe,
            fixture_hash: spec.fixture_hash,
            role: "primary",
            fixture: spec.fixture,
            operation: spec.operation,
            operations: spec.operations,
            resources,
            document,
            lookup_keys,
        }
    }

    fn operation_checksum(&mut self) -> Result<(u64, usize, usize), DataError> {
        match self.operation {
            LegacyRelationshipOperation::TopologyTargetOnly
            | LegacyRelationshipOperation::TopologyUnkeyedFull
            | LegacyRelationshipOperation::TopologyKeyedFull => self.topology_checksum(self.operation),
            LegacyRelationshipOperation::ArrayTraversal => self.array_checksum(),
            LegacyRelationshipOperation::ObjectLookupIteration => self.object_checksum(),
            LegacyRelationshipOperation::MaterializeRoot => {
                let value = self.document.materialize_root(&mut self.resources)?;
                let (_, relationships, _, _) = semantic_shape(&value);
                Ok((checksum::value(&value), relationships, relationships))
            }
            LegacyRelationshipOperation::MaterializeSubtree => {
                let root = self.document.value_view(self.document.root_handle())?;
                let selected = match self.fixture {
                    LegacyRelationshipFixture::ArrayDirect
                    | LegacyRelationshipFixture::ArrayOwnerReuse
                    | LegacyRelationshipFixture::ArrayIrregularIndexed => root
                        .array()?
                        .ok_or(DataError::InvalidDocument)?
                        .get(LEGACY_ROUTE_WIDTH / 2)
                        .ok_or(DataError::InvalidDocument)?,
                    LegacyRelationshipFixture::ObjectSubtree { .. } => root
                        .object()?
                        .ok_or(DataError::InvalidDocument)?
                        .get("subtree")
                        .ok_or(DataError::InvalidDocument)?,
                    LegacyRelationshipFixture::Object { .. } => {
                        return Err(DataError::InvalidDocument);
                    }
                };
                let value = self
                    .document
                    .materialize_node(self.document.node_handle(selected.node())?, &mut self.resources)?;
                let (nodes, relationships, _, _) = semantic_shape(&value);
                Ok((checksum::value(&value), nodes, relationships))
            }
        }
    }

    fn topology_checksum(&self, operation: LegacyRelationshipOperation) -> Result<(u64, usize, usize), DataError> {
        let mut resources = reader_resources()?;
        let mut reader = self.document.topology_reader(&mut resources)?;
        let limit = BatchLimit::new(512).ok_or(DataError::InvalidDocument)?;
        let mut checksum = checksum::OFFSET;
        let mut occurrences = 0usize;
        let mut keyed = 0usize;
        loop {
            match reader.poll_batch(limit, &mut resources)? {
                ReaderPoll::Batch(TopologyBatch::Nodes(_)) => {}
                ReaderPoll::Batch(TopologyBatch::Occurrences(batch)) => {
                    for occurrence in &batch {
                        let occurrence = occurrence?;
                        occurrences += 1;
                        checksum = checksum::u64(checksum, occurrence.target().get());
                        if operation != LegacyRelationshipOperation::TopologyTargetOnly {
                            checksum = checksum::u64(checksum, occurrence.position());
                            checksum = checksum::str(checksum, occurrence.role().as_str());
                            checksum = owner_checksum(checksum, occurrence.owner());
                            if let Some(key) = occurrence.key_text() {
                                keyed += 1;
                                checksum = checksum::str(checksum, key);
                            }
                        }
                    }
                }
                ReaderPoll::Pending => renew(&mut resources)?,
                ReaderPoll::End(_) => return Ok((checksum, occurrences, keyed)),
            }
        }
    }

    fn array_checksum(&self) -> Result<(u64, usize, usize), DataError> {
        let array = self
            .document
            .value_view(self.document.root_handle())?
            .array()?
            .ok_or(DataError::InvalidDocument)?;
        let mut checksum = checksum::usize(checksum::OFFSET, array.len());
        for value in array.iter() {
            checksum = checksum::u64(checksum, value.node().get());
        }
        Ok((checksum, array.len(), 0))
    }

    fn object_checksum(&self) -> Result<(u64, usize, usize), DataError> {
        let object = self
            .document
            .value_view(self.document.root_handle())?
            .object()?
            .ok_or(DataError::InvalidDocument)?;
        let mut checksum = checksum::usize(checksum::OFFSET, object.len());
        for entry in object.iter() {
            let entry = entry?;
            checksum = checksum::str(checksum, entry.key());
            checksum = checksum::u64(checksum, entry.value().node().get());
        }
        let keys = self.lookup_keys.as_ref().ok_or(DataError::InvalidDocument)?;
        for key in keys {
            let value = object.get(key).ok_or(DataError::InvalidDocument)?;
            checksum = checksum::str(checksum, key);
            checksum = checksum::u64(checksum, value.node().get());
        }
        Ok((checksum, object.len() + 2_048, object.len()))
    }

    fn validate_fixture(&self, work_items: usize, keyed: usize) -> Result<(), String> {
        let observed_hash = crate::fixture::fnv1a64(self.fixture_recipe.as_bytes());
        if observed_hash != self.fixture_hash {
            return Err(format!(
                "{} fixture recipe drifted: observed=0x{observed_hash:016x} frozen=0x{:016x}",
                self.name, self.fixture_hash
            ));
        }
        let occurrences = covered_occurrence_count(&self.document);
        let expected_occurrences = match self.fixture {
            LegacyRelationshipFixture::ArrayDirect => LEGACY_ROUTE_WIDTH + 1,
            LegacyRelationshipFixture::ArrayOwnerReuse | LegacyRelationshipFixture::ArrayIrregularIndexed => {
                LEGACY_ROUTE_WIDTH * 2
            }
            LegacyRelationshipFixture::Object { passes } => 2_048 * passes,
            LegacyRelationshipFixture::ObjectSubtree { passes } => 2_048 * passes + 1,
        };
        if occurrences != expected_occurrences {
            return Err(format!(
                "{} authored occurrences={occurrences}, expected {expected_occurrences}",
                self.name
            ));
        }
        match self.operation {
            LegacyRelationshipOperation::TopologyTargetOnly
            | LegacyRelationshipOperation::TopologyUnkeyedFull
            | LegacyRelationshipOperation::TopologyKeyedFull => {
                if work_items != occurrences {
                    return Err(format!(
                        "{} topology work={work_items}, expected {occurrences}",
                        self.name
                    ));
                }
                let expected_keyed = if matches!(self.operation, LegacyRelationshipOperation::TopologyKeyedFull) {
                    occurrences
                } else {
                    0
                };
                if keyed != expected_keyed {
                    return Err(format!(
                        "{} keyed occurrences={keyed}, expected {expected_keyed}",
                        self.name
                    ));
                }
            }
            LegacyRelationshipOperation::ArrayTraversal => {
                if work_items != LEGACY_ROUTE_WIDTH {
                    return Err(format!(
                        "{} projected array items={work_items}, expected {LEGACY_ROUTE_WIDTH}",
                        self.name
                    ));
                }
            }
            LegacyRelationshipOperation::ObjectLookupIteration => {
                if self.lookup_keys.as_ref().map(Vec::len) != Some(2_048) || keyed != 2_048 || work_items != 4_096 {
                    return Err(format!(
                        "{} object work={work_items} winners={keyed}, expected work=4096 winners=2048",
                        self.name
                    ));
                }
            }
            LegacyRelationshipOperation::MaterializeRoot => {}
            LegacyRelationshipOperation::MaterializeSubtree => {
                if !matches!(self.fixture, LegacyRelationshipFixture::ObjectSubtree { .. })
                    || work_items != 2_049
                    || keyed != 2_048
                {
                    return Err(format!(
                        "{} subtree work={work_items} relationships={keyed}, expected object nodes=2049 relationships=2048",
                        self.name
                    ));
                }
            }
        }
        Ok(())
    }
}

impl BenchmarkCase for LegacyRelationshipCase {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new(self.name, self.operations, 0)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one typed field construction mirrors the closed legacy receipt schema"
    )]
    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let (operation_checksum, work_items, operation_aux_count) =
            self.operation_checksum().map_err(|error| error.to_string())?;
        self.validate_fixture(work_items, operation_aux_count)?;
        let value = self
            .document
            .materialize_root(&mut self.resources)
            .map_err(|error| error.to_string())?;
        let semantic_checksum = checksum::value(&value);
        let operation_key_count = match self.operation {
            LegacyRelationshipOperation::TopologyTargetOnly
            | LegacyRelationshipOperation::TopologyUnkeyedFull
            | LegacyRelationshipOperation::TopologyKeyedFull
            | LegacyRelationshipOperation::ObjectLookupIteration => operation_aux_count,
            LegacyRelationshipOperation::ArrayTraversal
            | LegacyRelationshipOperation::MaterializeRoot
            | LegacyRelationshipOperation::MaterializeSubtree => 0,
        };
        let materialized_relationship_count = match self.operation {
            LegacyRelationshipOperation::MaterializeRoot | LegacyRelationshipOperation::MaterializeSubtree => {
                operation_aux_count
            }
            _ => 0,
        };
        let layout = self.document.benchmark_storage_layout_stats();
        let authored_keyed_occurrences = match self.fixture {
            LegacyRelationshipFixture::Object { passes } | LegacyRelationshipFixture::ObjectSubtree { passes } => {
                2_048usize
                    .checked_mul(passes)
                    .and_then(|value| {
                        if matches!(self.fixture, LegacyRelationshipFixture::ObjectSubtree { .. }) {
                            value.checked_add(1)
                        } else {
                            Some(value)
                        }
                    })
                    .ok_or_else(|| "legacy keyed occurrence count overflow".to_owned())?
            }
            LegacyRelationshipFixture::ArrayDirect
            | LegacyRelationshipFixture::ArrayOwnerReuse
            | LegacyRelationshipFixture::ArrayIrregularIndexed => 0,
        };
        let relationship_capacity_bytes = layout
            .occurrence_table_capacity_bytes
            .checked_add(layout.array_projection_capacity_bytes)
            .and_then(|value| value.checked_add(layout.object_projection_capacity_bytes))
            .and_then(|value| value.checked_add(layout.object_projection_index_capacity_bytes))
            .ok_or_else(|| "legacy relationship capacity byte sum overflow".to_owned())?;
        let unchanged_shallow_backing_capacity_bytes = layout
            .decoded_text_arena_capacity_bytes
            .checked_add(layout.fact_table_capacity_bytes)
            .and_then(|value| value.checked_add(layout.identity_table_shallow_bytes))
            .ok_or_else(|| "legacy unchanged shallow byte sum overflow".to_owned())?;
        let legacy_document_storage_inline_bytes = 1_384usize;
        let legacy_relationship_owner_inline_bytes = 320usize;
        let legacy_fixed_nonowner_inline_bytes = legacy_document_storage_inline_bytes
            .checked_sub(legacy_relationship_owner_inline_bytes)
            .ok_or_else(|| "legacy inline byte equation underflow".to_owned())?;
        let legacy_whole_shallow_capacity_bytes = legacy_document_storage_inline_bytes
            .checked_add(layout.node_table_capacity_bytes)
            .and_then(|value| value.checked_add(relationship_capacity_bytes))
            .and_then(|value| value.checked_add(unchanged_shallow_backing_capacity_bytes))
            .ok_or_else(|| "legacy whole-shallow byte sum overflow".to_owned())?;
        let fields = vec![
            ("receipt_schema", "4".to_owned()),
            ("lane", self.name.to_owned()),
            ("evidence_role", self.role.to_owned()),
            ("alias_of", "none".to_owned()),
            ("independent_evidence", "true".to_owned()),
            ("fixture_id", self.fixture_id.to_owned()),
            ("fixture_revision", "1".to_owned()),
            ("fixture_recipe_hash", format!("0x{:016x}", self.fixture_hash)),
            ("operations_per_invocation", self.operations.to_string()),
            ("work_items", work_items.to_string()),
            ("operation", legacy_operation_token(self.operation).to_owned()),
            ("fixture_shape", legacy_fixture_token(self.fixture).to_owned()),
            ("logical_nodes", self.document.node_count().to_string()),
            (
                "authored_occurrences",
                covered_occurrence_count(&self.document).to_string(),
            ),
            ("semantic_relationships", semantic_shape(&value).1.to_string()),
            ("keyed_occurrences", authored_keyed_occurrences.to_string()),
            ("operation_key_count", operation_key_count.to_string()),
            (
                "materialized_relationship_count",
                materialized_relationship_count.to_string(),
            ),
            ("timed_lookup_key_construction", "false".to_owned()),
            (
                "precomputed_lookup_key_count",
                self.lookup_keys.as_ref().map_or(0, Vec::len).to_string(),
            ),
            ("physical_authority", "legacy-table-layout-v1".to_owned()),
            ("occurrence_authority", "legacy-occurrence-record-v1".to_owned()),
            ("array_authority", "legacy-copied-node-id-projection-v1".to_owned()),
            ("object_authority", "legacy-copied-target-key-projection-v1".to_owned()),
            ("node_record_bytes", layout.node_record_bytes.to_string()),
            ("occurrence_record_bytes", layout.occurrence_record_bytes.to_string()),
            ("array_projection_record_bytes", "4".to_owned()),
            ("object_projection_record_bytes", "16".to_owned()),
            ("object_projection_index_record_bytes", "4".to_owned()),
            ("node_len", layout.node_len.to_string()),
            ("node_capacity", layout.node_capacity.to_string()),
            ("occurrence_len", layout.occurrence_len.to_string()),
            ("occurrence_capacity", layout.occurrence_capacity.to_string()),
            ("array_projection_len", layout.array_projection_len.to_string()),
            (
                "array_projection_capacity",
                layout.array_projection_capacity.to_string(),
            ),
            ("object_projection_len", layout.object_projection_len.to_string()),
            (
                "object_projection_capacity",
                layout.object_projection_capacity.to_string(),
            ),
            (
                "object_projection_index_len",
                layout.object_projection_index_len.to_string(),
            ),
            (
                "object_projection_index_capacity",
                layout.object_projection_index_capacity.to_string(),
            ),
            (
                "node_table_capacity_bytes",
                layout.node_table_capacity_bytes.to_string(),
            ),
            (
                "occurrence_table_capacity_bytes",
                layout.occurrence_table_capacity_bytes.to_string(),
            ),
            (
                "array_projection_capacity_bytes",
                layout.array_projection_capacity_bytes.to_string(),
            ),
            (
                "object_projection_capacity_bytes",
                layout.object_projection_capacity_bytes.to_string(),
            ),
            (
                "object_projection_index_capacity_bytes",
                layout.object_projection_index_capacity_bytes.to_string(),
            ),
            ("relationship_capacity_bytes", relationship_capacity_bytes.to_string()),
            (
                "decoded_text_arena_capacity_bytes",
                layout.decoded_text_arena_capacity_bytes.to_string(),
            ),
            (
                "fact_table_capacity_bytes",
                layout.fact_table_capacity_bytes.to_string(),
            ),
            (
                "identity_table_shallow_bytes",
                layout.identity_table_shallow_bytes.to_string(),
            ),
            (
                "unchanged_shallow_backing_capacity_bytes",
                unchanged_shallow_backing_capacity_bytes.to_string(),
            ),
            (
                "legacy_document_storage_inline_bytes",
                legacy_document_storage_inline_bytes.to_string(),
            ),
            (
                "legacy_relationship_owner_inline_bytes",
                legacy_relationship_owner_inline_bytes.to_string(),
            ),
            (
                "legacy_fixed_nonowner_inline_bytes",
                legacy_fixed_nonowner_inline_bytes.to_string(),
            ),
            (
                "legacy_whole_shallow_capacity_bytes",
                legacy_whole_shallow_capacity_bytes.to_string(),
            ),
            (
                "shallow_table_capacity_bytes",
                layout.shallow_table_capacity_bytes.to_string(),
            ),
            ("semantic_checksum_schema", "jqf-value-fnv1a64-v1".to_owned()),
            ("semantic_checksum", format!("0x{semantic_checksum:016x}")),
            ("operation_checksum", format!("0x{operation_checksum:016x}")),
            ("schema5_claims", "false".to_owned()),
            ("cache_observation_eligible", "true".to_owned()),
            ("layout_profile_eligible", "true".to_owned()),
        ];
        let physical_checksum = legacy_receipt_checksum(&fields);
        let mut detail = fields
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join(" ");
        write!(detail, " physical_checksum=0x{physical_checksum:016x}")
            .map_err(|_| "legacy receipt formatting failed".to_owned())?;
        validate_legacy_schema4_receipt(&detail, physical_checksum, self.name)?;
        Ok(PreflightReceipt::new(physical_checksum, detail))
    }

    fn run(&mut self) -> u64 {
        std::hint::black_box(&self.resources);
        self.operation_checksum().expect("legacy relationship operation").0
    }
}

fn build_legacy_relationship_fixture(
    fixture: LegacyRelationshipFixture,
    resources: &ResourceContext<'_>,
) -> Result<Document<'static>, DataError> {
    match fixture {
        LegacyRelationshipFixture::Object { passes } => {
            build_accounted_shape(AccountedShape::WideDuplicates { passes }, resources)
        }
        LegacyRelationshipFixture::ObjectSubtree { passes } => build_legacy_object_subtree_fixture(passes, resources),
        LegacyRelationshipFixture::ArrayDirect
        | LegacyRelationshipFixture::ArrayOwnerReuse
        | LegacyRelationshipFixture::ArrayIrregularIndexed => build_legacy_array_fixture(fixture, resources),
    }
}

fn build_legacy_object_subtree_fixture(
    passes: usize,
    resources: &ResourceContext<'_>,
) -> Result<Document<'static>, DataError> {
    const MEMBER_ROLE: &str = "json.object.member";
    let members = 2_048usize.checked_mul(passes).ok_or(DataError::ArithmeticOverflow)?;
    let occurrences = members.checked_add(1).ok_or(DataError::ArithmeticOverflow)?;
    let nodes = members.checked_add(2).ok_or(DataError::ArithmeticOverflow)?;
    let mut builder = AccountedDocumentBuilder::try_new("json", Some("legacy-relationship-v1"))?;
    builder.try_reserve(
        DocumentCapacity {
            nodes,
            occurrences,
            ..DocumentCapacity::default()
        },
        resources,
    )?;
    let root = builder.add_node(
        "json.object",
        AccountedSemanticNode::Object {
            member_role: MEMBER_ROLE,
        },
        None,
        resources,
    )?;
    let subtree = builder.add_node(
        "json.object",
        AccountedSemanticNode::Object {
            member_role: MEMBER_ROLE,
        },
        None,
        resources,
    )?;
    builder.add_occurrence(
        LocalOwnerRef::Node(root),
        MEMBER_ROLE,
        Some(AccountedOccurrenceKey::Text("subtree")),
        subtree,
        resources,
    )?;
    for pass in 0..passes {
        for index in (0..2_048usize).rev() {
            let key = format!("key-{index:04}");
            let integer = index
                .checked_add(pass.checked_mul(2_048).ok_or(DataError::ArithmeticOverflow)?)
                .ok_or(DataError::ArithmeticOverflow)?
                .to_string();
            let value = builder.add_node("json.number", AccountedSemanticNode::Integer(&integer), None, resources)?;
            builder.add_occurrence(
                LocalOwnerRef::Node(subtree),
                MEMBER_ROLE,
                Some(AccountedOccurrenceKey::Text(&key)),
                value,
                resources,
            )?;
        }
    }
    builder.finish(root, resources)
}

#[allow(
    clippy::too_many_lines,
    reason = "one explicit builder freezes the three legacy array-route fixture laws"
)]
fn build_legacy_array_fixture(
    fixture: LegacyRelationshipFixture,
    resources: &ResourceContext<'_>,
) -> Result<Document<'static>, DataError> {
    const ITEM_ROLE: &str = "bench.array.item";
    const AUX_ROLE: &str = "bench.array.aux";
    let occurrences = match fixture {
        LegacyRelationshipFixture::ArrayDirect => LEGACY_ROUTE_WIDTH + 1,
        LegacyRelationshipFixture::ArrayOwnerReuse | LegacyRelationshipFixture::ArrayIrregularIndexed => {
            LEGACY_ROUTE_WIDTH * 2
        }
        LegacyRelationshipFixture::Object { .. } | LegacyRelationshipFixture::ObjectSubtree { .. } => {
            return Err(DataError::InvalidDocument);
        }
    };
    let mut builder = AccountedDocumentBuilder::try_new("bench", Some("legacy-relationship-v1"))?;
    builder.try_reserve(
        DocumentCapacity {
            nodes: occurrences.saturating_add(2),
            occurrences,
            ..DocumentCapacity::default()
        },
        resources,
    )?;
    let root = builder.add_node(
        "bench.array",
        AccountedSemanticNode::Array { item_role: ITEM_ROLE },
        None,
        resources,
    )?;
    let other_owner = builder.add_node(
        "bench.topology",
        AccountedSemanticNode::Unrepresentable,
        None,
        resources,
    )?;
    for index in 0..LEGACY_ROUTE_WIDTH {
        let selected = builder.add_node(
            "bench.bool",
            AccountedSemanticNode::Bool(index & 1 == 0),
            None,
            resources,
        )?;
        builder.add_occurrence(LocalOwnerRef::Node(root), ITEM_ROLE, None, selected, resources)?;
        match fixture {
            LegacyRelationshipFixture::ArrayDirect => {}
            LegacyRelationshipFixture::ArrayOwnerReuse | LegacyRelationshipFixture::ArrayIrregularIndexed => {
                let aux = builder.add_node("bench.null", AccountedSemanticNode::Null, None, resources)?;
                builder.add_occurrence(
                    if fixture == LegacyRelationshipFixture::ArrayOwnerReuse {
                        LocalOwnerRef::Node(other_owner)
                    } else {
                        LocalOwnerRef::Node(root)
                    },
                    AUX_ROLE,
                    None,
                    aux,
                    resources,
                )?;
            }
            LegacyRelationshipFixture::Object { .. } | LegacyRelationshipFixture::ObjectSubtree { .. } => {
                unreachable!()
            }
        }
    }
    if fixture == LegacyRelationshipFixture::ArrayDirect {
        let aux = builder.add_node("bench.null", AccountedSemanticNode::Null, None, resources)?;
        builder.add_occurrence(LocalOwnerRef::Node(root), AUX_ROLE, None, aux, resources)?;
    }
    builder.finish(root, resources)
}

#[derive(Clone, Copy)]
struct LegacyRelationshipSpec {
    name: &'static str,
    fixture_id: &'static str,
    fixture_recipe: &'static str,
    fixture_hash: u64,
    fixture: LegacyRelationshipFixture,
    operation: LegacyRelationshipOperation,
    operations: u64,
    semantic_checksum: u64,
    operation_checksum: u64,
}

const LEGACY_RELATIONSHIP_SPECS: &[LegacyRelationshipSpec] = &[
    LegacyRelationshipSpec {
        name: "legacy/topology/target-only-4097",
        fixture_id: "legacy-array-direct-v1",
        fixture_recipe: "array-direct-v1:4096 selected contiguous root edges then one root auxiliary edge",
        fixture_hash: 0xe793_2bf0_89f0_30c4,
        fixture: LegacyRelationshipFixture::ArrayDirect,
        operation: LegacyRelationshipOperation::TopologyTargetOnly,
        operations: 4_097,
        semantic_checksum: 0x7b9a_51bd_22b3_f7c5,
        operation_checksum: 0xdf22_26e9_1b3c_8317,
    },
    LegacyRelationshipSpec {
        name: "legacy/topology/unkeyed-full-8192",
        fixture_id: "legacy-array-owner-reuse-v1",
        fixture_recipe: "array-owner-reuse-v1:4096 selected root edges interleaved with other-owner auxiliary edges",
        fixture_hash: 0x189a_16d0_d2ea_a2fe,
        fixture: LegacyRelationshipFixture::ArrayOwnerReuse,
        operation: LegacyRelationshipOperation::TopologyUnkeyedFull,
        operations: 8_192,
        semantic_checksum: 0x7b9a_51bd_22b3_f7c5,
        operation_checksum: 0x3a9a_9309_51db_fa25,
    },
    LegacyRelationshipSpec {
        name: "legacy/topology/keyed-full-4096",
        fixture_id: "legacy-object-duplicate-50-v1",
        fixture_recipe: "object-duplicate-50-v1:2048 keys two passes descending",
        fixture_hash: 0x9337_77f0_9ed4_4889,
        fixture: LegacyRelationshipFixture::Object { passes: 2 },
        operation: LegacyRelationshipOperation::TopologyKeyedFull,
        operations: 4_096,
        semantic_checksum: 0x893f_5904_e607_67d3,
        operation_checksum: 0x6db7_fddc_f331_ab9d,
    },
    LegacyRelationshipSpec {
        name: "legacy/array/direct-traversal-4096",
        fixture_id: "legacy-array-direct-v1",
        fixture_recipe: "array-direct-v1:4096 selected contiguous root edges then one root auxiliary edge",
        fixture_hash: 0xe793_2bf0_89f0_30c4,
        fixture: LegacyRelationshipFixture::ArrayDirect,
        operation: LegacyRelationshipOperation::ArrayTraversal,
        operations: 4_096,
        semantic_checksum: 0x7b9a_51bd_22b3_f7c5,
        operation_checksum: 0xdc0c_0af5_f494_24b5,
    },
    LegacyRelationshipSpec {
        name: "legacy/array/owner-reuse-traversal-4096",
        fixture_id: "legacy-array-owner-reuse-v1",
        fixture_recipe: "array-owner-reuse-v1:4096 selected root edges interleaved with other-owner auxiliary edges",
        fixture_hash: 0x189a_16d0_d2ea_a2fe,
        fixture: LegacyRelationshipFixture::ArrayOwnerReuse,
        operation: LegacyRelationshipOperation::ArrayTraversal,
        operations: 4_096,
        semantic_checksum: 0x7b9a_51bd_22b3_f7c5,
        operation_checksum: 0x9037_50f1_cb0c_d175,
    },
    LegacyRelationshipSpec {
        name: "legacy/array/irregular-indexed-traversal-4096",
        fixture_id: "legacy-array-irregular-indexed-v1",
        fixture_recipe: "array-irregular-indexed-v1:4096 selected root edges interleaved with same-owner auxiliary edges",
        fixture_hash: 0x014a_012d_283b_3e71,
        fixture: LegacyRelationshipFixture::ArrayIrregularIndexed,
        operation: LegacyRelationshipOperation::ArrayTraversal,
        operations: 4_096,
        semantic_checksum: 0x7b9a_51bd_22b3_f7c5,
        operation_checksum: 0x9037_50f1_cb0c_d175,
    },
    LegacyRelationshipSpec {
        name: "legacy/object/unique-lookup-iteration-2048",
        fixture_id: "legacy-object-unique-v1",
        fixture_recipe: "object-unique-v1:2048 keys one pass descending",
        fixture_hash: 0xe086_821b_7a9f_4691,
        fixture: LegacyRelationshipFixture::Object { passes: 1 },
        operation: LegacyRelationshipOperation::ObjectLookupIteration,
        operations: 4_096,
        semantic_checksum: 0x5576_04d5_54de_116b,
        operation_checksum: 0x49a8_c6f5_fd55_9fe9,
    },
    LegacyRelationshipSpec {
        name: "legacy/object/duplicate-50-lookup-iteration-2048",
        fixture_id: "legacy-object-duplicate-50-v1",
        fixture_recipe: "object-duplicate-50-v1:2048 keys two passes descending",
        fixture_hash: 0x9337_77f0_9ed4_4889,
        fixture: LegacyRelationshipFixture::Object { passes: 2 },
        operation: LegacyRelationshipOperation::ObjectLookupIteration,
        operations: 4_096,
        semantic_checksum: 0x893f_5904_e607_67d3,
        operation_checksum: 0x3cb8_3c4d_648a_ea79,
    },
    LegacyRelationshipSpec {
        name: "legacy/object/duplicate-90-lookup-iteration-2048",
        fixture_id: "legacy-object-duplicate-90-v1",
        fixture_recipe: "object-duplicate-90-v1:2048 keys ten passes descending",
        fixture_hash: 0x50b2_0345_15bd_eb3c,
        fixture: LegacyRelationshipFixture::Object { passes: 10 },
        operation: LegacyRelationshipOperation::ObjectLookupIteration,
        operations: 4_096,
        semantic_checksum: 0x2be9_fdc0_d394_d563,
        operation_checksum: 0x5b0b_6996_1d43_bd79,
    },
    LegacyRelationshipSpec {
        name: "legacy/materialize/root-array-direct-4096",
        fixture_id: "legacy-array-direct-v1",
        fixture_recipe: "array-direct-v1:4096 selected contiguous root edges then one root auxiliary edge",
        fixture_hash: 0xe793_2bf0_89f0_30c4,
        fixture: LegacyRelationshipFixture::ArrayDirect,
        operation: LegacyRelationshipOperation::MaterializeRoot,
        operations: 4_096,
        semantic_checksum: 0x7b9a_51bd_22b3_f7c5,
        operation_checksum: 0x7b9a_51bd_22b3_f7c5,
    },
    LegacyRelationshipSpec {
        name: "legacy/materialize/subtree-object-duplicate-50",
        fixture_id: "legacy-object-subtree-duplicate-50-v1",
        fixture_recipe: "object-subtree-duplicate-50-v1:root.subtree contains 2048 keys two passes descending",
        fixture_hash: 0x439b_46b6_70f1_a3a7,
        fixture: LegacyRelationshipFixture::ObjectSubtree { passes: 2 },
        operation: LegacyRelationshipOperation::MaterializeSubtree,
        operations: 2_049,
        semantic_checksum: 0x7dc6_a965_569c_f6b9,
        operation_checksum: 0x893f_5904_e607_67d3,
    },
];

fn build_legacy_relationship_case(spec: LegacyRelationshipSpec) -> LegacyRelationshipCase {
    LegacyRelationshipCase::new(spec)
}

fn legacy_relationship_inventory_cases() -> Vec<Box<dyn BenchmarkCase>> {
    LEGACY_RELATIONSHIP_SPECS
        .iter()
        .copied()
        .map(|spec| Box::new(build_legacy_relationship_case(spec)) as Box<dyn BenchmarkCase>)
        .collect()
}

pub(crate) fn legacy_relationship_preflight(name: &str) -> Result<(CaseMetadata, PreflightReceipt), String> {
    let spec = LEGACY_RELATIONSHIP_SPECS
        .iter()
        .copied()
        .find(|spec| spec.name == name)
        .ok_or_else(|| format!("unknown legacy relationship lane {name:?}"))?;
    let mut case = build_legacy_relationship_case(spec);
    Ok((case.metadata(), case.preflight()?))
}

fn is_strict_json_primary_cache_lane(name: &str) -> bool {
    matches!(
        name,
        "json/decode/full/nested"
            | "json/decode/full/escape-heavy"
            | "json/decode/full/wide-duplicate"
            | "json/decode/full/deep"
    )
}

pub(crate) fn cache_lane_preflight(name: &str) -> Result<(CaseMetadata, PreflightReceipt), String> {
    if name.starts_with("legacy/") {
        return legacy_relationship_preflight(name);
    }
    if !is_strict_json_primary_cache_lane(name) {
        return Err(format!("unknown required cache lane {name:?}"));
    }
    let mut case = build_batch2_baseline_case(name)?;
    Ok((case.metadata(), case.preflight()?))
}

pub(crate) fn cache_lane_profile(name: &str) -> Result<&'static str, String> {
    if LEGACY_RELATIONSHIP_SPECS.iter().any(|spec| spec.name == name) {
        return Ok("schema4-legacy-relationship-primary");
    }
    if is_strict_json_primary_cache_lane(name) {
        return Ok("schema4-strict-json-primary");
    }
    Err(format!("unknown required cache lane {name:?}"))
}

pub(crate) fn validate_cache_schema4_receipt(
    detail: &str,
    preflight_checksum: u64,
    expected_lane: &str,
) -> Result<(), String> {
    if expected_lane.starts_with("legacy/") {
        return validate_legacy_schema4_receipt(detail, preflight_checksum, expected_lane);
    }
    if !is_strict_json_primary_cache_lane(expected_lane) {
        return Err(format!("unknown required cache lane {expected_lane:?}"));
    }
    let (_, expected) = cache_lane_preflight(expected_lane)?;
    if expected.checksum != preflight_checksum || expected.detail != detail {
        return Err(format!(
            "strict-JSON schema-4 receipt differs from exact live primary route {expected_lane:?}"
        ));
    }
    if !validate_data_evidence_role_receipt(expected_lane, detail)? {
        return Err("strict-JSON cache receipt is not an independent primary".to_owned());
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Batch2Expected {
    logical_nodes: usize,
    semantic_relationships: usize,
    authored_occurrences: usize,
    unique_keys: usize,
    duplicate_occurrences: usize,
    maximum_depth: usize,
    source_string_values: usize,
    source_keys: usize,
    stored_string_values: usize,
    stored_keys: usize,
    stored_integer_refs: usize,
    stored_decimal_coefficient_refs: usize,
    decoded_arena_len: usize,
    semantic_checksum: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Batch2Observation {
    expected: Batch2Expected,
    facts: usize,
    provenance: usize,
    tags: usize,
    source_string_values: usize,
    source_keys: usize,
    /// Integers whose canonical spelling was already verbatim in the input, so
    /// they name a source span instead of arena bytes. Counted apart from the
    /// string and key arms because only this one can appear with neither.
    source_integer_values: usize,
    trusted_session_source_attachment: bool,
    request_retained_bytes: u64,
    request_peak_bytes: u64,
    request_authorized_peak_bytes: u64,
    route: u64,
    slot: u32,
    coverage: jqf_data::DocumentCoverage,
    layout: Option<DocumentStorageLayoutStats>,
}

#[derive(Clone, Copy)]
enum Batch2LaneKind {
    JsonFull,
    JsonRich,
}

struct Batch2BaselineCase {
    name: &'static str,
    fixture: SourceFixtureEvidence,
    source: String,
    expected: Batch2Expected,
    lane: Batch2LaneKind,
}

struct DataBuildCase {
    name: &'static str,
    fixture: SourceFixtureEvidence,
    shape: AccountedShape,
    expected: Batch2Expected,
}

struct DataBuildObservation {
    expected: Batch2Expected,
    /// Source-backed integers are a decode-route product; the direct-builder
    /// route has no source to name, so this stays zero there and the receipt
    /// still states it rather than leaving the source-ref totals unexplained.
    source_integer_values: usize,
    trusted_session_source_attachment: bool,
    layout: Option<DocumentStorageLayoutStats>,
    request_retained_bytes: u64,
    request_peak_bytes: u64,
    request_authorized_peak_bytes: u64,
}

#[allow(
    clippy::too_many_lines,
    reason = "the frozen direct-builder fixture table is intentionally visible in one place"
)]
fn batch2_data_cases() -> Vec<Box<dyn BenchmarkCase>> {
    vec![
        Box::new(DataBuildCase {
            name: "document/build-nested-balanced-json-v1",
            fixture: NESTED_JSON,
            shape: AccountedShape::Nested,
            expected: Batch2Expected {
                logical_nodes: 2_564,
                semantic_relationships: 2_563,
                authored_occurrences: 2_563,
                unique_keys: 2_051,
                duplicate_occurrences: 0,
                maximum_depth: 3,
                source_string_values: 0,
                source_keys: 0,
                stored_string_values: 512,
                stored_keys: 2_051,
                // RE-PINNED (P12): dynamic Integer/Decimal nodes route through
                // the stored-text arena, so the fixture's numbers now count as
                // stored refs instead of wide payloads.
                stored_integer_refs: 513,
                stored_decimal_coefficient_refs: 512,
                decoded_arena_len: 16_605,
                semantic_checksum: 0xb43f_970f_cc15_e0fa,
            },
        }),
        Box::new(DataBuildCase {
            name: "document/build-escape-heavy-json-v1",
            fixture: ESCAPE_HEAVY_JSON,
            shape: AccountedShape::EscapeHeavy,
            expected: Batch2Expected {
                logical_nodes: 1_025,
                semantic_relationships: 1_024,
                authored_occurrences: 1_024,
                unique_keys: 0,
                duplicate_occurrences: 0,
                maximum_depth: 1,
                source_string_values: 0,
                source_keys: 0,
                stored_string_values: 1_024,
                stored_keys: 0,
                stored_integer_refs: 0,
                stored_decimal_coefficient_refs: 0,
                decoded_arena_len: 26_624,
                semantic_checksum: 0xb125_ff7f_3de4_e861,
            },
        }),
        Box::new(DataBuildCase {
            name: "document/build-wide-object-duplicates-50-json-v1",
            fixture: WIDE_DUPLICATE_50_JSON,
            shape: AccountedShape::WideDuplicates { passes: 2 },
            expected: Batch2Expected {
                logical_nodes: 4_097,
                semantic_relationships: 2_048,
                authored_occurrences: 4_096,
                unique_keys: 2_048,
                duplicate_occurrences: 2_048,
                maximum_depth: 1,
                source_string_values: 0,
                source_keys: 0,
                stored_string_values: 0,
                stored_keys: 4_096,
                // RE-PINNED (P12): the fixture's numbers route through the
                // stored-text arena.
                stored_integer_refs: 4_096,
                stored_decimal_coefficient_refs: 0,
                decoded_arena_len: 48_042,
                semantic_checksum: 0x893f_5904_e607_67d3,
            },
        }),
        Box::new(DataBuildCase {
            name: "document/build-wide-object-duplicates-90-json-v1",
            fixture: WIDE_DUPLICATE_90_JSON,
            shape: AccountedShape::WideDuplicates { passes: 10 },
            expected: Batch2Expected {
                logical_nodes: 20_481,
                semantic_relationships: 2_048,
                authored_occurrences: 20_480,
                unique_keys: 2_048,
                duplicate_occurrences: 18_432,
                maximum_depth: 1,
                source_string_values: 0,
                source_keys: 0,
                stored_string_values: 0,
                stored_keys: 20_480,
                // RE-PINNED (P12): the fixture's numbers route through the
                // stored-text arena.
                stored_integer_refs: 20_480,
                stored_decimal_coefficient_refs: 0,
                decoded_arena_len: 255_130,
                semantic_checksum: 0x2be9_fdc0_d394_d563,
            },
        }),
        Box::new(DataBuildCase {
            name: "document/build-deep-chain-256-json-v1",
            fixture: DEEP_JSON,
            shape: AccountedShape::Deep { depth: 256 },
            expected: Batch2Expected {
                logical_nodes: 257,
                semantic_relationships: 256,
                authored_occurrences: 256,
                unique_keys: 0,
                duplicate_occurrences: 0,
                maximum_depth: 256,
                source_string_values: 0,
                source_keys: 0,
                stored_string_values: 0,
                stored_keys: 0,
                // RE-PINNED (P12): the chain's single number routes through the
                // stored-text arena.
                stored_integer_refs: 1,
                stored_decimal_coefficient_refs: 0,
                decoded_arena_len: 1,
                semantic_checksum: 0xb068_b0e3_acd1_380a,
            },
        }),
    ]
}

impl DataBuildCase {
    fn observe(&self, validate: bool) -> Result<DataBuildObservation, String> {
        let control = ContinueControl;
        let account = RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, 512 << 20, u64::MAX, 1_024))
            .map_err(|error| format!("{error:?}"))?;
        let resources = ResourceContext::new(
            account,
            &control,
            WorkMeter::try_new_v1(4_096).ok_or("invalid work meter")?,
        )
        .map_err(|error| error.to_string())?;
        let baseline = resources.snapshot();
        let document =
            build_accounted_shape(self.shape, &resources).map_err(|error| format!("accounted shape build: {error}"))?;
        let usage = resources.snapshot();
        if !validate {
            return Ok(DataBuildObservation {
                expected: self.expected,
                source_integer_values: 0,
                trusted_session_source_attachment: false,
                layout: None,
                request_retained_bytes: usage
                    .memory_current_bytes()
                    .saturating_sub(baseline.memory_current_bytes()),
                request_peak_bytes: usage.memory_peak_bytes().saturating_sub(baseline.memory_peak_bytes()),
                request_authorized_peak_bytes: usage.memory_peak_bytes().saturating_sub(baseline.memory_peak_bytes()),
            });
        }
        let text = document.text_storage_stats().map_err(|error| error.to_string())?;
        if text.trusted_session_source_attachment {
            return Err(format!(
                "{} direct-builder fixture falsely reports trusted session source attachment",
                self.name
            ));
        }
        let value = document
            .materialize_root(&mut bench_ledger())
            .map_err(|error| error.to_string())?;
        let (_, semantic_relationships, unique_keys, maximum_depth) = semantic_shape(&value);
        let authored_occurrences = document.occurrence_count().map_err(|error| error.to_string())?;
        let expected = Batch2Expected {
            logical_nodes: document.node_count(),
            semantic_relationships,
            authored_occurrences,
            unique_keys,
            duplicate_occurrences: authored_occurrences.saturating_sub(semantic_relationships),
            maximum_depth,
            source_string_values: text.source_string_values,
            source_keys: text.source_keys,
            stored_string_values: text.stored_string_values,
            stored_keys: text.stored_keys,
            stored_integer_refs: text.stored_integer_refs,
            stored_decimal_coefficient_refs: text.stored_decimal_coefficient_refs,
            decoded_arena_len: text.decoded_arena_len,
            semantic_checksum: checksum::value(&value),
        };
        if expected != self.expected {
            return Err(format!(
                "{} direct-builder baseline drifted: actual={expected:?} expected={:?}",
                self.name, self.expected
            ));
        }
        Ok(DataBuildObservation {
            expected,
            source_integer_values: text.source_integer_values,
            trusted_session_source_attachment: text.trusted_session_source_attachment,
            layout: Some(document.benchmark_storage_layout_stats()),
            request_retained_bytes: usage
                .memory_current_bytes()
                .saturating_sub(baseline.memory_current_bytes()),
            request_peak_bytes: usage.memory_peak_bytes().saturating_sub(baseline.memory_peak_bytes()),
            request_authorized_peak_bytes: usage.memory_peak_bytes().saturating_sub(baseline.memory_peak_bytes()),
        })
    }
}

impl BenchmarkCase for DataBuildCase {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new(self.name, 1, 0)
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let observation = self.observe(true)?;
        let layout = observation.layout.ok_or("layout observation missing")?;
        if layout.dynamic_existing_schema_fast_append_count == 0
            || layout.dynamic_schema_transaction_append_count == 0
            || layout
                .dynamic_existing_schema_fast_append_count
                .saturating_add(layout.dynamic_schema_transaction_append_count)
                != layout.dynamic_append_count
        {
            return Err("dynamic schema-route receipt is incomplete".to_owned());
        }
        let schema_route = format!(
            "dynamic_existing_schema_fast_append_count={} dynamic_schema_transaction_append_count={} dynamic_append_count={}",
            layout.dynamic_existing_schema_fast_append_count,
            layout.dynamic_schema_transaction_append_count,
            layout.dynamic_append_count,
        );
        let physical_checksum = checksum::u64(
            observation.expected.semantic_checksum,
            observation.request_retained_bytes,
        );
        Ok(PreflightReceipt::new(
            physical_checksum,
            format!(
                "{} receipt_schema=4 recipe_fixture_id={} recipe_fixture_hash=0x{:016x} route=AccountedDocumentBuilder::finish codec_decode=false logical_nodes={} semantic_relationships={} authored_occurrences={} unique_keys={} duplicate_occurrences={} maximum_depth={} source_string_values={} source_keys={} source_integer_values={} total_source_refs={} source_reference_count={} source_span_sum_bytes={} source_span_union_bytes={} stored_string_values={} stored_keys={} stored_integer_refs={} stored_decimal_coefficient_refs={} total_stored_refs={} decoded_arena_len={} decoded_arena_capacity={} facts=0 provenance=0 tags=0 text_ref_size={} stored_occurrence_key_size={} node_len={} node_capacity={} occurrence_len={} occurrence_capacity={} array_projection_len={} array_projection_capacity={} object_projection_len={} object_projection_capacity={} object_projection_index_len={} object_projection_index_capacity={} fact_len={} fact_capacity={} decoded_text_arena_capacity_bytes={} node_table_capacity_bytes={} occurrence_table_capacity_bytes={} array_projection_capacity_bytes={} object_projection_capacity_bytes={} object_projection_index_capacity_bytes={} fact_table_capacity_bytes={} shallow_table_capacity_bytes={} nested_record_owned_allocation_bytes=NotYetImplemented identity_table_allocation_bytes=NotYetImplemented request_retained_bytes={} request_peak_bytes={} request_authorized_peak_bytes={} allocator_requested_bytes=measurement-envelope source_identity_present={} physical_source_backing={} trusted_session_source_attachment={} coverage_source=false externally_pinned_source_bytes=NotYetImplemented semantic_checksum_schema=jqf-value-fnv1a64-v1 semantic_checksum=0x{:016x} physical_checksum=0x{physical_checksum:016x}",
                schema_route,
                self.fixture.id,
                self.fixture.hash,
                observation.expected.logical_nodes,
                observation.expected.semantic_relationships,
                observation.expected.authored_occurrences,
                observation.expected.unique_keys,
                observation.expected.duplicate_occurrences,
                observation.expected.maximum_depth,
                observation.expected.source_string_values,
                observation.expected.source_keys,
                observation.source_integer_values,
                observation.expected.source_string_values
                    + observation.expected.source_keys
                    + observation.source_integer_values,
                layout.source_reference_count,
                layout.source_span_sum_bytes,
                layout.source_span_union_bytes,
                observation.expected.stored_string_values,
                observation.expected.stored_keys,
                observation.expected.stored_integer_refs,
                observation.expected.stored_decimal_coefficient_refs,
                observation.expected.stored_string_values
                    + observation.expected.stored_keys
                    + observation.expected.stored_integer_refs
                    + observation.expected.stored_decimal_coefficient_refs,
                observation.expected.decoded_arena_len,
                layout.decoded_text_arena_capacity_bytes,
                layout.text_ref_size,
                layout.stored_occurrence_key_size,
                layout.node_len,
                layout.node_capacity,
                layout.occurrence_len,
                layout.occurrence_capacity,
                layout.array_projection_len,
                layout.array_projection_capacity,
                layout.object_projection_len,
                layout.object_projection_capacity,
                layout.object_projection_index_len,
                layout.object_projection_index_capacity,
                layout.fact_len,
                layout.fact_capacity,
                layout.decoded_text_arena_capacity_bytes,
                layout.node_table_capacity_bytes,
                layout.occurrence_table_capacity_bytes,
                layout.array_projection_capacity_bytes,
                layout.object_projection_capacity_bytes,
                layout.object_projection_index_capacity_bytes,
                layout.fact_table_capacity_bytes,
                layout.shallow_table_capacity_bytes,
                observation.request_retained_bytes,
                observation.request_peak_bytes,
                observation.request_authorized_peak_bytes,
                layout.source_identity_present,
                layout.physical_source_backing,
                observation.trusted_session_source_attachment,
                observation.expected.semantic_checksum,
            ),
        ))
    }

    fn run(&mut self) -> u64 {
        let observation = self.observe(false).expect("direct accounted data build");
        checksum::u64(
            observation.expected.semantic_checksum,
            observation.request_retained_bytes,
        )
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "the frozen fixture table is intentionally visible in one place"
)]
fn batch2_baseline_cases() -> Vec<Box<dyn BenchmarkCase>> {
    const NAMES: &[&str] = &[
        "json/decode/full/nested",
        "json/decode/rich/nested",
        "json/decode/full/escape-heavy",
        "json/decode/rich/escape-heavy",
        "json/decode/full/wide-duplicate",
        "json/decode/rich/wide-duplicate",
        "json/decode/full/deep",
        "json/decode/rich/deep",
    ];
    NAMES
        .iter()
        .map(|name| {
            Box::new(
                build_batch2_baseline_case(name).expect("closed Batch 2 baseline inventory contains only known lanes"),
            ) as Box<dyn BenchmarkCase>
        })
        .collect()
}

#[allow(
    clippy::too_many_lines,
    reason = "the exact strict-JSON cache factory keeps its four frozen fixture laws together"
)]
fn build_batch2_baseline_case(name: &str) -> Result<Batch2BaselineCase, String> {
    let (family, lane) = match name {
        "json/decode/full/nested" => ("nested", Batch2LaneKind::JsonFull),
        "json/decode/rich/nested" => ("nested", Batch2LaneKind::JsonRich),
        "json/decode/full/escape-heavy" => ("escape-heavy", Batch2LaneKind::JsonFull),
        "json/decode/rich/escape-heavy" => ("escape-heavy", Batch2LaneKind::JsonRich),
        "json/decode/full/wide-duplicate" => ("wide-duplicate", Batch2LaneKind::JsonFull),
        "json/decode/rich/wide-duplicate" => ("wide-duplicate", Batch2LaneKind::JsonRich),
        "json/decode/full/deep" => ("deep", Batch2LaneKind::JsonFull),
        "json/decode/rich/deep" => ("deep", Batch2LaneKind::JsonRich),
        _ => return Err(format!("unknown strict-JSON baseline lane {name:?}")),
    };
    let (fixture, source, expected) = match family {
        "nested" => (
            NESTED_JSON,
            nested_json(),
            Batch2Expected {
                logical_nodes: 2_564,
                semantic_relationships: 2_563,
                authored_occurrences: 2_563,
                unique_keys: 2_051,
                duplicate_occurrences: 0,
                maximum_depth: 3,
                source_string_values: 512,
                source_keys: 2_051,
                stored_string_values: 0,
                stored_keys: 0,
                stored_integer_refs: 513,
                stored_decimal_coefficient_refs: 512,
                // The 513 integers are spelled canonically in the fixture, so
                // they name their source spans and the arena holds only the
                // decimal coefficients it has to re-render.
                decoded_arena_len: 2_470,
                semantic_checksum: 0xb43f_970f_cc15_e0fa,
            },
        ),
        "escape-heavy" => (
            ESCAPE_HEAVY_JSON,
            escape_heavy_json(),
            Batch2Expected {
                logical_nodes: 1_025,
                semantic_relationships: 1_024,
                authored_occurrences: 1_024,
                unique_keys: 0,
                duplicate_occurrences: 0,
                maximum_depth: 1,
                source_string_values: 0,
                source_keys: 0,
                stored_string_values: 1_024,
                stored_keys: 0,
                stored_integer_refs: 0,
                stored_decimal_coefficient_refs: 0,
                decoded_arena_len: 26_624,
                semantic_checksum: 0xb125_ff7f_3de4_e861,
            },
        ),
        "wide-duplicate" => (
            WIDE_DUPLICATE_50_JSON,
            wide_duplicate_json(2),
            Batch2Expected {
                logical_nodes: 4_097,
                semantic_relationships: 2_048,
                authored_occurrences: 4_096,
                unique_keys: 2_048,
                duplicate_occurrences: 2_048,
                maximum_depth: 1,
                source_string_values: 0,
                // Demand-scoped minimal coverage retains one key per unique object
                // winner, so duplicate authored keys no longer count here.
                source_keys: 2_048,
                stored_string_values: 0,
                stored_keys: 0,
                stored_integer_refs: 4_096,
                stored_decimal_coefficient_refs: 0,
                // Every value is a verbatim integer and every key a source
                // span, so nothing in this fixture needs arena text at all.
                decoded_arena_len: 0,
                semantic_checksum: 0x893f_5904_e607_67d3,
            },
        ),
        "deep" => (
            DEEP_JSON,
            deep_json(256),
            Batch2Expected {
                logical_nodes: 257,
                semantic_relationships: 256,
                authored_occurrences: 256,
                unique_keys: 0,
                duplicate_occurrences: 0,
                maximum_depth: 256,
                source_string_values: 0,
                source_keys: 0,
                stored_string_values: 0,
                stored_keys: 0,
                stored_integer_refs: 1,
                stored_decimal_coefficient_refs: 0,
                // The one integer at the bottom of the nest is verbatim, so
                // this document holds no arena text and still retains source.
                decoded_arena_len: 0,
                semantic_checksum: 0xb068_b0e3_acd1_380a,
            },
        ),
        _ => unreachable!("lane match established a known family"),
    };
    Ok(Batch2BaselineCase {
        name: match name {
            "json/decode/full/nested" => "json/decode/full/nested",
            "json/decode/rich/nested" => "json/decode/rich/nested",
            "json/decode/full/escape-heavy" => "json/decode/full/escape-heavy",
            "json/decode/rich/escape-heavy" => "json/decode/rich/escape-heavy",
            "json/decode/full/wide-duplicate" => "json/decode/full/wide-duplicate",
            "json/decode/rich/wide-duplicate" => "json/decode/rich/wide-duplicate",
            "json/decode/full/deep" => "json/decode/full/deep",
            "json/decode/rich/deep" => "json/decode/rich/deep",
            _ => unreachable!("lane match established a known static name"),
        },
        fixture,
        source,
        expected,
        lane,
    })
}

impl Batch2BaselineCase {
    #[allow(
        clippy::too_many_lines,
        reason = "one decode transaction keeps route, product, accounting, and receipt observations atomic"
    )]
    fn observe(&self, validate: bool) -> Result<Batch2Observation, String> {
        verify_source_fixture(self.fixture, &self.source);
        let control = ContinueControl;
        let account = RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, 512 << 20, u64::MAX, 1_024))
            .map_err(|error| format!("{error:?}"))?;
        let mut resources = ResourceContext::new(
            account,
            &control,
            WorkMeter::try_new_v1(4_096).ok_or("invalid work meter")?,
        )
        .map_err(|error| error.to_string())?;
        let baseline = resources.snapshot();
        let source = ResolvedSource::new(
            SourceRef::new(SourceId::new(23), SourceKind::Input),
            self.fixture.id,
            self.source.as_bytes(),
            0,
        );
        let mut provider = jqf_codec_json::registration()
            .map_err(|error| format!("registration: {error:?}"))?
            .decoder()
            .ok_or("JSON decoder unavailable")?
            .create_provider(
                source,
                DecodeRequest {
                    validation: ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect: &DialectId::try_new("rfc8259").expect("dialect"),
                    options: None,
                    allow_adjacent_values: false,
                    value_separator: &[],
                },
                &mut resources,
            )
            .map_err(|error| format!("provider: {error:?}"))?;
        let mut demand = CodecDemand::try_new(&resources);
        demand
            .try_insert(&DemandClause::SemanticRoot)
            .map_err(|error| error.to_string())?;
        demand
            .try_insert(&DemandClause::ValueShape)
            .map_err(|error| error.to_string())?;
        let requirement = AccessRequirement::try_whole(
            demand,
            AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            &resources,
        )
        .map_err(|error| format!("{error:?}"))?;
        let handle = provider
            .bind(&requirement)
            .map_err(|error| format!("bind: {error:?}"))?;
        let mut session = provider
            .open(&handle, &mut resources)
            .map_err(|error| format!("open: {error:?}"))?;
        let receipt = session
            .physical_route_receipt()
            .ok_or("physical route receipt missing")?;
        if receipt.route() != jqf_codec_json::FULL_PHYSICAL_ROUTE_ID || receipt.slot().get() != 0 {
            return Err(format!("unexpected JSON physical route: {receipt:?}"));
        }
        {
            let mut run = jqf_codec_core::CodecRunContext::new(&mut resources);
            run.set_cooperative_credits(4_096);
            let result = session.decode(&mut run).map_err(|error| format!("decode: {error:?}"))?;
            let (outcome, report) = result.into_parts();
            if report.route() != Some(receipt) {
                return Err("access report route differs from opened route".to_owned());
            }
            let AccessOutcome::FullDocument(product) = outcome else {
                return Err("complete requirement returned a located result".to_owned());
            };
            let document = product.document();
            if !validate {
                let usage = resources.snapshot();
                return Ok(Batch2Observation {
                    expected: self.expected,
                    facts: 0,
                    provenance: 0,
                    tags: 0,
                    source_string_values: 0,
                    source_keys: 0,
                    source_integer_values: 0,
                    trusted_session_source_attachment: false,
                    request_retained_bytes: usage
                        .memory_current_bytes()
                        .saturating_sub(baseline.memory_current_bytes()),
                    request_peak_bytes: usage.memory_peak_bytes().saturating_sub(baseline.memory_peak_bytes()),
                    request_authorized_peak_bytes: usage
                        .memory_peak_bytes()
                        .saturating_sub(baseline.memory_peak_bytes()),
                    route: receipt.route().get(),
                    slot: receipt.slot().get(),
                    coverage: document.coverage(),
                    layout: None,
                });
            }
            let text = document.text_storage_stats().map_err(|error| error.to_string())?;
            // A verbatim integer names its source span exactly as an
            // unescaped string does, so a document can retain source
            // refs — and demand the trusted attachment — with no string
            // or key of its own. The `deep` fixture is precisely that.
            // Since plan 141 S1/S2 (the JSON edit lane's out-of-band
            // AUTHORED spans, recorded at leaf and container-open time),
            // the strict-JSON whole-document route also commits the root
            // container's authored anchor, so the attachment is the rule
            // even for a ref-less document: a document attaches exactly
            // when it holds ANY span-bearing text — a retained ref or an
            // authored record.
            let refs = text
                .source_string_values
                .checked_add(text.source_keys)
                .and_then(|total| total.checked_add(text.source_integer_values))
                .ok_or("source-text reference count overflow")?;
            let root_anchored = document
                .node_source_span(document.root())
                .map_err(|error| error.to_string())?
                .is_some();
            let expects_trusted_attachment = refs != 0 || root_anchored;
            if text.trusted_session_source_attachment != expects_trusted_attachment {
                return Err(format!(
                    "{} strict JSON trusted-session attachment disagrees with retained source refs: source_string_values={} source_keys={} source_integer_values={} trusted={}",
                    self.name,
                    text.source_string_values,
                    text.source_keys,
                    text.source_integer_values,
                    text.trusted_session_source_attachment,
                ));
            }
            let value = document
                .materialize_root(&mut bench_ledger())
                .map_err(|error| error.to_string())?;
            let (semantic_nodes, relationships, unique_keys, maximum_depth) = semantic_shape(&value);
            let authored_occurrences = covered_occurrence_count(document);
            let tags = count_tags(&value);
            let expected = Batch2Expected {
                logical_nodes: document.node_count(),
                semantic_relationships: relationships,
                authored_occurrences,
                unique_keys,
                duplicate_occurrences: authored_occurrences.saturating_sub(relationships),
                maximum_depth,
                source_string_values: text.source_string_values,
                source_keys: text.source_keys,
                stored_string_values: text.stored_string_values,
                stored_keys: text.stored_keys,
                stored_integer_refs: text.stored_integer_refs,
                stored_decimal_coefficient_refs: text.stored_decimal_coefficient_refs,
                decoded_arena_len: text.decoded_arena_len,
                semantic_checksum: checksum::value(&value),
            };
            if semantic_nodes > expected.logical_nodes {
                return Err("semantic projection has more nodes than storage".to_owned());
            }
            if expected != self.expected {
                return Err(format!(
                    "{} fixed baseline drifted: actual={expected:?} expected={:?}",
                    self.name, self.expected
                ));
            }
            let facts = covered_fact_count(document);
            let provenance = covered_provenance_count(document);
            let coverage = document.coverage();
            // The strict-JSON decode route requests semantic root + shape,
            // so demand-scoped coverage retains only mandatory semantics:
            // rich topology, facts, provenance, and whole source are all
            // absent while the materialized value stays byte-identical.
            if facts != 0
                || provenance != 0
                || tags != 0
                || !coverage.contains(DocumentCapability::SemanticNodes)
                || coverage.contains(DocumentCapability::Topology)
                || coverage.contains(DocumentCapability::AttachedFacts)
                || coverage.contains(DocumentCapability::WholeSource)
            {
                return Err(format!(
                    "{} optional-side-data or coverage baseline drifted: facts={facts} provenance={provenance} tags={tags} source_string_values={} source_keys={} semantic={} topology={} facts_coverage={} source_coverage={}",
                    self.name,
                    text.source_string_values,
                    text.source_keys,
                    coverage.contains(DocumentCapability::SemanticNodes),
                    coverage.contains(DocumentCapability::Topology),
                    coverage.contains(DocumentCapability::AttachedFacts),
                    coverage.contains(DocumentCapability::WholeSource),
                ));
            }
            let usage = resources.snapshot();
            Ok(Batch2Observation {
                expected,
                facts,
                provenance,
                tags,
                source_string_values: text.source_string_values,
                source_keys: text.source_keys,
                source_integer_values: text.source_integer_values,
                trusted_session_source_attachment: text.trusted_session_source_attachment,
                request_retained_bytes: usage
                    .memory_current_bytes()
                    .saturating_sub(baseline.memory_current_bytes()),
                request_peak_bytes: usage.memory_peak_bytes().saturating_sub(baseline.memory_peak_bytes()),
                request_authorized_peak_bytes: usage.memory_peak_bytes().saturating_sub(baseline.memory_peak_bytes()),
                route: receipt.route().get(),
                slot: receipt.slot().get(),
                coverage,
                layout: Some(document.benchmark_storage_layout_stats()),
            })
        }
    }
}

impl BenchmarkCase for Batch2BaselineCase {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new(self.name, 1, self.fixture.bytes as u64)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one retained receipt keeps execution proof and storage attribution auditable"
    )]
    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let observation = self.observe(true)?;
        let layout = observation.layout.ok_or("layout observation missing")?;
        let lane = match self.lane {
            Batch2LaneKind::JsonFull => "strict-json-full",
            Batch2LaneKind::JsonRich => "strict-json-complete-rich",
        };
        let (evidence_role, alias_of) = match self.lane {
            Batch2LaneKind::JsonFull => ("primary", "none"),
            Batch2LaneKind::JsonRich => ("alias", json_full_alias_name(self.fixture.id)),
        };
        let expected = observation.expected;
        if !layout.prepared_schema_only
            || !layout.prepared_builder_accounted
            || layout.dynamic_append_count != 0
            || layout.prepared_append_count == 0
        {
            return Err("executed JSON route did not publish prepared-only schema proof".to_owned());
        }
        let recipe_fingerprint = layout
            .prepared_schema_recipe_fingerprint
            .ok_or("executed JSON route omitted its bound recipe fingerprint")?;
        let physical_checksum = checksum::u64(
            checksum::u64(expected.semantic_checksum, observation.route),
            observation.request_retained_bytes,
        );
        Ok(PreflightReceipt::new(
            physical_checksum,
            format!(
                "receipt_schema=4 fixture_id={} fixture_hash=0x{:016x} lane={lane} evidence_role={evidence_role} alias_of={alias_of} independent_evidence={} strict_validation=true builder_frontend=AccountedDocumentBuilder prepared_schema={} prepared_schema_recipe_fingerprint=0x{:016x} prepared_builder_frontend_accounted={} prepared_append_count={} dynamic_append_count={} prepared_working_peak_bytes=NotYetImplemented canonical_identity_count={} canonical_identity_utf8_bytes={} node_kind_binding_count={} occurrence_role_binding_count={} fact_kind_binding_count={} fact_role_binding_count={} identity_table_shallow_bytes={} identity_owned_retained_bytes=NotYetImplemented node_record_bytes={} occurrence_record_bytes={} stored_fact_record_bytes={} physical_route=0x{:016x} sealed_slot={} validated_bytes={} logical_nodes={} semantic_relationships={} authored_occurrences={} unique_keys={} duplicate_occurrences={} maximum_depth={} source_string_values={} source_keys={} source_integer_values={} total_source_refs={} source_reference_count={} source_span_sum_bytes={} source_span_union_bytes={} stored_string_values={} stored_keys={} stored_integer_refs={} stored_decimal_coefficient_refs={} total_stored_refs={} decoded_arena_len={} decoded_arena_capacity={} facts={} provenance={} tags={} text_ref_size={} stored_occurrence_key_size={} node_len={} node_capacity={} occurrence_len={} occurrence_capacity={} array_projection_len={} array_projection_capacity={} object_projection_len={} object_projection_capacity={} object_projection_index_len={} object_projection_index_capacity={} fact_len={} fact_capacity={} decoded_text_arena_capacity_bytes={} node_table_capacity_bytes={} occurrence_table_capacity_bytes={} array_projection_capacity_bytes={} object_projection_capacity_bytes={} object_projection_index_capacity_bytes={} fact_table_capacity_bytes={} shallow_table_capacity_bytes={} nested_record_owned_allocation_bytes=NotYetImplemented request_retained_bytes={} request_peak_bytes={} request_authorized_peak_bytes={} allocator_requested_bytes=measurement-envelope source_identity_present={} physical_source_backing={} trusted_session_source_attachment={} coverage_semantic={} coverage_topology={} coverage_facts={} coverage_source={} externally_pinned_source_bytes=NotYetImplemented semantic_checksum_schema=jqf-value-fnv1a64-v1 semantic_checksum=0x{:016x} physical_checksum=0x{physical_checksum:016x} decode_only_timed=true",
                self.fixture.id,
                self.fixture.hash,
                matches!(self.lane, Batch2LaneKind::JsonFull),
                layout.prepared_schema_only,
                recipe_fingerprint,
                layout.prepared_builder_accounted,
                layout.prepared_append_count,
                layout.dynamic_append_count,
                layout.canonical_identity_count,
                layout.canonical_identity_utf8_bytes,
                layout.node_kind_binding_count,
                layout.occurrence_role_binding_count,
                layout.fact_kind_binding_count,
                layout.fact_role_binding_count,
                layout.identity_table_shallow_bytes,
                layout.node_record_bytes,
                layout.occurrence_record_bytes,
                layout.stored_fact_record_bytes,
                observation.route,
                observation.slot,
                self.fixture.bytes,
                expected.logical_nodes,
                expected.semantic_relationships,
                expected.authored_occurrences,
                expected.unique_keys,
                expected.duplicate_occurrences,
                expected.maximum_depth,
                observation.source_string_values,
                observation.source_keys,
                observation.source_integer_values,
                observation.source_string_values + observation.source_keys + observation.source_integer_values,
                layout.source_reference_count,
                layout.source_span_sum_bytes,
                layout.source_span_union_bytes,
                expected.stored_string_values,
                expected.stored_keys,
                expected.stored_integer_refs,
                expected.stored_decimal_coefficient_refs,
                expected.stored_string_values
                    + expected.stored_keys
                    + expected.stored_integer_refs
                    + expected.stored_decimal_coefficient_refs,
                expected.decoded_arena_len,
                layout.decoded_text_arena_capacity_bytes,
                observation.facts,
                observation.provenance,
                observation.tags,
                layout.text_ref_size,
                layout.stored_occurrence_key_size,
                layout.node_len,
                layout.node_capacity,
                layout.occurrence_len,
                layout.occurrence_capacity,
                layout.array_projection_len,
                layout.array_projection_capacity,
                layout.object_projection_len,
                layout.object_projection_capacity,
                layout.object_projection_index_len,
                layout.object_projection_index_capacity,
                layout.fact_len,
                layout.fact_capacity,
                layout.decoded_text_arena_capacity_bytes,
                layout.node_table_capacity_bytes,
                layout.occurrence_table_capacity_bytes,
                layout.array_projection_capacity_bytes,
                layout.object_projection_capacity_bytes,
                layout.object_projection_index_capacity_bytes,
                layout.fact_table_capacity_bytes,
                layout.shallow_table_capacity_bytes,
                observation.request_retained_bytes,
                observation.request_peak_bytes,
                observation.request_authorized_peak_bytes,
                layout.source_identity_present,
                layout.physical_source_backing,
                observation.trusted_session_source_attachment,
                observation.coverage.contains(DocumentCapability::SemanticNodes),
                observation.coverage.contains(DocumentCapability::Topology),
                observation.coverage.contains(DocumentCapability::AttachedFacts),
                observation.coverage.contains(DocumentCapability::WholeSource),
                expected.semantic_checksum,
            ),
        ))
    }

    fn run(&mut self) -> u64 {
        let observation = self.observe(false).expect("Batch 2 fixture decode");
        checksum::u64(
            checksum::u64(observation.expected.semantic_checksum, observation.route),
            observation.request_retained_bytes,
        )
    }
}

fn json_full_alias_name(fixture_id: &str) -> &'static str {
    match fixture_id {
        "nested-catalog-v1" => "json/decode/full/nested",
        "escape-array-v1" => "json/decode/full/escape-heavy",
        "wide-duplicate-object-v1" => "json/decode/full/wide-duplicate",
        "deep-array-256-v1" => "json/decode/full/deep",
        _ => "unknown",
    }
}

fn semantic_shape(root: &Value) -> (usize, usize, usize, usize) {
    let mut nodes = 0usize;
    let mut relationships = 0usize;
    let mut unique_keys = 0usize;
    let mut maximum_depth = 0usize;
    let mut pending = vec![(root, 0usize)];
    while let Some((value, depth)) = pending.pop() {
        nodes += 1;
        maximum_depth = maximum_depth.max(depth);
        match value.untagged() {
            Value::Array(array) => {
                relationships += array.len();
                pending.extend(array.iter().map(|value| (value, depth + 1)));
            }
            Value::Object(object) => {
                relationships += object.len();
                unique_keys += object.len();
                pending.extend(object.iter().map(|entry| (entry.value(), depth + 1)));
            }
            Value::Null
            | Value::Bool(_)
            | Value::Number(_)
            | Value::String(_)
            | Value::Bytes(_)
            | Value::LocalDate(_)
            | Value::LocalTime(_)
            | Value::LocalDateTime(_)
            | Value::OffsetDateTime(_)
            | Value::Tagged { .. } => {}
        }
    }
    (nodes, relationships, unique_keys, maximum_depth)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_expectations_derive_from_fixture_laws_without_readers() {
        assert_eq!(expected_topology_reader_checksum(), TOPOLOGY_READER_CHECKSUM);
        assert_eq!(expected_fact_reader_checksum(), FACT_READER_CHECKSUM);
        assert_eq!(RichPlan::new(RICH_WIDTH).source_checksum(), RICH_SOURCE_CHECKSUM);
    }

    #[test]
    fn object_view_hot_witness_matches_independently_validated_sequence() {
        let lookup = ObjectViewLookup::new();
        let (validated_hits, _) = lookup.validate_sequence().expect("sequence validates");
        let (hot_hits, hot_checksum) = lookup.execute_hot().expect("hot lookup succeeds");
        assert_eq!(validated_hits, lookup.hits);
        assert_eq!(hot_hits, lookup.hits);
        assert_ne!(hot_checksum, checksum::OFFSET);
    }

    #[cfg(feature = "allocation-stats")]
    #[test]
    fn object_view_hot_witness_is_allocation_free() {
        let _lock = crate::ALLOCATION_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let lookup = ObjectViewLookup::new();
        let (observation, statistics) =
            jqf_bench_core::allocation::measure(|| std::hint::black_box(lookup.execute_hot()));
        assert_eq!(observation.expect("hot lookup succeeds").0, lookup.hits);
        assert_eq!(statistics.allocation_calls, 0);
        assert_eq!(statistics.reallocation_calls, 0);
        assert_eq!(statistics.requested_bytes, 0);
        assert_eq!(statistics.peak_live_bytes, 0);
        assert_eq!(statistics.retained_bytes, 0);
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one adversarial test keeps the closed receipt mutation matrix auditable"
    )]
    fn legacy_receipt_parser_is_closed_typed_and_recomputes_physical_checksum() {
        fn forge_receipt(detail: &str, replacements: &[(&str, String)]) -> (String, u64) {
            let mut checksum_fields = Vec::new();
            for token in detail.split_ascii_whitespace() {
                let (key, value) = token.split_once('=').expect("closed receipt token");
                if key == "physical_checksum" {
                    continue;
                }
                let value = replacements
                    .iter()
                    .find_map(|(replacement_key, replacement_value)| {
                        (*replacement_key == key).then_some(replacement_value.as_str())
                    })
                    .unwrap_or(value)
                    .to_owned();
                checksum_fields.push((key, value));
            }
            let checksum = legacy_receipt_checksum(&checksum_fields);
            let detail = checksum_fields
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .chain(std::iter::once(format!("physical_checksum=0x{checksum:016x}")))
                .collect::<Vec<_>>()
                .join(" ");
            (detail, checksum)
        }

        fn decimal_field(detail: &str, wanted: &str) -> usize {
            detail
                .split_ascii_whitespace()
                .find_map(|token| {
                    let (key, value) = token.split_once('=')?;
                    (key == wanted).then(|| value.parse::<usize>().expect("decimal field"))
                })
                .expect("receipt field")
        }

        let spec = LEGACY_RELATIONSHIP_SPECS[3];
        let mut case = build_legacy_relationship_case(spec);
        let receipt = case.preflight().expect("legacy direct preflight");
        validate_legacy_schema4_receipt(&receipt.detail, receipt.checksum, spec.name).expect("exact receipt validates");
        for malformed in [
            format!("{} unknown_field=1", receipt.detail),
            receipt.detail.replacen("lane=", "lane=wrong lane=", 1),
            receipt
                .detail
                .replacen("evidence_role=primary", "evidence_role=alias", 1),
            receipt.detail.replacen("fixture_revision=1", "fixture_revision=01", 1),
            receipt.detail.replacen("physical_checksum=0x", "physical_checksum=", 1),
        ] {
            assert!(
                validate_legacy_schema4_receipt(&malformed, receipt.checksum, spec.name).is_err(),
                "malformed receipt unexpectedly validated: {malformed}"
            );
        }

        let original_node_bytes = decimal_field(&receipt.detail, "node_table_capacity_bytes");
        let (forged, forged_checksum) = forge_receipt(
            &receipt.detail,
            &[(
                "node_table_capacity_bytes",
                original_node_bytes
                    .checked_add(1)
                    .expect("fixture byte count")
                    .to_string(),
            )],
        );
        assert!(
            validate_legacy_schema4_receipt(&forged, forged_checksum, spec.name).is_err(),
            "capacity equation mutation survived a recomputed checksum"
        );

        let node_capacity = decimal_field(&receipt.detail, "node_capacity");
        let forged_width = 153usize;
        let forged_node_bytes = node_capacity.checked_mul(forged_width).expect("fixture byte count");
        let (forged, forged_checksum) = forge_receipt(
            &receipt.detail,
            &[
                ("node_record_bytes", forged_width.to_string()),
                ("node_table_capacity_bytes", forged_node_bytes.to_string()),
            ],
        );
        assert!(
            validate_legacy_schema4_receipt(&forged, forged_checksum, spec.name).is_err(),
            "width plus equation-consistent byte mutation survived a recomputed checksum"
        );

        for (field, value) in [
            ("semantic_checksum", format!("0x{:016x}", spec.semantic_checksum ^ 1)),
            ("operation_checksum", format!("0x{:016x}", spec.operation_checksum ^ 1)),
        ] {
            let (forged, forged_checksum) = forge_receipt(&receipt.detail, &[(field, value)]);
            assert!(
                validate_legacy_schema4_receipt(&forged, forged_checksum, spec.name).is_err(),
                "frozen {field} mutation survived a recomputed physical checksum"
            );
        }
    }

    #[test]
    fn data_evidence_roles_are_exact_and_duplicate_fields_fail_closed() {
        assert_eq!(
            validate_data_evidence_role_receipt(
                "json/decode/rich/nested",
                "evidence_role=alias alias_of=json/decode/full/nested independent_evidence=false"
            ),
            Ok(true)
        );
        assert!(
            validate_data_evidence_role_receipt(
                "json/decode/rich/nested",
                "evidence_role=alias evidence_role=primary alias_of=json/decode/full/nested independent_evidence=false"
            )
            .is_err()
        );
        assert!(
            validate_data_evidence_role_receipt(
                "json/decode/rich/nested",
                "evidence_role=alias alias_of=none independent_evidence=false"
            )
            .is_err()
        );
        assert_eq!(
            validate_data_evidence_role_receipt("integer/parse-mixed-4096", "unrelated=true"),
            Ok(false)
        );
    }

    #[cfg(feature = "allocation-stats")]
    #[test]
    fn legacy_object_lookup_iteration_has_no_timed_key_or_heap_allocation() {
        let _lock = crate::ALLOCATION_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let spec = LEGACY_RELATIONSHIP_SPECS[6];
        let mut case = build_legacy_relationship_case(spec);
        assert_eq!(case.lookup_keys.as_ref().map(Vec::len), Some(2_048));
        let (result, statistics) =
            jqf_bench_core::allocation::measure(|| std::hint::black_box(case.operation_checksum()));
        result.expect("legacy object lookup iteration");
        assert_eq!(statistics.allocation_calls, 0);
        assert_eq!(statistics.reallocation_calls, 0);
        assert_eq!(statistics.requested_bytes, 0);
        assert_eq!(statistics.peak_live_bytes, 0);
        assert_eq!(statistics.retained_bytes, 0);
    }
}
