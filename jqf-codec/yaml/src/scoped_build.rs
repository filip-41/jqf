//! Subtree document construction for the scoped route.
//!
//! Builds a fresh demand-scoped document from a located graph subtree instead of the whole document: aliases resolve to
//! their shared node, and the memo keeps sharing intact.
//!
//! The builders are the same `AccountedDocumentBuilder` the whole-document route uses, with the same schema recipe, so
//! the products carry the same format identity.

use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::vec::Vec;

use jqf_codec_core::{CodecError, CodecFailureKind};
use jqf_data::{
    AccountedDocumentBuilder, AccountedIntrinsicTag, AccountedOccurrenceKey, AccountedSemanticNode, BuilderCoverage,
    DocumentSchemaRecipe, LocalOwnerRef, NodeId, ValueKind,
};
use jqf_resource::ResourceContext;
use jqf_source::ResolvedSource;

use crate::document::{
    CommentIndex, ITEM_ROLE, MAP_KIND, MEMBER_ROLE, SCALAR_KIND, SEQ_KIND, collection_intrinsic, map_data,
};
use crate::graph::{NodeId as GraphNode, YamlGraph, YamlNode};
use crate::provider::DialectKind;
use crate::schema::{self, ScalarCategory, TAG_STR};

/// Builds a fresh document from one located graph subtree.
pub(crate) fn build_subtree_document(
    graph: &YamlGraph,
    root: GraphNode,
    source: ResolvedSource<'_>,
    dialect: DialectKind,
    source_mapped: bool,
    resources: &mut ResourceContext<'_>,
) -> Result<(AccountedDocumentBuilder<'static>, NodeId), CodecError> {
    let mut builder = fresh_subtree_builder(resources)?;
    let mut memo: Vec<Option<NodeId>> = Vec::new();
    let mut in_progress: Vec<GraphNode> = Vec::new();
    let built = build_subtree(
        &mut builder,
        graph,
        root,
        source,
        dialect,
        &mut memo,
        &mut in_progress,
        resources,
    )?;
    // A scoped build attaches comments once per located subtree; the index is graph-derived, so only comment-free
    // graphs skip it.
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
    Ok((builder, built))
}

/// Builds the null product document (a single null scalar root).
pub(crate) fn build_null_document(
    resources: &mut ResourceContext<'_>,
) -> Result<(AccountedDocumentBuilder<'static>, NodeId), CodecError> {
    let mut builder = fresh_subtree_builder(resources)?;
    let root = builder
        .add_node(SCALAR_KIND, AccountedSemanticNode::Null, None, resources)
        .map_err(map_data)?;
    Ok((builder, root))
}

fn fresh_subtree_builder(_resources: &ResourceContext<'_>) -> Result<AccountedDocumentBuilder<'static>, CodecError> {
    let recipe = DocumentSchemaRecipe::try_new(
        "yaml",
        Some("yaml"),
        &[SCALAR_KIND, SEQ_KIND, MAP_KIND],
        &[ITEM_ROLE, MEMBER_ROLE],
        &[],
        &[],
    )
    .map_err(map_data)?;
    AccountedDocumentBuilder::try_new_with_coverage(
        recipe.format(),
        recipe.dialect(),
        // Attached facts are demanded ONLY for the comment projection, shared with the whole-document route so
        // `.key.@comment` serves on the scoped route too.
        BuilderCoverage::minimal_semantic().with_attached_facts(true),
    )
    .map_err(map_data)
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
    resources: &mut ResourceContext<'_>,
) -> Result<NodeId, CodecError> {
    if let Some(id) = memo.get(node.index()).copied().flatten() {
        return Ok(id);
    }
    if in_progress.contains(&node) {
        return Err(CodecError::new(CodecFailureKind::UnsupportedRepresentation));
    }
    in_progress.push(node);
    let result = build_subtree_inner(builder, graph, node, source, dialect, memo, in_progress, resources);
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
    resources: &mut ResourceContext<'_>,
) -> Result<NodeId, CodecError> {
    let yaml_node = graph.node_opt(node, source).ok_or_else(|| {
        CodecError::new(CodecFailureKind::InternalContractViolation {
            contract: "YAML subtree walk over a missing node",
        })
    })?;
    match yaml_node {
        YamlNode::Alias(target) => build_subtree(builder, graph, target, source, dialect, memo, in_progress, resources),
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
            builder
                .add_node(SCALAR_KIND, semantic, intrinsic, resources)
                .map_err(map_data)
        }
        YamlNode::Sequence { items, tag, .. } => {
            let intrinsic = tag.map(collection_intrinsic);
            let id = builder
                .add_node(
                    SEQ_KIND,
                    AccountedSemanticNode::Array { item_role: ITEM_ROLE },
                    intrinsic,
                    resources,
                )
                .map_err(map_data)?;
            let items: Vec<GraphNode> = items.to_vec();
            for item in items {
                let child = build_subtree(builder, graph, item, source, dialect, memo, in_progress, resources)?;
                builder
                    .add_occurrence(LocalOwnerRef::Node(id), ITEM_ROLE, None, child, resources)
                    .map_err(map_data)?;
            }
            Ok(id)
        }
        YamlNode::Mapping { entries, tag, .. } => {
            let intrinsic = tag.map(collection_intrinsic);
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
                let value = build_subtree(
                    builder,
                    graph,
                    value_node,
                    source,
                    dialect,
                    memo,
                    in_progress,
                    resources,
                )?;
                builder
                    .add_occurrence(
                        LocalOwnerRef::Node(id),
                        MEMBER_ROLE,
                        Some(AccountedOccurrenceKey::Text(&key_text)),
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
fn key_text_of(
    graph: &YamlGraph,
    key: GraphNode,
    source: ResolvedSource<'_>,
    dialect: DialectKind,
) -> Result<Option<String>, CodecError> {
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
            Ok(Some(text.to_owned()))
        }
        _ => Ok(None),
    }
}

/// Builds a fresh document whose root sequence holds the given elements — the SLICE-materialization law for a
/// `Located::Range`.
pub(crate) fn build_range_document(
    graph: &YamlGraph,
    elements: &[GraphNode],
    source: ResolvedSource<'_>,
    dialect: DialectKind,
    source_mapped: bool,
    resources: &mut ResourceContext<'_>,
) -> Result<(AccountedDocumentBuilder<'static>, NodeId), CodecError> {
    let mut builder = fresh_subtree_builder(resources)?;
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
    for element in elements {
        let child = build_subtree(
            &mut builder,
            graph,
            *element,
            source,
            dialect,
            &mut memo,
            &mut in_progress,
            resources,
        )?;
        builder
            .add_occurrence(LocalOwnerRef::Node(root), ITEM_ROLE, None, child, resources)
            .map_err(map_data)?;
    }
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
