//! Subtree document construction for the scoped route.
//!
//! Builds a fresh demand-scoped document from a located graph subtree instead of the whole document. Aliases and
//! merge-spliced nodes build whole on first reach so the memo never under-delivers a later kept-whole path. Exact
//! prune omits unread members of the located subtree only; the graph is already fully parsed. Coverage gating and
//! kind-only documents follow the same laws as the whole-document walker.
//!
//! The builders are the same `AccountedDocumentBuilder` the whole-document route uses, with the same schema recipe, so
//! the products carry the same format identity.

use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::vec::Vec;

use jqf_codec_core::{CodecError, CodecFailureKind};
use jqf_data::{
    AccountedDocumentBuilder, AccountedIntrinsicTag, AccountedOccurrenceKey, AccountedSemanticNode, BuilderCoverage,
    DocumentCapacity, DocumentSchemaRecipe, LocalOwnerRef, NodeId, ValueKind,
};
use jqf_resource::ResourceContext;
use jqf_source::ResolvedSource;

use crate::document::{
    CommentIndex, ITEM_ROLE, MAP_KIND, MEMBER_ROLE, PRUNE_ALL, PruneLookup, PruneRef, SCALAR_KIND, SEQ_KIND,
    collection_intrinsic, demanded_intrinsic, map_data,
};
use crate::graph::{NodeId as GraphNode, YamlGraph, YamlNode};
use crate::provider::DialectKind;
use crate::schema::{self, ScalarCategory, TAG_STR};

/// Builds a fresh document from one located graph subtree.
#[allow(
    clippy::too_many_arguments,
    reason = "the subtree walk takes the graph, source, coverage, tag-skip, and prune together"
)]
pub(crate) fn build_subtree_document(
    graph: &YamlGraph,
    root: GraphNode,
    source: ResolvedSource<'_>,
    dialect: DialectKind,
    source_mapped: bool,
    coverage: BuilderCoverage,
    want_tags: bool,
    prune: Option<&PruneLookup>,
    resources: &mut ResourceContext<'_>,
) -> Result<(AccountedDocumentBuilder<'static>, NodeId), CodecError> {
    let mut builder = fresh_subtree_builder(coverage, resources)?;
    let _ = builder.try_reserve(
        DocumentCapacity {
            nodes: graph.len(),
            occurrences: graph.occurrence_count(),
            facts: if coverage.attached_facts() {
                graph.comments().len().saturating_add(graph.merge_hosts().len())
            } else {
                0
            },
            ..DocumentCapacity::default()
        },
        resources,
    );
    let mut memo: Vec<Option<NodeId>> = Vec::new();
    let mut in_progress: Vec<GraphNode> = Vec::new();
    let alias_shared = alias_shared_marks(graph, prune.is_some());
    let built = build_subtree(
        &mut builder,
        graph,
        root,
        source,
        dialect,
        &mut memo,
        &mut in_progress,
        want_tags,
        prune,
        prune.map_or(PRUNE_ALL, |_| jqf_codec_core::PruneTree::ROOT),
        &alias_shared,
        resources,
    )?;
    // A scoped build attaches comments once per located subtree; the index is graph-derived, so only comment-free
    // graphs skip it.
    if coverage.attached_facts() {
        let comment_index = (!graph.comments().is_empty()).then(|| CommentIndex::from_graph(graph));
        crate::document::attach_comment_facts(
            &mut builder,
            comment_index.as_ref(),
            &memo,
            source,
            source_mapped,
            resources,
        )?;
        crate::document::attach_anchor_facts(&mut builder, graph, &memo, source, resources)?;
        crate::document::attach_style_facts(&mut builder, graph, &memo, source, resources)?;
    }
    Ok((builder, built))
}

/// Builds the null product document (a single null scalar root).
pub(crate) fn build_null_document(
    resources: &mut ResourceContext<'_>,
) -> Result<(AccountedDocumentBuilder<'static>, NodeId), CodecError> {
    let mut builder = fresh_subtree_builder(BuilderCoverage::minimal_semantic(), resources)?;
    let root = builder
        .add_node(SCALAR_KIND, AccountedSemanticNode::Null, None, resources)
        .map_err(map_data)?;
    Ok((builder, root))
}

/// Kind-only located document: empty sequence/mapping, or a dummy scalar of the resolved tag/category. Children are
/// not built. The graph was already fully parsed.
pub(crate) fn build_kind_only_document(
    graph: &YamlGraph,
    root: GraphNode,
    source: ResolvedSource<'_>,
    dialect: DialectKind,
    coverage: BuilderCoverage,
    want_tags: bool,
    resources: &mut ResourceContext<'_>,
) -> Result<(AccountedDocumentBuilder<'static>, NodeId), CodecError> {
    let mut builder = fresh_subtree_builder(coverage, resources)?;
    let built = kind_only_node(&mut builder, graph, root, source, dialect, want_tags, 0, resources)?;
    Ok((builder, built))
}

/// Empty array used when `PATH | type` locates a range.
pub(crate) fn build_empty_array_document(
    coverage: BuilderCoverage,
    resources: &mut ResourceContext<'_>,
) -> Result<(AccountedDocumentBuilder<'static>, NodeId), CodecError> {
    let mut builder = fresh_subtree_builder(coverage, resources)?;
    let root = builder
        .add_node(
            SEQ_KIND,
            AccountedSemanticNode::Array { item_role: ITEM_ROLE },
            None,
            resources,
        )
        .map_err(map_data)?;
    Ok((builder, root))
}

#[allow(clippy::too_many_arguments)]
fn kind_only_node(
    builder: &mut AccountedDocumentBuilder<'static>,
    graph: &YamlGraph,
    node: GraphNode,
    source: ResolvedSource<'_>,
    dialect: DialectKind,
    want_tags: bool,
    hops: usize,
    resources: &mut ResourceContext<'_>,
) -> Result<NodeId, CodecError> {
    if hops > graph.len() {
        return Err(CodecError::new(CodecFailureKind::UnsupportedRepresentation));
    }
    let yaml_node = graph.node_opt(node, source).ok_or_else(|| {
        CodecError::new(CodecFailureKind::InternalContractViolation {
            contract: "YAML kind-only walk over a missing node",
        })
    })?;
    match yaml_node {
        YamlNode::Alias(target) => {
            kind_only_node(builder, graph, target, source, dialect, want_tags, hops + 1, resources)
        }
        YamlNode::Sequence { tag, .. } => {
            let intrinsic = demanded_intrinsic(want_tags, tag.map(collection_intrinsic));
            builder
                .add_node(
                    SEQ_KIND,
                    AccountedSemanticNode::Array { item_role: ITEM_ROLE },
                    intrinsic,
                    resources,
                )
                .map_err(map_data)
        }
        YamlNode::Mapping { tag, .. } => {
            let intrinsic = demanded_intrinsic(want_tags, tag.map(collection_intrinsic));
            builder
                .add_node(
                    MAP_KIND,
                    AccountedSemanticNode::Object {
                        member_role: MEMBER_ROLE,
                    },
                    intrinsic,
                    resources,
                )
                .map_err(map_data)
        }
        YamlNode::Scalar { .. } => {
            let resolved = schema::resolve_scalar(graph, node, dialect, source)?;
            let semantic = match &resolved {
                schema::ResolvedScalar::Core { category, .. } => dummy_scalar(*category),
                schema::ResolvedScalar::Tagged { payload, .. } => dummy_scalar(*payload),
            };
            let intrinsic = match &resolved {
                schema::ResolvedScalar::Core { category, tag } => demanded_intrinsic(
                    want_tags,
                    Some(AccountedIntrinsicTag::Core {
                        tag,
                        kind: ValueKind::from(*category),
                    }),
                ),
                schema::ResolvedScalar::Tagged { tag, .. } => {
                    demanded_intrinsic(want_tags, Some(AccountedIntrinsicTag::Tagged(tag)))
                }
            };
            builder
                .add_node(SCALAR_KIND, semantic, intrinsic, resources)
                .map_err(map_data)
        }
    }
}

fn dummy_scalar(category: ScalarCategory) -> AccountedSemanticNode<'static> {
    match category {
        ScalarCategory::String => AccountedSemanticNode::String(""),
        ScalarCategory::Null => AccountedSemanticNode::Null,
        ScalarCategory::Bool(value) => AccountedSemanticNode::Bool(value),
        ScalarCategory::Integer => AccountedSemanticNode::Integer("0"),
        ScalarCategory::Float => AccountedSemanticNode::Float(jqf_data::Float::new(0.0)),
    }
}

fn fresh_subtree_builder(
    coverage: BuilderCoverage,
    resources: &ResourceContext<'_>,
) -> Result<AccountedDocumentBuilder<'static>, CodecError> {
    let recipe = DocumentSchemaRecipe::try_new(
        "yaml",
        Some("yaml"),
        &[SCALAR_KIND, SEQ_KIND, MAP_KIND],
        &[ITEM_ROLE, MEMBER_ROLE],
        &[],
        &[],
    )
    .map_err(map_data)?;
    let mut builder = AccountedDocumentBuilder::try_new_with_coverage(recipe.format(), recipe.dialect(), coverage)
        .map_err(map_data)?;
    let _ = builder.try_reserve(
        DocumentCapacity {
            nodes: 1,
            ..DocumentCapacity::default()
        },
        resources,
    );
    Ok(builder)
}

fn alias_shared_marks(graph: &YamlGraph, pruning: bool) -> Vec<bool> {
    if !pruning {
        return Vec::new();
    }
    let mut alias_shared = alloc::vec![false; graph.len()];
    for target in graph.alias_targets() {
        alias_shared[target.index()] = true;
    }
    for (value, _) in graph.merge_hosts() {
        alias_shared[value.index()] = true;
    }
    alias_shared
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_subtree(
    builder: &mut AccountedDocumentBuilder<'static>,
    graph: &YamlGraph,
    node: GraphNode,
    source: ResolvedSource<'_>,
    dialect: DialectKind,
    memo: &mut Vec<Option<NodeId>>,
    in_progress: &mut Vec<GraphNode>,
    want_tags: bool,
    prune: Option<&PruneLookup>,
    prune_id: u32,
    alias_shared: &[bool],
    resources: &mut ResourceContext<'_>,
) -> Result<NodeId, CodecError> {
    if let Some(id) = memo.get(node.index()).copied().flatten() {
        return Ok(id);
    }
    if in_progress.contains(&node) {
        return Err(CodecError::new(CodecFailureKind::UnsupportedRepresentation));
    }
    in_progress.push(node);
    let prune_id = if alias_shared.get(node.index()).copied().unwrap_or(false) {
        PRUNE_ALL
    } else {
        prune_id
    };
    let result = build_subtree_inner(
        builder,
        graph,
        node,
        source,
        dialect,
        memo,
        in_progress,
        want_tags,
        prune,
        prune_id,
        alias_shared,
        resources,
    );
    in_progress.pop();
    let id = result?;
    let index = node.index();
    while memo.len() <= index {
        memo.push(None);
    }
    memo[index] = Some(id);
    Ok(id)
}

#[allow(clippy::too_many_arguments)]
fn build_subtree_inner(
    builder: &mut AccountedDocumentBuilder<'static>,
    graph: &YamlGraph,
    node: GraphNode,
    source: ResolvedSource<'_>,
    dialect: DialectKind,
    memo: &mut Vec<Option<NodeId>>,
    in_progress: &mut Vec<GraphNode>,
    want_tags: bool,
    prune: Option<&PruneLookup>,
    prune_id: u32,
    alias_shared: &[bool],
    resources: &mut ResourceContext<'_>,
) -> Result<NodeId, CodecError> {
    let yaml_node = graph.node_opt(node, source).ok_or_else(|| {
        CodecError::new(CodecFailureKind::InternalContractViolation {
            contract: "YAML subtree walk over a missing node",
        })
    })?;
    match yaml_node {
        YamlNode::Alias(target) => build_subtree(
            builder,
            graph,
            target,
            source,
            dialect,
            memo,
            in_progress,
            want_tags,
            prune,
            prune_id,
            alias_shared,
            resources,
        ),
        YamlNode::Scalar { .. } => {
            let resolved = schema::resolve_scalar(graph, node, dialect, source)?;
            let text = scalar_text(graph, node, source).to_owned();
            // Lazy: only the RESOLVED category parses its payload ("Ada" is a string and must not fail a float parse).
            let number: Option<crate::document::ScalarNumber> = match &resolved {
                schema::ResolvedScalar::Core {
                    category: ScalarCategory::Float,
                    ..
                } => crate::document::scalar_number_of(&text),
                _ => None,
            };
            let canonical: Option<String> = match &resolved {
                schema::ResolvedScalar::Core {
                    category: ScalarCategory::Integer,
                    ..
                } => crate::document::canonical_integer_for(&text),
                _ => None,
            };
            // The intrinsic tag text is COPIED by `add_node`; a Core tag is a static constant and a Tagged tag is
            // already owned by the resolution, so borrow instead of allocating a third copy.
            let resolved_tag: Option<&str> = match &resolved {
                schema::ResolvedScalar::Core { tag, .. } => Some(tag),
                schema::ResolvedScalar::Tagged { tag, .. } => Some(tag.as_str()),
            };
            let (semantic, intrinsic) = match resolved {
                schema::ResolvedScalar::Core { category, .. } => {
                    let semantic = match category {
                        ScalarCategory::String => AccountedSemanticNode::String(&text),
                        ScalarCategory::Null => AccountedSemanticNode::Null,
                        ScalarCategory::Bool(value) => AccountedSemanticNode::Bool(value),
                        ScalarCategory::Integer => AccountedSemanticNode::Integer(
                            canonical
                                .as_ref()
                                .ok_or_else(|| {
                                    CodecError::new(CodecFailureKind::InternalContractViolation {
                                        contract: "YAML subtree integer already validated",
                                    })
                                })?
                                .as_str(),
                        ),
                        ScalarCategory::Float => match number.as_ref().ok_or_else(|| {
                            CodecError::new(CodecFailureKind::InternalContractViolation {
                                contract: "YAML subtree float already validated",
                            })
                        })? {
                            crate::document::ScalarNumber::Binary64(float) => AccountedSemanticNode::Float(*float),
                            crate::document::ScalarNumber::Decimal(coefficient, scale) => {
                                AccountedSemanticNode::Decimal {
                                    coefficient,
                                    scale: *scale,
                                }
                            }
                        },
                    };
                    let intrinsic = resolved_tag.map(|tag| AccountedIntrinsicTag::Core {
                        tag,
                        kind: ValueKind::from(category),
                    });
                    (semantic, intrinsic)
                }
                schema::ResolvedScalar::Tagged { payload, .. } => {
                    let semantic = match payload {
                        ScalarCategory::String => AccountedSemanticNode::String(&text),
                        _ => {
                            return Err(CodecError::new(CodecFailureKind::UnsupportedRepresentation));
                        }
                    };
                    let intrinsic = resolved_tag.map(AccountedIntrinsicTag::Tagged);
                    (semantic, intrinsic)
                }
            };
            let intrinsic = demanded_intrinsic(want_tags, intrinsic);
            builder
                .add_node(SCALAR_KIND, semantic, intrinsic, resources)
                .map_err(map_data)
        }
        YamlNode::Sequence { items, tag, .. } => {
            let intrinsic = demanded_intrinsic(want_tags, tag.map(collection_intrinsic));
            let id = builder
                .add_node(
                    SEQ_KIND,
                    AccountedSemanticNode::Array { item_role: ITEM_ROLE },
                    intrinsic,
                    resources,
                )
                .map_err(map_data)?;
            let items: Vec<GraphNode> = items.to_vec();
            let item_prune = PruneRef::root(prune).at(prune_id).element().id();
            for item in items {
                let child = build_subtree(
                    builder,
                    graph,
                    item,
                    source,
                    dialect,
                    memo,
                    in_progress,
                    want_tags,
                    prune,
                    item_prune,
                    alias_shared,
                    resources,
                )?;
                builder
                    .add_occurrence(LocalOwnerRef::Node(id), ITEM_ROLE, None, child, resources)
                    .map_err(map_data)?;
            }
            Ok(id)
        }
        YamlNode::Mapping { entries, tag, .. } => {
            let intrinsic = demanded_intrinsic(want_tags, tag.map(collection_intrinsic));
            let id = builder
                .add_node(
                    MAP_KIND,
                    AccountedSemanticNode::Object {
                        member_role: MEMBER_ROLE,
                    },
                    intrinsic,
                    resources,
                )
                .map_err(map_data)?;
            let entries: Vec<(GraphNode, GraphNode)> = entries.to_vec();
            for (key_node, value_node) in entries {
                let key_text = key_text_of(graph, key_node, source, dialect)?
                    .ok_or_else(|| crate::locate::non_string_key_error(graph, key_node, source))?;
                let Some(value_prune) = PruneRef::root(prune).at(prune_id).member(key_text.as_bytes()) else {
                    continue;
                };
                let value = build_subtree(
                    builder,
                    graph,
                    value_node,
                    source,
                    dialect,
                    memo,
                    in_progress,
                    want_tags,
                    prune,
                    value_prune,
                    alias_shared,
                    resources,
                )?;
                builder
                    .add_occurrence(
                        LocalOwnerRef::Node(id),
                        MEMBER_ROLE,
                        Some(AccountedOccurrenceKey::Text(key_text)),
                        value,
                        resources,
                    )
                    .map_err(map_data)?;
            }
            Ok(id)
        }
    }
}

fn scalar_text<'a>(graph: &'a YamlGraph, node: GraphNode, source: ResolvedSource<'a>) -> &'a str {
    match graph.node(node, source) {
        YamlNode::Scalar { text, .. } => text,
        _ => "",
    }
}

/// The object-key text of a mapping key in a SUBTREE: same law as the whole-document build — a quoted scalar, an
/// explicit `!!str`, the EMPTY plain scalar (`: v` reads as the key ""), or a plain scalar that resolves to String
/// under the schema becomes a key; anything else (a complex or non-core-tagged key) is never coerced. A
/// schema-resolution ERROR propagates instead of collapsing into "not a string": the identical input fails the
/// whole-document build with that same diagnostic.
fn key_text_of<'graph>(
    graph: &'graph YamlGraph,
    key: GraphNode,
    source: ResolvedSource<'graph>,
    dialect: DialectKind,
) -> Result<Option<&'graph str>, CodecError> {
    let node = graph.node(key, source);
    match node {
        YamlNode::Scalar { text, tag, style, .. } => {
            let quoted = style != crate::graph::ScalarStyle::Plain;
            let explicit_str = tag == Some(TAG_STR);
            let empty_key_str = !quoted && !explicit_str && text.is_empty();
            let resolved_str = !quoted
                && !explicit_str
                && matches!(
                    schema::resolve_scalar(graph, key, dialect, source)?,
                    schema::ResolvedScalar::Core {
                        category: ScalarCategory::String,
                        ..
                    }
                );
            if !quoted && !explicit_str && !empty_key_str && !resolved_str {
                return Ok(None);
            }
            Ok(Some(text))
        }
        _ => Ok(None),
    }
}

/// Builds a fresh document whose root sequence holds the given elements — the SLICE-materialization law for a
/// `Located::Range`.
#[allow(
    clippy::too_many_arguments,
    reason = "the range walk takes the graph, source, coverage, tag-skip, and prune together"
)]
pub(crate) fn build_range_document(
    graph: &YamlGraph,
    elements: &[GraphNode],
    source: ResolvedSource<'_>,
    dialect: DialectKind,
    source_mapped: bool,
    coverage: BuilderCoverage,
    want_tags: bool,
    prune: Option<&PruneLookup>,
    resources: &mut ResourceContext<'_>,
) -> Result<(AccountedDocumentBuilder<'static>, NodeId), CodecError> {
    let mut builder = fresh_subtree_builder(coverage, resources)?;
    let _ = builder.try_reserve(
        DocumentCapacity {
            nodes: graph.len().saturating_add(1),
            occurrences: elements.len().saturating_add(graph.occurrence_count()),
            ..DocumentCapacity::default()
        },
        resources,
    );
    let root = builder
        .add_node(
            SEQ_KIND,
            AccountedSemanticNode::Array { item_role: ITEM_ROLE },
            None,
            resources,
        )
        .map_err(map_data)?;
    let mut memo: Vec<Option<NodeId>> = Vec::new();
    let mut in_progress: Vec<GraphNode> = Vec::new();
    let alias_shared = alias_shared_marks(graph, prune.is_some());
    let item_prune = PruneRef::root(prune).element().id();
    for element in elements {
        let child = build_subtree(
            &mut builder,
            graph,
            *element,
            source,
            dialect,
            &mut memo,
            &mut in_progress,
            want_tags,
            prune,
            item_prune,
            &alias_shared,
            resources,
        )?;
        builder
            .add_occurrence(LocalOwnerRef::Node(root), ITEM_ROLE, None, child, resources)
            .map_err(map_data)?;
    }
    if coverage.attached_facts() {
        let comment_index = (!graph.comments().is_empty()).then(|| CommentIndex::from_graph(graph));
        crate::document::attach_comment_facts(
            &mut builder,
            comment_index.as_ref(),
            &memo,
            source,
            source_mapped,
            resources,
        )?;
        crate::document::attach_anchor_facts(&mut builder, graph, &memo, source, resources)?;
        crate::document::attach_style_facts(&mut builder, graph, &memo, source, resources)?;
    }
    Ok((builder, root))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{NodeId as GraphNode, ScalarStyle, TextRef, YamlGraph};
    use jqf_source::{SourceId, SourceKind, SourceRef};

    fn source(bytes: &'static [u8]) -> ResolvedSource<'static> {
        ResolvedSource::new(
            SourceRef::new(SourceId::new(1), SourceKind::Input),
            "test.yaml",
            bytes,
            0,
        )
    }

    fn span() -> jqf_source::Span {
        jqf_source::Span::try_from_usize(0, 1).expect("span")
    }

    fn plain_scalar(graph: &mut YamlGraph, text: &'static str) -> GraphNode {
        let id = graph.store_text(text);
        graph
            .add_scalar(TextRef::Owned(id), ScalarStyle::Plain as u8, None, None, span())
            .expect("node id in range")
    }

    fn mapping(graph: &mut YamlGraph, entries: &[(GraphNode, GraphNode)]) -> GraphNode {
        let map = graph.add_mapping(None, None, span()).expect("node id");
        graph.close_mapping(map, entries).expect("close");
        map
    }

    /// The empty plain mapping key (`: v`) is the empty STRING on the scoped route exactly like the whole-document
    /// floor — never a non-coercible null key.
    #[test]
    fn empty_plain_key_is_a_string_key_like_the_floor() {
        let mut graph = YamlGraph::try_new().expect("graph");
        let key = plain_scalar(&mut graph, "");
        let value = plain_scalar(&mut graph, "v");
        let _map = mapping(&mut graph, &[(key, value)]);
        let text = key_text_of(&graph, key, source(b": v\n"), DialectKind::Core)
            .expect("no schema error")
            .expect("empty plain key is a string key");
        assert_eq!(text, "");
    }

    /// A schema-resolution error propagates from the key walk instead of collapsing into "not a string": under the JSON
    /// dialect a plain scalar matching nothing fails with the SCHEMA error the floor raises, not the generic yaml.key
    /// refusal.
    #[test]
    fn schema_error_propagates_from_key_text_of() {
        let mut graph = YamlGraph::try_new().expect("graph");
        let key = plain_scalar(&mut graph, "hello");
        let value = plain_scalar(&mut graph, "v");
        let _map = mapping(&mut graph, &[(key, value)]);
        let error =
            key_text_of(&graph, key, source(b"hello: v\n"), DialectKind::Json).expect_err("unresolvable JSON scalar");
        assert!(matches!(error.kind(), jqf_codec_core::CodecFailureKind::InvalidInput));
    }
}
