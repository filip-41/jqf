use std::{fmt::Write as _, sync::Arc};

use jqf_data::{
    AccountedDocumentBuilder, AccountedIntrinsicTag, AccountedOccurrenceKey, AccountedSemanticNode, BuilderCoverage,
    DataError, Document, DocumentCapability, DocumentCapacity, DocumentFact, DocumentSourceBinding,
    DocumentTextStorageStats, FactPayload, LocalOwnerRef, Value,
};
use jqf_resource::ResourceContext;
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef, Span};

use crate::checksum;

/// A deliberately leaked request context whose account the fixture documents'
/// storage stays bound to for the whole bench process.
///
/// The bench builds `Document<'static>` fixtures with the accounted builder;
/// the returned document's `Residency` references the context's account, so the
/// context must outlive every call site. Leaking is acceptable here: the bench
/// is a measurement tool, not a request server, and each leaked context is
/// one per fixture build.
fn fixture_resources() -> Result<&'static mut ResourceContext<'static>, DataError> {
    Ok(Box::leak(Box::new(benchmark_fixture_resources()?)))
}

fn benchmark_fixture_resources() -> Result<ResourceContext<'static>, DataError> {
    use jqf_resource::{RequestAccount, ResourceLimits, WorkMeter};
    static CONTROL: jqf_resource::ContinueControl = jqf_resource::ContinueControl;
    let limits = ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
    let account = RequestAccount::try_new(limits)?;
    let work = WorkMeter::try_new_v1(1).ok_or(DataError::InvalidDocument)?;
    ResourceContext::new(account, &CONTROL, work).map_err(DataError::from)
}

pub(crate) const PLAIN_WIDTH: usize = 65_536;
pub(crate) const RICH_WIDTH: usize = 32_768;
pub(crate) const RICH_LINE_BYTES: usize = 20;

/// Immutable identity for one source fixture used by the Batch 2 baseline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SourceFixtureEvidence {
    pub(crate) id: &'static str,
    pub(crate) bytes: usize,
    pub(crate) hash: u64,
}

pub(crate) const NESTED_JSON: SourceFixtureEvidence = SourceFixtureEvidence {
    id: "nested-catalog-v1",
    bytes: 29_420,
    hash: 0x8cf9_8f28_763c_a366,
};

pub(crate) const ESCAPE_HEAVY_JSON: SourceFixtureEvidence = SourceFixtureEvidence {
    id: "escape-array-v1",
    bytes: 40_961,
    hash: 0xc4a5_51ec_7bce_827d,
};

pub(crate) const WIDE_DUPLICATE_50_JSON: SourceFixtureEvidence = SourceFixtureEvidence {
    id: "wide-duplicate-object-v1",
    bytes: 64_427,
    hash: 0x074f_256c_7835_ad53,
};

pub(crate) const WIDE_DUPLICATE_90_JSON: SourceFixtureEvidence = SourceFixtureEvidence {
    id: "wide-duplicate-object-90-v1",
    bytes: 337_051,
    hash: 0xa82c_fd0d_4893_734b,
};

pub(crate) const DEEP_JSON: SourceFixtureEvidence = SourceFixtureEvidence {
    id: "deep-array-256-v1",
    bytes: 513,
    hash: 0x7160_49c2_b7c4_c0af,
};

pub(crate) fn nested_json() -> String {
    let mut value = String::from("{\"meta\":{\"version\":1},\"catalog\":[");
    for index in 0..512 {
        if index != 0 {
            value.push(',');
        }
        let _ = write!(
            value,
            "{{\"id\":{index},\"name\":\"item-{index}\",\"price\":{}.25,\"active\":true}}",
            index + 10
        );
    }
    value.push_str("]}");
    value
}

pub(crate) fn escape_heavy_json() -> String {
    let mut value = String::from("[");
    for index in 0..1_024 {
        if index != 0 {
            value.push(',');
        }
        value.push_str("\"line\\nquote\\\"slash\\\\music\\uD834\\uDD1E\"");
    }
    value.push(']');
    value
}

pub(crate) fn wide_duplicate_json(passes: usize) -> String {
    const UNIQUE: usize = 2_048;
    let mut value = String::from("{");
    for pass in 0..passes {
        for index in (0..UNIQUE).rev() {
            if pass != 0 || index != UNIQUE - 1 {
                value.push(',');
            }
            let _ = write!(value, "\"key-{index:04}\":{}", index + pass * UNIQUE);
        }
    }
    value.push('}');
    value
}

pub(crate) fn deep_json(depth: usize) -> String {
    let mut value = String::with_capacity(depth.saturating_mul(2).saturating_add(1));
    value.extend(core::iter::repeat_n('[', depth));
    value.push('0');
    value.extend(core::iter::repeat_n(']', depth));
    value
}

pub(crate) fn verify_source_fixture(evidence: SourceFixtureEvidence, source: &str) -> u64 {
    assert_eq!(source.len(), evidence.bytes, "{} byte length drifted", evidence.id);
    let hash = fnv1a64(source.as_bytes());
    assert_eq!(hash, evidence.hash, "{} hash drifted", evidence.id);
    hash
}

pub(crate) fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3)
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AccountedShape {
    Nested,
    EscapeHeavy,
    WideDuplicates { passes: usize },
    Deep { depth: usize },
}

pub(crate) fn build_accounted_shape(
    shape: AccountedShape,
    resources: &ResourceContext<'_>,
) -> Result<Document<'static>, DataError> {
    match shape {
        AccountedShape::Nested => build_accounted_nested(resources),
        AccountedShape::EscapeHeavy => build_accounted_escape_heavy(resources),
        AccountedShape::WideDuplicates { passes } => build_accounted_wide_duplicates(passes, resources),
        AccountedShape::Deep { depth } => build_accounted_deep(depth, resources),
    }
}

fn accounted_builder(_resources: &ResourceContext<'_>) -> Result<AccountedDocumentBuilder<'static>, DataError> {
    AccountedDocumentBuilder::try_new("json", Some("rfc8259"))
}

#[allow(
    clippy::too_many_lines,
    reason = "the frozen nested fixture recipe is intentionally explicit and auditable"
)]
fn build_accounted_nested(resources: &ResourceContext<'_>) -> Result<Document<'static>, DataError> {
    const OBJECT_ROLE: &str = "json.object.member";
    const ARRAY_ROLE: &str = "json.array.item";
    let mut builder = accounted_builder(resources)?;
    builder.try_reserve(
        DocumentCapacity {
            nodes: 2_564,
            occurrences: 2_563,
            ..DocumentCapacity::default()
        },
        resources,
    )?;
    let root = builder.add_node(
        "json.object",
        AccountedSemanticNode::Object {
            member_role: OBJECT_ROLE,
        },
        None,
        resources,
    )?;
    let meta = builder.add_node(
        "json.object",
        AccountedSemanticNode::Object {
            member_role: OBJECT_ROLE,
        },
        None,
        resources,
    )?;
    let version = builder.add_node("json.number", AccountedSemanticNode::Integer("1"), None, resources)?;
    let catalog = builder.add_node(
        "json.array",
        AccountedSemanticNode::Array { item_role: ARRAY_ROLE },
        None,
        resources,
    )?;
    add_accounted_member(&mut builder, root, "meta", meta, resources)?;
    add_accounted_member(&mut builder, meta, "version", version, resources)?;
    add_accounted_member(&mut builder, root, "catalog", catalog, resources)?;
    for index in 0..512 {
        let item = builder.add_node(
            "json.object",
            AccountedSemanticNode::Object {
                member_role: OBJECT_ROLE,
            },
            None,
            resources,
        )?;
        builder.add_occurrence(LocalOwnerRef::Node(catalog), ARRAY_ROLE, None, item, resources)?;
        let id = index.to_string();
        let name = format!("item-{index}");
        let price = format!("{}25", index + 10);
        let id_node = builder.add_node("json.number", AccountedSemanticNode::Integer(&id), None, resources)?;
        let name_node = builder.add_node("json.string", AccountedSemanticNode::String(&name), None, resources)?;
        let price_node = builder.add_node(
            "json.number",
            AccountedSemanticNode::Decimal {
                coefficient: &price,
                scale: 2,
            },
            None,
            resources,
        )?;
        let active_node = builder.add_node("json.bool", AccountedSemanticNode::Bool(true), None, resources)?;
        add_accounted_member(&mut builder, item, "id", id_node, resources)?;
        add_accounted_member(&mut builder, item, "name", name_node, resources)?;
        add_accounted_member(&mut builder, item, "price", price_node, resources)?;
        add_accounted_member(&mut builder, item, "active", active_node, resources)?;
    }
    builder.finish(root, resources)
}

fn build_accounted_escape_heavy(resources: &ResourceContext<'_>) -> Result<Document<'static>, DataError> {
    const ARRAY_ROLE: &str = "json.array.item";
    const VALUE: &str = "line\nquote\"slash\\music𝄞";
    let mut builder = accounted_builder(resources)?;
    builder.try_reserve(
        DocumentCapacity {
            nodes: 1_025,
            occurrences: 1_024,
            ..DocumentCapacity::default()
        },
        resources,
    )?;
    let root = builder.add_node(
        "json.array",
        AccountedSemanticNode::Array { item_role: ARRAY_ROLE },
        None,
        resources,
    )?;
    for _ in 0..1_024 {
        let node = builder.add_node("json.string", AccountedSemanticNode::String(VALUE), None, resources)?;
        builder.add_occurrence(LocalOwnerRef::Node(root), ARRAY_ROLE, None, node, resources)?;
    }
    builder.finish(root, resources)
}

fn build_accounted_wide_duplicates(
    passes: usize,
    resources: &ResourceContext<'_>,
) -> Result<Document<'static>, DataError> {
    const OBJECT_ROLE: &str = "json.object.member";
    const UNIQUE: usize = 2_048;
    let occurrences = UNIQUE.checked_mul(passes).ok_or(DataError::ArithmeticOverflow)?;
    let mut builder = accounted_builder(resources)?;
    builder.try_reserve(
        DocumentCapacity {
            nodes: occurrences.saturating_add(1),
            occurrences,
            ..DocumentCapacity::default()
        },
        resources,
    )?;
    let root = builder.add_node(
        "json.object",
        AccountedSemanticNode::Object {
            member_role: OBJECT_ROLE,
        },
        None,
        resources,
    )?;
    for pass in 0..passes {
        for index in (0..UNIQUE).rev() {
            let key = format!("key-{index:04}");
            let value = (index + pass * UNIQUE).to_string();
            let node = builder.add_node("json.number", AccountedSemanticNode::Integer(&value), None, resources)?;
            add_accounted_member(&mut builder, root, &key, node, resources)?;
        }
    }
    builder.finish(root, resources)
}

fn build_accounted_deep(depth: usize, resources: &ResourceContext<'_>) -> Result<Document<'static>, DataError> {
    const ARRAY_ROLE: &str = "json.array.item";
    let mut builder = accounted_builder(resources)?;
    builder.try_reserve(
        DocumentCapacity {
            nodes: depth.saturating_add(1),
            occurrences: depth,
            ..DocumentCapacity::default()
        },
        resources,
    )?;
    let mut arrays = Vec::with_capacity(depth);
    for _ in 0..depth {
        arrays.push(builder.add_node(
            "json.array",
            AccountedSemanticNode::Array { item_role: ARRAY_ROLE },
            None,
            resources,
        )?);
    }
    let scalar = builder.add_node("json.number", AccountedSemanticNode::Integer("0"), None, resources)?;
    for index in 0..depth {
        let target = arrays.get(index + 1).copied().unwrap_or(scalar);
        builder.add_occurrence(LocalOwnerRef::Node(arrays[index]), ARRAY_ROLE, None, target, resources)?;
    }
    builder.finish(arrays.first().copied().unwrap_or(scalar), resources)
}

fn add_accounted_member(
    builder: &mut AccountedDocumentBuilder<'static>,
    owner: jqf_data::NodeId,
    key: &str,
    target: jqf_data::NodeId,
    resources: &ResourceContext<'_>,
) -> Result<(), DataError> {
    builder.add_occurrence(
        LocalOwnerRef::Node(owner),
        "json.object.member",
        Some(AccountedOccurrenceKey::Text(key)),
        target,
        resources,
    )?;
    Ok(())
}

pub(crate) struct RichPlan {
    source: Arc<[u8]>,
    source_ref: SourceRef,
    fields: Vec<(Span, Span)>,
}

impl RichPlan {
    pub(crate) fn new(width: usize) -> Self {
        let mut source = Vec::with_capacity(width * RICH_LINE_BYTES);
        let mut fields = Vec::with_capacity(width);
        for index in 0..width {
            let start = source.len();
            let key_index = semantic_key_index(index);
            let line = format!("key{key_index:05}=value{index:05}\n");
            let separator = line.find('=').expect("the deterministic source line has a separator");
            source.extend_from_slice(line.as_bytes());
            fields.push((
                Span::from_usize(start, start + separator),
                Span::from_usize(start + separator + 1, start + line.len() - 1),
            ));
        }
        Self {
            source: Arc::from(source),
            source_ref: SourceRef::new(SourceId::new(7), SourceKind::Input),
            fields,
        }
    }

    pub(crate) fn width(&self) -> usize {
        self.fields.len()
    }

    pub(crate) fn source_len(&self) -> usize {
        self.source.len()
    }

    #[cfg(test)]
    pub(crate) fn source_checksum(&self) -> u64 {
        checksum::bytes(checksum::OFFSET, &self.source)
    }

    pub(crate) fn unique_keys(&self) -> usize {
        self.width() - self.width() / 8
    }
}

pub(crate) const fn semantic_key_index(occurrence: usize) -> usize {
    if occurrence % 8 == 7 {
        occurrence - 1
    } else {
        occurrence
    }
}

pub(crate) fn build_plain_document(width: usize) -> Result<Document<'static>, DataError> {
    // The fixture document's storage is accounted to a deliberately leaked
    // context, so the returned `Document<'static>` outlives every call site.
    let resources = fixture_resources()?;
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

pub(crate) fn build_rich_document(plan: &RichPlan) -> Result<Document<'static>, DataError> {
    let resources = fixture_resources()?;
    // The document's retained source must outlive every call site; the fixture
    // leaks one copy per build, like the leaked request context above.
    let source_bytes: &'static [u8] = Box::leak(plan.source.to_vec().into_boxed_slice());
    let whole = ResolvedSource::new(plan.source_ref, plan.source_ref.kind().as_str(), source_bytes, 0);
    let mut builder = AccountedDocumentBuilder::try_new("bench", None)?;
    // Seal the segment ONCE; per-text `binding.text` reuses the seal instead of
    // re-hashing the whole source for every field.
    let binding = DocumentSourceBinding::from_resolved(whole)?;
    builder.bind_source(binding)?;
    builder.try_reserve(
        DocumentCapacity {
            nodes: plan.width().saturating_add(1),
            occurrences: plan.width(),
            facts: plan.width().div_ceil(4),
            ..DocumentCapacity::default()
        },
        resources,
    )?;
    let root = builder.add_node(
        "bench.object",
        AccountedSemanticNode::Object {
            member_role: "bench.member",
        },
        Some(AccountedIntrinsicTag::Tagged("!catalog")),
        resources,
    )?;

    for (index, &(key_span, value_span)) in plan.fields.iter().enumerate() {
        // SAFETY: `plan.source` is the exact immutable authority the binding
        // was sealed over, retained by the caller for the whole build; the
        // metadata-checked token path avoids re-hashing the source per field.
        let value_text =
            unsafe { binding.text_from_bound_authority(whole, value_span) }.map_err(|_| DataError::InvalidDocument)?;
        let key_text =
            unsafe { binding.text_from_bound_authority(whole, key_span) }.map_err(|_| DataError::InvalidDocument)?;
        let tag = if index % 8 == 0 {
            Some(AccountedIntrinsicTag::Tagged("!entry"))
        } else {
            None
        };
        let node = builder.add_node(
            "bench.string",
            AccountedSemanticNode::SourceString(value_text),
            tag,
            resources,
        )?;
        let occurrence = builder.add_occurrence(
            LocalOwnerRef::Node(root),
            "bench.member",
            Some(AccountedOccurrenceKey::SourceText(key_text)),
            node,
            resources,
        )?;
        if index % 4 == 0 {
            let payload = FactPayload::Bool(index & 8 == 0);
            builder.add_fact(
                LocalOwnerRef::Occurrence(occurrence),
                "bench.comment",
                "bench.text",
                1,
                &payload,
                resources,
            )?;
        }
    }
    finish_with_source(builder, root, whole, resources)
}

/// Finishes a source-bearing accounted build through the finalizer's
/// source-attachment poll, the only path that makes retained source spans
/// resolvable. Mirrors the codec pattern (`begin_finish` + `poll_with_source`).
fn finish_with_source(
    builder: AccountedDocumentBuilder<'static>,
    root: jqf_data::NodeId,
    source: ResolvedSource<'static>,
    resources: &'static mut ResourceContext<'static>,
) -> Result<Document<'static>, DataError> {
    let mut finalizer = builder.begin_finish(root, resources)?;
    let document = loop {
        // SAFETY: `plan.source` is retained immutably by the caller's `RichPlan`
        // for the whole call, and is the exact segment the binding was taken over.
        match unsafe { finalizer.poll_with_source(source, resources) }? {
            jqf_data::DocumentFinalizationPoll::Pending => {
                resources
                    .try_begin_next_cooperative_entry(4_096)
                    .map_err(DataError::from)?;
            }
            jqf_data::DocumentFinalizationPoll::Ready(document) => break document,
        }
    };
    // SAFETY: the same immutable authority is retained by the caller across
    // this call; the codec session pattern installs the source after Ready.
    // SAFETY: the same immutable authority is retained by the caller across
    // this call; the codec session pattern installs the source after Ready.
    unsafe { document.with_borrowed_source_from_bound_authority(source, resources) }
}

#[allow(
    clippy::too_many_lines,
    reason = "one fixture builder keeps the rich/minimal twin auditable side by side"
)]
pub(crate) fn build_same_semantics_document(plan: &RichPlan, rich: bool) -> Result<Document<'static>, DataError> {
    let resources = fixture_resources()?;
    // See build_rich_document: the retained source must outlive every call site.
    let source_bytes: &'static [u8] = Box::leak(plan.source.to_vec().into_boxed_slice());
    let whole = ResolvedSource::new(plan.source_ref, plan.source_ref.kind().as_str(), source_bytes, 0);
    // The rich build retains every optional side-data family; the minimal build
    // demands only mandatory semantics, so its topology and fact arenas are
    // neither reserved nor populated and its coverage omits them.
    let coverage = if rich {
        BuilderCoverage::complete()
    } else {
        BuilderCoverage::minimal_semantic()
    };
    let mut builder = AccountedDocumentBuilder::try_new_with_coverage("bench", None, coverage)?;
    let binding = if rich {
        let binding = DocumentSourceBinding::from_resolved(whole)?;
        builder.bind_source(binding)?;
        Some(binding)
    } else {
        None
    };
    builder.try_reserve(
        DocumentCapacity {
            nodes: plan.width().saturating_add(1),
            occurrences: plan.width(),
            facts: usize::from(rich).saturating_mul(plan.width().div_ceil(4)),
            ..DocumentCapacity::default()
        },
        resources,
    )?;
    let root = builder.add_node(
        "bench.object",
        AccountedSemanticNode::Object {
            member_role: "bench.member",
        },
        None,
        resources,
    )?;
    for (index, &(key_span, value_span)) in plan.fields.iter().enumerate() {
        let key = core::str::from_utf8(&plan.source[key_span.start() as usize..key_span.end() as usize])
            .map_err(|_| DataError::InvalidDocument)?;
        let value = core::str::from_utf8(&plan.source[value_span.start() as usize..value_span.end() as usize])
            .map_err(|_| DataError::InvalidDocument)?;
        let (semantic, occurrence_key) = if rich {
            let binding = binding.expect("rich build binds the source");
            (
                AccountedSemanticNode::SourceString(
                    // SAFETY: as above — `plan.source` is the binding's own
                    // immutable authority, retained for the whole build.
                    unsafe { binding.text_from_bound_authority(whole, value_span) }
                        .map_err(|_| DataError::InvalidDocument)?,
                ),
                AccountedOccurrenceKey::SourceText(
                    unsafe { binding.text_from_bound_authority(whole, key_span) }
                        .map_err(|_| DataError::InvalidDocument)?,
                ),
            )
        } else {
            (AccountedSemanticNode::String(value), AccountedOccurrenceKey::Text(key))
        };
        let node = builder.add_node("bench.string", semantic, None, resources)?;
        let occurrence = builder.add_occurrence(
            LocalOwnerRef::Node(root),
            "bench.member",
            Some(occurrence_key),
            node,
            resources,
        )?;
        if rich && index % 4 == 0 {
            let payload = FactPayload::Bool(index & 8 == 0);
            builder.add_fact(
                LocalOwnerRef::Occurrence(occurrence),
                "bench.comment",
                "bench.text",
                1,
                &payload,
                resources,
            )?;
        }
    }
    if rich {
        finish_with_source(builder, root, whole, resources)
    } else {
        builder.finish(root, resources)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DocumentSummary {
    pub(crate) nodes: usize,
    pub(crate) occurrences: usize,
    pub(crate) facts: usize,
    pub(crate) provenance: usize,
    pub(crate) tags: usize,
    pub(crate) text: DocumentTextStorageStats,
    pub(crate) semantic_checksum: u64,
}

pub(crate) fn summarize(
    document: &Document<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<DocumentSummary, DataError> {
    let value = document.materialize_root(resources)?;
    let coverage = document.coverage();
    // Optional side-data reads are gated by retained coverage; a family that was
    // not retained cannot occur, so it summarizes as zero.
    let occurrences = if coverage.contains(DocumentCapability::Topology) {
        document.occurrence_count()?
    } else {
        document.semantic_relationship_count()
    };
    let facts = if coverage.contains(DocumentCapability::AttachedFacts) {
        document.fact_count()?
    } else {
        0
    };
    // The provenance records were removed (F3); the summary field stays at
    // zero so the bench receipt details keep their pinned shape.
    let provenance = 0;
    Ok(DocumentSummary {
        nodes: document.node_count(),
        occurrences,
        facts,
        provenance,
        tags: count_tags(&value),
        text: document.text_storage_stats()?,
        semantic_checksum: checksum::value(&value),
    })
}

pub(crate) fn count_tags(root: &Value) -> usize {
    let mut count = 0;
    let mut values = vec![root];
    while let Some(value) = values.pop() {
        match value {
            Value::Tagged { payload, .. } => {
                count += 1;
                values.push(payload);
            }
            Value::Array(array) => values.extend(array.iter()),
            Value::Object(object) => values.extend(object.iter().map(jqf_data::ObjectEntry::value)),
            Value::Null
            | Value::Bool(_)
            | Value::Number(_)
            | Value::String(_)
            | Value::Bytes(_)
            | Value::LocalDate(_)
            | Value::LocalTime(_)
            | Value::LocalDateTime(_)
            | Value::OffsetDateTime(_) => {}
        }
    }
    count
}

pub(crate) fn expected_plain_summary(width: usize, checksum: u64) -> DocumentSummary {
    DocumentSummary {
        nodes: width + 1,
        occurrences: width,
        facts: 0,
        provenance: 0,
        tags: 0,
        text: DocumentTextStorageStats::default(),
        semantic_checksum: checksum,
    }
}

pub(crate) fn expected_rich_summary(plan: &RichPlan, checksum: u64) -> DocumentSummary {
    DocumentSummary {
        nodes: plan.width() + 1,
        occurrences: plan.width(),
        facts: plan.width().div_ceil(4),
        provenance: 0,
        tags: 1 + plan.width().div_ceil(8),
        text: DocumentTextStorageStats {
            source_string_values: plan.width(),
            source_keys: plan.width(),
            // The rich fixture now finalizes through the codec session
            // (source-attached finalizer), which sets this flag (plan 108).
            trusted_session_source_attachment: true,
            ..DocumentTextStorageStats::default()
        },
        semantic_checksum: checksum,
    }
}

pub(crate) fn summary_detail(summary: DocumentSummary) -> String {
    format!(
        "nodes={} occurrences={} facts={} provenance={} tags={} source_string_values={} source_keys={} stored_string_values={} stored_keys={} stored_integer_refs={} stored_decimal_coefficient_refs={} decoded_arena_len={} decoded_arena_capacity={} semantic_checksum=0x{:016x}",
        summary.nodes,
        summary.occurrences,
        summary.facts,
        summary.provenance,
        summary.tags,
        summary.text.source_string_values,
        summary.text.source_keys,
        summary.text.stored_string_values,
        summary.text.stored_keys,
        summary.text.stored_integer_refs,
        summary.text.stored_decimal_coefficient_refs,
        summary.text.decoded_arena_len,
        summary.text.decoded_arena_capacity,
        summary.semantic_checksum,
    )
}

pub(crate) fn fact_checksum(fact: &DocumentFact) -> u64 {
    let mut checksum = checksum::u64(checksum::OFFSET, fact.id().get());
    checksum = match fact.owner() {
        LocalOwnerRef::DocumentRoot => checksum::byte(checksum, 0),
        LocalOwnerRef::Node(node) => checksum::u64(checksum::byte(checksum, 1), node.get()),
        LocalOwnerRef::Occurrence(occurrence) => checksum::u64(checksum::byte(checksum, 2), occurrence.get()),
    };
    checksum = checksum::str(checksum, fact.role().as_str());
    checksum = checksum::str(checksum, fact.kind().as_str());
    checksum = checksum::u64(checksum, u64::from(fact.schema_version()));
    checksum::fact_payload(checksum, fact.payload())
}

#[cfg(test)]
mod tests {
    use super::{
        DEEP_JSON, ESCAPE_HEAVY_JSON, NESTED_JSON, WIDE_DUPLICATE_50_JSON, WIDE_DUPLICATE_90_JSON, deep_json,
        escape_heavy_json, nested_json, verify_source_fixture, wide_duplicate_json,
    };

    #[test]
    fn batch2_source_fixture_identities_are_frozen() {
        verify_source_fixture(NESTED_JSON, &nested_json());
        verify_source_fixture(ESCAPE_HEAVY_JSON, &escape_heavy_json());
        verify_source_fixture(WIDE_DUPLICATE_50_JSON, &wide_duplicate_json(2));
        verify_source_fixture(WIDE_DUPLICATE_90_JSON, &wide_duplicate_json(10));
        verify_source_fixture(DEEP_JSON, &deep_json(256));
    }
}
