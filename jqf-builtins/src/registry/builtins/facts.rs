//! The jqf JSON-facts projection family (`json_facts/0`).
//!
//! One overload, one law: rebuild the input value so its attached facts are visible in a JSON-shaped projection. Markup
//! elements (any located array carrying an element-name fact) project as xq-style trees: the element name is the key,
//! attributes are `@attr` keys, the concatenated **text-leaf** runs are `#text` (comment and processing-instruction
//! leaves are excluded; comments stay on `@comment`, PI content is dropped), and repeated sibling elements become
//! arrays. Every other fact uses the accessor spelling as the key (`@comment`, `@tag`, `@attrs`, `@name`, `@content`,
//! `&attr`), and a fact-bearing scalar or array is wrapped as an object with a `value` key. Data keys win over fact
//! keys on collision. The projection is a presentation, not a round-trippable encoding: it is lossy by design.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::ops::ControlFlow;

use jqf_codec_core::markup;
use jqf_codec_core::{CodecError, CodecFailureKind};
use jqf_data::{
    Array, DataError, DataErrorClass, DocumentId, LocalOwnerRef, MaterializeWorkspace, NodeId, ObjectBuilder,
    ObjectKey, ScalarView, Value, ValueKind,
};
use jqf_resource::ResourceContext;

use super::id;
use crate::codec_result::EngineResult;
use crate::error::EngineRunError;
use crate::registry::record::{
    BuiltinExample, BuiltinExecution, BuiltinFamilyId, BuiltinFamilyRecord, BuiltinOverloadId, BuiltinOverloadRecord,
    DemandTransfer, Effects, SemanticRevision,
};
use crate::semantics::{accessor_matches_fact, materialize_fact_payload};

/// The `json_facts` family record.
pub const FAMILIES: &[BuiltinFamilyRecord] = &[JSON_FACTS_FAMILY];

/// The `json_facts/0` overload record.
pub const OVERLOADS: &[BuiltinOverloadRecord] = &[JSON_FACTS_OVERLOAD];

/// The facts execution payloads, aligned one-to-one with [`OVERLOADS`].
///
/// Every entry carries its overload id so the const coverage walk in `registry::dispatch` can prove pairwise alignment.
#[derive(Clone, Copy, Debug)]
pub enum FactsPayload {
    /// `json_facts/0` — the attached-facts JSON projection.
    JsonFacts,
}

pub const PAYLOADS: &[(u16, FactsPayload)] = &[(id::JSON_FACTS, FactsPayload::JsonFacts)];

const JSON_FACTS_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::JSON_FACTS_FAMILY_ID),
    canonical_name: "json_facts",
    category: "jqf-extension",
    summary: "Rebuild the input so its attached facts appear in a JSON-shaped projection.",
    detail: "Markup elements project as xq-style trees (element name as key, \
             attributes as `@attr`, text as `#text`, repeated elements as \
             arrays). Other facts use the accessor spelling as keys \
             (`@comment`, `@tag`, `@attrs`, `@name`, `@content`, `&attr`), \
             and a fact-bearing scalar or array is wrapped as an object with a \
             `value` key. Data keys win over fact keys on collision. The \
             projection is lossy and is not a round-trippable encoding.",
};

const JSON_FACTS_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::JSON_FACTS),
    family: BuiltinFamilyId::new(id::JSON_FACTS_FAMILY_ID),
    canonical_name: "json_facts",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    // The projection reads the whole input value and every attached fact, so the conservative whole-document transfer
    // is the only honest one.
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        // Fact-free JSON projects to itself, which is the only shape the strict-JSON harness can feed: the fact-bearing
        // arms (markup trees, `@tag`/`@comment`/`@attrs`) need a located document with attached facts and are covered
        // by the codec-level suites.
        BuiltinExample {
            program: "json_facts",
            input: "{\"a\":1}",
            expected: "{\"a\":1}\n",
        },
        BuiltinExample {
            program: "json_facts",
            input: "[1,2]",
            expected: "[1,2]\n",
        },
    ],
};

/// One attached fact, materialized to an owned value at read time so the projection can be built without holding the
/// fact reader's borrow.
pub(crate) struct Fact {
    owner: NodeId,
    role: String,
    kind: String,
    payload: Value,
}

/// The comment-position projection filter: a position fact that is not a NON-EMPTY list is absent —
/// `@comment_inline`/`@comment_foot` appear only when the node carries that position, never as `[]` (jqft attaches an
/// always-empty foot fact; an empty list would make the projection claim a position the document does not hold).
fn non_empty_comment(payload: Option<&Value>) -> Option<&Value> {
    match payload {
        Some(Value::Array(items)) if !items.is_empty() => payload,
        _ => None,
    }
}

/// One run's cached whole-document fact state behind `json_facts`.
///
/// The attached-fact drain ([`read_facts`]) and the text-leaf topology scan ([`read_text_leaves`]) are O(facts + nodes)
/// per call when rebuilt every time, which made a per-element program such as `[.catalog[] | json_facts] | length`
/// quadratic over a markup document:
/// every element re-drained the WHOLE document's facts to project one subtree. The cache holds the drained state once
/// per document identity and hands every later call over the SAME document an O(log E) owner lookup.
///
/// Soundness: documents are immutable once published, so state read for one [`DocumentId`] never goes stale, and ids
/// are minted from a monotone process-local counter, so no later document can alias an entry. The engine keeps one slot
/// per machine and clears it on reseed — machine-lifetime, exactly like the user-declared index store.
#[derive(Default)]
pub struct JsonFactsCache {
    entry: Option<CachedFacts>,
}

/// The cached drained state for one document.
pub struct CachedFacts {
    key: DocumentId,
    /// Every attached fact in global fact (reader) order.
    facts: Vec<Fact>,
    /// Owner → indexes into `facts`, in fact order (the first-match accessor law reads a group in fact order).
    by_owner: BTreeMap<NodeId, Vec<usize>>,
    text_leaves: BTreeSet<NodeId>,
}

impl JsonFactsCache {
    /// Whether the cached entry was drained from `key`.
    #[must_use]
    pub(crate) fn matches(&self, key: DocumentId) -> bool {
        self.entry.as_ref().is_some_and(|cached| cached.key == key)
    }

    /// Stores one freshly drained state under `key`, replacing any entry for another document. One slot per machine:
    /// alternating documents thrash, they never misanswer. Grouping walks the reader order once — the same one pass
    /// the per-call rebuild paid, now paid once per DOCUMENT.
    pub(crate) fn store(&mut self, key: DocumentId, facts: Vec<Fact>, text_leaves: BTreeSet<NodeId>) {
        let mut by_owner: BTreeMap<NodeId, Vec<usize>> = BTreeMap::new();
        for (index, fact) in facts.iter().enumerate() {
            by_owner.entry(fact.owner).or_default().push(index);
        }
        self.entry = Some(CachedFacts {
            key,
            facts,
            by_owner,
            text_leaves,
        });
    }

    /// Borrows the stored entry.
    #[must_use]
    pub(crate) fn entry(&self) -> Option<&CachedFacts> {
        self.entry.as_ref()
    }

    /// Drops the entry. The machine reseed law: a new element may be a new document, and the slot is cheap to refill.
    pub fn clear(&mut self) {
        self.entry = None;
    }
}

/// Borrowed view over the cached state: the lookup surface the projection walk reads, unchanged from the per-call
/// rebuild it replaced.
struct FactIndex<'cache> {
    cached: &'cache CachedFacts,
}

impl<'cache> FactIndex<'cache> {
    fn new(cached: &'cache CachedFacts) -> Self {
        Self { cached }
    }

    fn for_node(&self, node: NodeId) -> impl Iterator<Item = &'cache Fact> + '_ {
        self.cached
            .by_owner
            .get(&node)
            .into_iter()
            .flatten()
            .copied()
            .map(move |index| &self.cached.facts[index])
    }

    fn is_text_leaf(&self, node: NodeId) -> bool {
        self.cached.text_leaves.contains(&node)
    }
}

/// The `json_facts/0` test helper: project one located or owned input value.
///
/// Uncached spelling: every call drains the document's facts fresh. Production dispatch goes through
/// [`json_facts_cached`] with the engine's run-scoped cache; this wrapper answers exactly as a one-call cache would and
/// exists only for the unit tests.
#[cfg(test)]
pub fn json_facts(input: &EngineResult<'_>, resources: &mut ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let mut cache = JsonFactsCache::default();
    json_facts_cached(input, resources, &mut cache)
}

/// [`json_facts`] with the caller's run-scoped cache: located calls over the same document will reuse the ONE drain
/// instead of re-reading every fact and node per element. A cache miss (first call, or a different [`DocumentId`] than
/// the stored entry) rebuilds and stores; a hit borrows.
/// The engine owns the run-scoped instance; until that consumer lands only the test helper below drives this path.
pub fn json_facts_cached(
    input: &EngineResult<'_>,
    resources: &mut ResourceContext<'_>,
    cache: &mut JsonFactsCache,
) -> Result<Value, EngineRunError> {
    match input {
        EngineResult::Owned(value) => project_owned(value),
        EngineResult::Located(located) => {
            let document = located.product().document();
            let node = document.resolve_node_handle(located.node()).map_err(internal_data)?;
            let key = document.key();
            if !cache.matches(key) {
                let facts = read_facts(document, resources)?;
                let text_leaves = read_text_leaves(document, resources)?;
                cache.store(key, facts, text_leaves);
            }
            let Some(entry) = cache.entry() else {
                return Err(internal("fact cache store"));
            };
            let index = FactIndex::new(entry);
            let mut workspace = MaterializeWorkspace::new();
            project_located(document, node, &index, &mut workspace, resources)
        }
    }
}

/// Reads every attached fact of the document, materialized to owned values.
fn read_facts(
    document: &jqf_data::Document<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<Vec<Fact>, EngineRunError> {
    let mut reader = match document.fact_reader(resources) {
        Ok(reader) => reader,
        Err(error) => match error.class() {
            DataErrorClass::Absent
                if matches!(
                    error,
                    DataError::CapabilityUnavailable {
                        capability: jqf_data::DocumentCapability::AttachedFacts,
                    }
                ) =>
            {
                return Ok(Vec::new());
            }
            _ => return Err(map_fact_data(error, "attached-fact reader over a valid document")),
        },
    };
    let mut facts = Vec::new();
    let mut error = None;
    let _ = reader
        .drain(resources, |fact| {
            let LocalOwnerRef::Node(owner) = fact.owner() else {
                return ControlFlow::Continue(());
            };
            match materialize_fact_payload(fact.payload()) {
                Ok(payload) => {
                    facts.push(Fact {
                        owner,
                        role: String::from(fact.role().as_str()),
                        kind: String::from(fact.kind().as_str()),
                        payload,
                    });
                    ControlFlow::Continue(())
                }
                Err(err) => {
                    error = Some(err);
                    ControlFlow::Break(())
                }
            }
        })
        .map_err(|error| map_fact_data(error, "attached-fact read failed over a valid document"))?;
    if let Some(error) = error {
        return Err(error);
    }
    Ok(facts)
}

/// Schema kinds of every markup TEXT leaf. One topology pass so [`project_markup`] can skip comment/PI string children
/// without opening a reader per leaf.
fn read_text_leaves(
    document: &jqf_data::Document<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<BTreeSet<NodeId>, EngineRunError> {
    let mut reader = match document.topology_reader(resources) {
        Ok(reader) => reader,
        Err(error) => match error.class() {
            DataErrorClass::Absent
                if matches!(
                    error,
                    DataError::CapabilityUnavailable {
                        capability: jqf_data::DocumentCapability::Topology,
                    }
                ) =>
            {
                return Ok(BTreeSet::new());
            }
            _ => return Err(map_fact_data(error, "topology reader over a valid document")),
        },
    };
    let mut text_leaves = BTreeSet::new();
    let _ = reader
        .drain_nodes(resources, |view| {
            if view.kind().as_str() == markup::TEXT_KIND {
                text_leaves.insert(view.id());
            }
            ControlFlow::<()>::Continue(())
        })
        .map_err(internal_data)?;
    Ok(text_leaves)
}

/// The first fact on `node` whose role serves `selector`, by the accessor role law, or `None`.
fn fact_for<'facts>(index: &FactIndex<'facts>, node: NodeId, selector: &str) -> Option<&'facts Value> {
    index
        .for_node(node)
        .find(|fact| accessor_matches_fact(&fact.role, selector))
        .map(|fact| &fact.payload)
}

/// Every markup-attribute fact on `node` as (name, value) pairs: the role is exactly `attribute`, and a map payload
/// carries the recovered name for identities the fact-kind grammar refuses.
fn attribute_map_entries(index: &FactIndex<'_>, node: NodeId) -> Vec<(String, Value)> {
    let mut entries = Vec::new();
    for fact in index.for_node(node) {
        if fact.role != jqf_codec_core::markup::ATTRIBUTE_FACT {
            continue;
        }
        match &fact.payload {
            Value::String(_) => entries.push((fact.kind.clone(), fact.payload.clone())),
            Value::Object(object) => {
                let mut name = None;
                let mut value = None;
                for entry in object {
                    match entry.key() {
                        "name" => {
                            if let Value::String(text) = entry.value() {
                                name = Some(String::from(text.as_str()));
                            }
                        }
                        "value" => {
                            if let Value::String(text) = entry.value() {
                                value = Some(String::from(text.as_str()));
                            }
                        }
                        _ => {}
                    }
                }
                if let (Some(name), Some(value)) = (name, value)
                    && let Ok(payload) = Value::try_string(&value)
                {
                    entries.push((name, payload));
                }
            }
            _ => {}
        }
    }
    entries
}

fn attrs_object_from_attributes(index: &FactIndex<'_>, node: NodeId) -> Option<Value> {
    let entries = attribute_map_entries(index, node);
    if entries.is_empty() {
        return None;
    }
    let mut builder = ObjectBuilder::new();
    for (name, payload) in entries {
        let key = ObjectKey::try_from_str(&name).ok()?;
        builder.try_insert_or_replace(key, payload).ok()?;
    }
    builder.try_finish().ok().map(Value::Object)
}

/// The text of the first `selector` fact on `node`, when its payload is text.
fn text_fact<'facts>(index: &FactIndex<'facts>, node: NodeId, selector: &str) -> Option<&'facts str> {
    match fact_for(index, node, selector) {
        Some(Value::String(text)) => Some(text.as_str()),
        _ => None,
    }
}

/// Projects one located node, recursing through its value tree.
fn project_located(
    document: &jqf_data::Document<'_>,
    node: NodeId,
    index: &FactIndex<'_>,
    workspace: &mut MaterializeWorkspace,
    resources: &mut ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let handle = document.node_handle(node).map_err(internal_data)?;
    let view = document.value_view(handle).map_err(internal_data)?;
    let kind = view.kind().map_err(internal_data)?;
    if kind == ValueKind::Array
        && let Some(name) = text_fact(index, node, "name")
    {
        let inner = project_markup(document, node, index, resources)?;
        return wrap_element(name, inner, resources);
    }

    let tag = view.tag().map_err(internal_data)?.map(jqf_data::TagId::as_str);
    let name = text_fact(index, node, "name");
    let content = text_fact(index, node, "content");
    let attrs = fact_for(index, node, "attrs")
        .cloned()
        .or_else(|| attrs_object_from_attributes(index, node));
    let comment = fact_for(index, node, "comment");
    let comment_inline = non_empty_comment(fact_for(index, node, "comment_inline"));
    let comment_foot = non_empty_comment(fact_for(index, node, "comment_foot"));
    let attributes = attribute_map_entries(index, node);
    let has_facts = tag.is_some()
        || name.is_some()
        || content.is_some()
        || attrs.is_some()
        || comment.is_some()
        || comment_inline.is_some()
        || comment_foot.is_some()
        || !attributes.is_empty();
    if !has_facts {
        return project_located_plain(document, node, index, workspace, resources);
    }

    let mut builder = ObjectBuilder::new();
    if let Some(tag) = tag {
        insert_fact_key(
            &mut builder,
            "@tag",
            Value::try_string(tag).map_err(|_| EngineRunError::allocation_failure())?,
        )?;
    }
    if let Some(name) = name {
        insert_fact_key(
            &mut builder,
            "@name",
            Value::try_string(name).map_err(|_| EngineRunError::allocation_failure())?,
        )?;
    }
    if let Some(attrs) = attrs {
        insert_fact_key(&mut builder, "@attrs", attrs)?;
    }
    if let Some(content) = content {
        insert_fact_key(
            &mut builder,
            "@content",
            Value::try_string(content).map_err(|_| EngineRunError::allocation_failure())?,
        )?;
    }
    for (kind, value) in attributes {
        let key = format!("&{kind}");
        insert_fact_key(&mut builder, &key, value.clone())?;
    }
    if let Some(comment) = comment {
        insert_fact_key(&mut builder, "@comment", comment.clone())?;
    }
    if let Some(comment) = comment_inline {
        insert_fact_key(&mut builder, "@comment_inline", comment.clone())?;
    }
    if let Some(comment) = comment_foot {
        insert_fact_key(&mut builder, "@comment_foot", comment.clone())?;
    }

    if let Some(object) = view.object().map_err(internal_data)? {
        for entry in object.iter() {
            let entry = entry.map_err(internal_data)?;
            let key = ObjectKey::try_from_str(entry.key()).map_err(|_| EngineRunError::allocation_failure())?;
            let value = project_located(document, entry.value().node(), index, workspace, resources)?;
            builder
                .try_insert_or_replace(key, value)
                .map_err(|_| EngineRunError::allocation_failure())?;
        }
    } else {
        let value = materialize_node(document, node, workspace, resources)?;
        let value = project_owned(value.untagged())?;
        insert_fact_key(&mut builder, "value", value)?;
    }
    let object = builder.try_finish().map_err(|_| EngineRunError::allocation_failure())?;
    Ok(Value::Object(object))
}

/// Projects a located node that carries no facts at this level: children are projected recursively and the node's own
/// shape is preserved.
fn project_located_plain(
    document: &jqf_data::Document<'_>,
    node: NodeId,
    index: &FactIndex<'_>,
    workspace: &mut MaterializeWorkspace,
    resources: &mut ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let handle = document.node_handle(node).map_err(internal_data)?;
    let view = document.value_view(handle).map_err(internal_data)?;
    match view.kind().map_err(internal_data)? {
        ValueKind::Object => {
            let mut builder = ObjectBuilder::new();
            let object = view
                .object()
                .map_err(internal_data)?
                .ok_or_else(|| internal("object view missing for an object-kind node"))?;
            for entry in object.iter() {
                let entry = entry.map_err(internal_data)?;
                let key = ObjectKey::try_from_str(entry.key()).map_err(|_| EngineRunError::allocation_failure())?;
                let value = project_located(document, entry.value().node(), index, workspace, resources)?;
                builder
                    .try_insert_last(key, value)
                    .map_err(|_| EngineRunError::allocation_failure())?;
            }
            let object = builder.try_finish().map_err(|_| EngineRunError::allocation_failure())?;
            Ok(Value::Object(object))
        }
        ValueKind::Array => {
            let array = view
                .array()
                .map_err(internal_data)?
                .ok_or_else(|| internal("array view missing for an array-kind node"))?;
            let mut values = Vec::new();
            for array_index in 0..array.len() {
                let Some(child) = array.get(array_index) else {
                    continue;
                };
                values.push(project_located(document, child.node(), index, workspace, resources)?);
            }
            let array = Array::try_from_vec(values).map_err(|_| EngineRunError::allocation_failure())?;
            Ok(Value::Array(array))
        }
        _ => materialize_node(document, node, workspace, resources),
    }
}

/// Projects one markup element as an xq-style tree.
fn project_markup(
    document: &jqf_data::Document<'_>,
    node: NodeId,
    index: &FactIndex<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let comment = fact_for(index, node, "comment");
    let comment_inline = non_empty_comment(fact_for(index, node, "comment_inline"));
    let comment_foot = non_empty_comment(fact_for(index, node, "comment_foot"));
    let attrs = match fact_for(index, node, "attrs") {
        Some(Value::Object(object)) => {
            let mut entries = Vec::new();
            for entry in object {
                entries.push((String::from(entry.key()), entry.value().clone()));
            }
            entries
        }
        _ => attribute_map_entries(index, node),
    };

    let handle = document.node_handle(node).map_err(internal_data)?;
    let view = document.value_view(handle).map_err(internal_data)?;
    let array = view
        .array()
        .map_err(internal_data)?
        .ok_or_else(|| internal("markup element without an array projection"))?;
    // The element's DIRECT text runs, in order — the `#text` projection. The content fact concatenates DESCENDANT
    // text, which would duplicate the text of child elements; direct text is what xq-style output means.
    // Comment and PI leaves are string scalars too, so the child's schema kind (not its ValueKind) decides membership:
    // only kernel `text` joins `#text`. PI content is dropped; comments stay on `@comment`.
    let mut direct_text = String::new();
    let mut children = Vec::new();
    for child_index in 0..array.len() {
        let Some(child) = array.get(child_index) else {
            continue;
        };
        let child_node = child.node();
        if let Some(child_name) = text_fact(index, child_node, "name") {
            let value = project_markup(document, child_node, index, resources)?;
            children.push((String::from(child_name), value));
        } else if index.is_text_leaf(child_node)
            && let Some(ScalarView::String(text)) = child.scalar().map_err(internal_data)?
        {
            direct_text.push_str(text);
        }
    }

    if attrs.is_empty()
        && comment.is_none()
        && comment_inline.is_none()
        && comment_foot.is_none()
        && children.is_empty()
    {
        return if direct_text.is_empty() {
            Ok(Value::Null)
        } else {
            Value::try_string(&direct_text).map_err(|_| EngineRunError::allocation_failure())
        };
    }

    let mut builder = ObjectBuilder::new();
    for (key, value) in attrs {
        let key = format!("@{key}");
        insert_fact_key(&mut builder, &key, value.clone())?;
    }
    if !direct_text.is_empty() {
        insert_fact_key(
            &mut builder,
            "#text",
            Value::try_string(&direct_text).map_err(|_| EngineRunError::allocation_failure())?,
        )?;
    }
    if let Some(comment) = comment {
        insert_fact_key(&mut builder, "@comment", comment.clone())?;
    }
    if let Some(comment) = comment_inline {
        insert_fact_key(&mut builder, "@comment_inline", comment.clone())?;
    }
    if let Some(comment) = comment_foot {
        insert_fact_key(&mut builder, "@comment_foot", comment.clone())?;
    }
    for (child_name, value) in group_children(children, resources)? {
        insert_fact_key(&mut builder, &child_name, value)?;
    }
    let object = builder.try_finish().map_err(|_| EngineRunError::allocation_failure())?;
    Ok(Value::Object(object))
}

/// Groups repeated sibling elements by name, first-occurrence order.
fn group_children(
    children: Vec<(String, Value)>,
    _resources: &ResourceContext<'_>,
) -> Result<Vec<(String, Value)>, EngineRunError> {
    let mut order: Vec<String> = Vec::new();
    let mut groups: Vec<Vec<Value>> = Vec::new();
    for (name, value) in children {
        let position = if let Some(position) = order.iter().position(|candidate| candidate == &name) {
            position
        } else {
            order.push(name.clone());
            groups.push(Vec::new());
            order.len() - 1
        };
        groups[position].push(value);
    }
    let mut out = Vec::new();
    for (name, group) in order.into_iter().zip(groups) {
        let value = if group.len() == 1 {
            group
                .into_iter()
                .next()
                .ok_or_else(|| internal("single-element group came up empty"))?
        } else {
            Value::Array(Array::try_from_vec(group).map_err(|_| EngineRunError::allocation_failure())?)
        };
        out.push((name, value));
    }
    Ok(out)
}

/// Projects one owned value: only tags can survive without a document.
fn project_owned(value: &Value) -> Result<Value, EngineRunError> {
    match value {
        Value::Tagged { tag, payload } => {
            let payload = project_owned(payload)?;
            let mut builder = ObjectBuilder::new();
            insert_fact_key(
                &mut builder,
                "@tag",
                Value::try_string(tag.as_str()).map_err(|_| EngineRunError::allocation_failure())?,
            )?;
            match payload {
                Value::Object(object) => {
                    for entry in &object {
                        let key =
                            ObjectKey::try_from_str(entry.key()).map_err(|_| EngineRunError::allocation_failure())?;
                        builder
                            .try_insert_or_replace(key, entry.value().clone())
                            .map_err(|_| EngineRunError::allocation_failure())?;
                    }
                }
                other => insert_fact_key(&mut builder, "value", other)?,
            }
            let object = builder.try_finish().map_err(|_| EngineRunError::allocation_failure())?;
            Ok(Value::Object(object))
        }
        Value::Array(array) => {
            let mut values = Vec::new();
            for entry in array {
                values.push(project_owned(entry)?);
            }
            let array = Array::try_from_vec(values).map_err(|_| EngineRunError::allocation_failure())?;
            Ok(Value::Array(array))
        }
        Value::Object(object) => {
            let mut builder = ObjectBuilder::new();
            for entry in object {
                let key = ObjectKey::try_from_str(entry.key()).map_err(|_| EngineRunError::allocation_failure())?;
                let value = project_owned(entry.value())?;
                builder
                    .try_insert_last(key, value)
                    .map_err(|_| EngineRunError::allocation_failure())?;
            }
            let object = builder.try_finish().map_err(|_| EngineRunError::allocation_failure())?;
            Ok(Value::Object(object))
        }
        other => Ok(other.clone()),
    }
}

/// Materializes one located node into an owned value.
fn materialize_node(
    document: &jqf_data::Document<'_>,
    node: NodeId,
    workspace: &mut MaterializeWorkspace,
    resources: &mut ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let handle = document.node_handle(node).map_err(internal_data)?;
    document
        .materialize_node_with(workspace, handle, resources)
        .map_err(internal_data)
}

/// Inserts one fact key. Fact keys are prefix-distinct by construction; a duplicate arriving anyway follows the
/// `ObjectBuilder` duplicate law — the later value wins, kept at the first occurrence's position.
fn insert_fact_key(builder: &mut ObjectBuilder, key: &str, value: Value) -> Result<(), EngineRunError> {
    let key = ObjectKey::try_from_str(key).map_err(|_| EngineRunError::allocation_failure())?;
    builder
        .try_insert_last(key, value)
        .map_err(|_| EngineRunError::allocation_failure())
}

/// Wraps one projected element under its name — the root element's own key, since only the parent of a child element
/// knows the name to key it by.
fn wrap_element(name: &str, value: Value, _resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let mut builder = ObjectBuilder::new();
    let key = ObjectKey::try_from_str(name).map_err(|_| EngineRunError::allocation_failure())?;
    builder
        .try_insert_last(key, value)
        .map_err(|_| EngineRunError::allocation_failure())?;
    let object = builder.try_finish().map_err(|_| EngineRunError::allocation_failure())?;
    Ok(Value::Object(object))
}

fn internal(contract: &'static str) -> EngineRunError {
    EngineRunError::Codec(CodecError::new(CodecFailureKind::InternalContractViolation {
        contract,
    }))
}

fn map_fact_data(error: DataError, contract: &'static str) -> EngineRunError {
    match error.class() {
        DataErrorClass::Budget => EngineRunError::allocation_failure(),
        DataErrorClass::Host => EngineRunError::Codec(jqf_codec_core::map_data(error, contract)),
        _ => internal(contract),
    }
}

fn internal_data(error: DataError) -> EngineRunError {
    map_fact_data(error, "facts projection over a valid document failed")
}

#[cfg(test)]
mod tests {
    use super::{JsonFactsCache, json_facts, json_facts_cached};
    use crate::codec_result::EngineResult;
    use crate::semantics::render;
    use alloc::string::String;
    use alloc::vec;
    use jqf_codec_core::{DocumentProduct, LocatedProduct};
    use jqf_data::{
        AccountedDocumentBuilder, AccountedSemanticNode, BuilderCoverage, DocumentSchemaRecipe, FactPayload,
        LocalOwnerRef,
    };
    use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

    static CONTROL: ContinueControl = ContinueControl;

    fn resources() -> ResourceContext<'static> {
        ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                .expect("account"),
            &CONTROL,
            WorkMeter::try_new_v1(4_096).expect("work"),
        )
        .expect("resources")
    }

    /// `<r>before<!-- c -->after<?target data?></r>` as a located document.
    fn mixed_markup() -> (jqf_data::Document<'static>, jqf_data::NodeHandle) {
        let resources = resources();
        let recipe = DocumentSchemaRecipe::try_new(
            "xml",
            Some("xml"),
            &["xml.element@1", "text", "comment", "pi"],
            &["xml.child@1"],
            &["name", "xml.comment@1"],
            &["name", "xml.comment@1"],
        )
        .expect("recipe");
        let mut builder = AccountedDocumentBuilder::try_new_with_coverage(
            recipe.format(),
            recipe.dialect(),
            BuilderCoverage::complete(),
        )
        .expect("builder");
        let text = |builder: &mut AccountedDocumentBuilder<'static>, body: &str, resources: &ResourceContext<'_>| {
            builder
                .add_node("text", AccountedSemanticNode::String(body), None, resources)
                .expect("text")
        };
        let before = text(&mut builder, "before", &resources);
        let comment = builder
            .add_node("comment", AccountedSemanticNode::String(" c "), None, &resources)
            .expect("comment");
        let after = text(&mut builder, "after", &resources);
        let pi = builder
            .add_node("pi", AccountedSemanticNode::String("<?target data?>"), None, &resources)
            .expect("pi");
        let root = builder
            .add_node(
                "xml.element@1",
                AccountedSemanticNode::Array {
                    item_role: "xml.child@1",
                },
                None,
                &resources,
            )
            .expect("element");
        builder
            .add_fact(
                LocalOwnerRef::Node(root),
                "name",
                "name",
                1,
                &FactPayload::Text(String::from("r")),
                &resources,
            )
            .expect("name");
        builder
            .add_fact(
                LocalOwnerRef::Node(root),
                "xml.comment@1",
                "xml.comment@1",
                1,
                &FactPayload::List(vec![FactPayload::Text(String::from(" c "))]),
                &resources,
            )
            .expect("comment fact");
        for child in [before, comment, after, pi] {
            builder
                .add_occurrence(LocalOwnerRef::Node(root), "xml.child@1", None, child, &resources)
                .expect("child");
        }
        let document = builder.finish(root, &resources).expect("finish");
        let handle = document.root_handle();
        (document, handle)
    }

    /// `#text` concatenates only text leaves; the comment stays on `@comment` and the PI body is dropped.
    #[test]
    fn markup_text_excludes_comment_and_pi_leaves() {
        let mut resources = resources();
        let (document, handle) = mixed_markup();
        let product = DocumentProduct::try_new(document, &resources).expect("product");
        let located = LocatedProduct::try_new(&product, handle).expect("located");
        let projected = json_facts(&EngineResult::Located(located), &mut resources).expect("facts");
        let json = render::to_json(&projected).expect("json");
        assert_eq!(json, "{\"r\":{\"#text\":\"beforeafter\",\"@comment\":[\" c \"]}}");
        assert!(
            !json.contains("target"),
            "PI body must not appear in the projection: {json}"
        );
        assert!(
            !json.contains("<?"),
            "PI spelling must not appear in the projection: {json}"
        );
    }

    /// `<r><e><!-- x --></e></r>`: an attribute-less, childless element whose ONLY fact is position-specific must
    /// project that fact, not collapse to null (the early-return guard has to consult the inline and foot runs beside
    /// the plain comment).
    #[test]
    fn an_inline_only_element_projects_its_comment_fact() {
        let mut resources = resources();
        let recipe = DocumentSchemaRecipe::try_new(
            "xml",
            Some("xml"),
            &["xml.element@1", "text", "comment", "pi"],
            &["xml.child@1"],
            &["name", "xml.comment@1"],
            &["name", "xml.comment@1"],
        )
        .expect("recipe");
        let mut builder = AccountedDocumentBuilder::try_new_with_coverage(
            recipe.format(),
            recipe.dialect(),
            BuilderCoverage::complete(),
        )
        .expect("builder");
        let child = builder
            .add_node(
                "xml.element@1",
                AccountedSemanticNode::Array {
                    item_role: "xml.child@1",
                },
                None,
                &resources,
            )
            .expect("element");
        builder
            .add_fact(
                LocalOwnerRef::Node(child),
                "name",
                "name",
                1,
                &FactPayload::Text(String::from("e")),
                &resources,
            )
            .expect("name");
        builder
            .add_fact(
                LocalOwnerRef::Node(child),
                "xml.comment_inline@1",
                "xml.comment_inline@1",
                1,
                &FactPayload::List(vec![FactPayload::Text(String::from(" x "))]),
                &resources,
            )
            .expect("inline fact");
        let root = builder
            .add_node(
                "xml.element@1",
                AccountedSemanticNode::Array {
                    item_role: "xml.child@1",
                },
                None,
                &resources,
            )
            .expect("root");
        builder
            .add_fact(
                LocalOwnerRef::Node(root),
                "name",
                "name",
                1,
                &FactPayload::Text(String::from("r")),
                &resources,
            )
            .expect("root name");
        builder
            .add_occurrence(LocalOwnerRef::Node(root), "xml.child@1", None, child, &resources)
            .expect("child");
        let document = builder.finish(root, &resources).expect("finish");
        let handle = document.root_handle();
        let product = DocumentProduct::try_new(document, &resources).expect("product");
        let located = LocatedProduct::try_new(&product, handle).expect("located");
        let projected = json_facts(&EngineResult::Located(located), &mut resources).expect("facts");
        let json = render::to_json(&projected).expect("json");
        assert_eq!(json, "{\"r\":{\"e\":{\"@comment_inline\":[\" x \"]}}}");
    }

    /// Cache-reuse law: a shared run cache answers every later call over the SAME document exactly as a fresh uncached
    /// rebuild would — the shape a per-element program (`[.catalog[] | json_facts]`) depends on. The second call here
    /// is a cache HIT (same `DocumentId`), so equality with the fresh twin pins the hit path, not just the fill.
    #[test]
    fn cached_calls_match_uncached_over_one_document() {
        let mut resources = resources();
        let (document, handle) = mixed_markup();
        let key = document.key();
        let product = DocumentProduct::try_new(document, &resources).expect("product");
        // The first CHILD of the root element: a different node, same document.
        let child_handle = {
            let doc = product.document();
            let view = doc.value_view(handle).expect("root view");
            let array = view.array().expect("array").expect("children");
            let child_node = array.get(0).expect("child").node();
            doc.node_handle(child_node).expect("child handle")
        };
        let root_located = LocatedProduct::try_new(&product, handle).expect("root located");
        let child_located = LocatedProduct::try_new(&product, child_handle).expect("child located");

        let mut cache = JsonFactsCache::default();
        assert!(!cache.matches(key));
        let cached_root = json_facts_cached(
            &EngineResult::Located(root_located.try_clone().expect("clone")),
            &mut resources,
            &mut cache,
        )
        .expect("cached root");
        assert!(cache.matches(key), "the root call fills the entry");
        let cached_child =
            json_facts_cached(&EngineResult::Located(child_located), &mut resources, &mut cache).expect("cached child");

        let fresh_root = json_facts(&EngineResult::Located(root_located), &mut resources).expect("fresh root");
        let fresh_child = json_facts(
            &EngineResult::Located(LocatedProduct::try_new(&product, child_handle).expect("twin")),
            &mut resources,
        )
        .expect("fresh child");

        assert_eq!(
            render::to_json(&cached_root).expect("json"),
            render::to_json(&fresh_root).expect("json"),
        );
        assert_eq!(
            render::to_json(&cached_child).expect("json"),
            render::to_json(&fresh_child).expect("json"),
        );
    }
}
