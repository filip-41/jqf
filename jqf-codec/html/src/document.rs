//! HTML semantic document construction (§4.10's projection).
//!
//! The recovered WHATWG tree projects into the format-neutral [`jqf_data::Document`] exactly as XML's does: an element
//! is an ARRAY of its recovered children (text runs and child elements; comments are ATTACHED FACTS, never child values
//! — the HTML comment model), its normalized expanded name and recovered semantic attribute map are facts, and the
//! document carries its recovered mode, pragma-set default language, and doctype as document-level facts.
//!
//! Exact prune is after recover, at subtree materialize — see [`build_subtree_document`]. Whole keeps every child.

use alloc::string::String;
use alloc::vec::Vec;

use alloc::vec;
use jqf_codec_core::{CodecError, CodecFailureKind, PRUNE_ALL, PruneLookup, PruneRef, PruneTree};

use jqf_data::{
    AccountedDocumentBuilder, AccountedSemanticNode, BuilderCoverage, DataError, DocumentCapacity,
    DocumentSchemaRecipe, FactPayload, Integer, LocalOwnerRef, NodeId,
};
use jqf_resource::ResourceContext;

use crate::tree::{NodeId as HtmlNodeId, NodeKind, QuirksMode, Tree};

pub(crate) const ELEMENT_KIND: &str = "html.element@1";
pub(crate) const TEXT_KIND: &str = jqf_codec_core::markup::TEXT_KIND;
pub(crate) const COMMENT_KIND: &str = jqf_codec_core::markup::COMMENT_KIND;
pub(crate) const NULL_KIND: &str = "html.null@1";
pub(crate) const CHILD_ROLE: &str = "html.child@1";
pub(crate) const NAME_FACT: &str = jqf_codec_core::markup::NAME_FACT;
pub(crate) const ATTRS_FACT: &str = jqf_codec_core::markup::ATTRS_FACT;
pub(crate) const CONTENT_FACT: &str = jqf_codec_core::markup::CONTENT_FACT;
/// One fact PER ATTRIBUTE, so the `.&name` markup-attribute accessor serves each attribute. The engine's `.&` selector
/// matches exactly this role.
pub(crate) const ATTRIBUTE_FACT: &str = jqf_codec_core::markup::ATTRIBUTE_FACT;
/// Fact kind for an attribute whose recovered name is not a jqf-data identity (ASCII control or whitespace). The
/// payload is `{name, value}` text.
pub(crate) const HTML_ATTR_BYTES_KIND: &str = "html.attr-bytes@1";
/// The comments attached to one element, grouped by recovered role.
pub(crate) const COMMENT_FACT: &str = "html.comment@1";
/// The recovered document mode fact (attached to the document element).
pub(crate) const MODE_FACT: &str = "html.mode@1";
/// The pragma-set default language fact (attached to the document element).
pub(crate) const PRAGMA_LANGUAGE_FACT: &str = "html.pragma-language@1";
/// The doctype fact (attached to the document element).
pub(crate) const DOCTYPE_FACT: &str = "html.doctype@1";

/// The comment roles the build produces (§4.10's leading/inline classification; the source-line refinement that would
/// further split after-child comments is not served, so every after-child comment is inline). Root marks document-edge
/// comments on a synthetic element-less root.
pub(crate) const COMMENT_ROLE_LEADING: &str = "leading";
pub(crate) const COMMENT_ROLE_INLINE: &str = "inline";
pub(crate) const COMMENT_ROLE_ROOT: &str = "root";

pub(crate) fn html_schema_recipe() -> Result<DocumentSchemaRecipe<'static>, DataError> {
    DocumentSchemaRecipe::try_new(
        "html",
        Some("html"),
        &[ELEMENT_KIND, TEXT_KIND, COMMENT_KIND],
        &[CHILD_ROLE],
        &[
            NAME_FACT,
            ATTRS_FACT,
            CONTENT_FACT,
            ATTRIBUTE_FACT,
            HTML_ATTR_BYTES_KIND,
            COMMENT_FACT,
            MODE_FACT,
            PRAGMA_LANGUAGE_FACT,
            DOCTYPE_FACT,
        ],
        &[
            NAME_FACT,
            ATTRS_FACT,
            CONTENT_FACT,
            ATTRIBUTE_FACT,
            COMMENT_FACT,
            MODE_FACT,
            PRAGMA_LANGUAGE_FACT,
            DOCTYPE_FACT,
        ],
    )
}

pub(crate) fn map_data(error: DataError) -> CodecError {
    #[cfg(jqf_trace)]
    std::eprintln!("HTML builder data error: {error:?}");
    jqf_codec_core::map_data(error, "HTML builder rejected document construction")
}

/// Builds the semantic document from the recovered tree. The root is the document element (the `html` element). The
/// session seals and binds the retained source authority cooperatively AFTER the build (the HTML session's Seal phase),
/// so the source-echo encoder can read the sealed source segment.
pub(crate) fn build_document(
    tree: &Tree,
    coverage: BuilderCoverage,
    resources: &mut ResourceContext<'_>,
) -> Result<(AccountedDocumentBuilder<'static>, NodeId), CodecError> {
    // Names, content, and occurrence topology are the HTML value model (the
    // encoder reads them). Attribute and comment facts skip when the demand
    // named neither (checked before attached_facts is forced). NAME_FACT
    // writes need attached_facts; topology follows the requirement.
    let attach_attrs = coverage.attached_facts();
    let mut builder = fresh_builder(coverage.with_attached_facts(true), resources)?;
    // The document element: the html element. An element-less tree (a comment-only or doctype-only document — the
    // "<!--x-->" corpus row) still decodes: the semantic root is a SYNTHETIC empty html element, because the WHATWG
    // parser accepts such documents (the tree has no document element for them) and the document's facts need a home.
    // The recovered comments/doctype stay document-level facts on that root.
    let root = build_document_element(&mut builder, tree, resources, attach_attrs)?;
    attach_document_level_facts(&mut builder, tree, root, resources)?;
    Ok((builder, root))
}

/// Builds the COUNT/TYPE skeleton from a recovered tree: the document element as an array of its
/// direct children. HTML recover cannot skip unread bytes (WHATWG), so the tree is already complete; this
/// path does not project descendant ATTRIBUTE_FACT tables. Each child ELEMENT is an empty array (NAME only)
/// and each text child is a leaf — comments stay facts and are omitted. Element demand uses
/// [`build_document`]: these stubs cannot rematerialize.
pub(crate) fn build_measure_document(
    tree: &Tree,
    resources: &mut ResourceContext<'_>,
) -> Result<(AccountedDocumentBuilder<'static>, NodeId), CodecError> {
    // Dynamic builder: NAME and doctype facts. Recover rearranges the tree, so
    // children are cheap nodes rather than source spans.
    let mut builder = fresh_builder(BuilderCoverage::complete(), resources)?;
    let child_ids: Vec<HtmlNodeId> = match tree.document_element() {
        Some(html) => measure_child_ids(tree, html),
        None => Vec::new(),
    };
    let n_children = if tree.document_element().is_none() {
        2
    } else {
        child_ids.len()
    };
    let _ = builder.try_reserve(
        DocumentCapacity {
            nodes: n_children.saturating_add(1),
            occurrences: n_children,
            facts: n_children.saturating_add(4),
            ..DocumentCapacity::default()
        },
        resources,
    );
    let root_name = tree
        .document_element()
        .map(|html| tree.nodes[html.0].name.as_str())
        .unwrap_or("html");
    let root = add_measure_element(&mut builder, root_name, resources)?;
    if tree.document_element().is_some() {
        for child in child_ids {
            let node = match tree.nodes[child.0].kind {
                NodeKind::Element => add_measure_element(&mut builder, &tree.nodes[child.0].name, resources)?,
                NodeKind::Text => builder
                    .add_node(
                        TEXT_KIND,
                        AccountedSemanticNode::String(&tree.nodes[child.0].data),
                        None,
                        resources,
                    )
                    .map_err(map_data)?,
                NodeKind::Comment | NodeKind::Doctype | NodeKind::Document => {
                    unreachable!("measure_child_ids keeps only Element and Text")
                }
            };
            builder
                .add_occurrence(LocalOwnerRef::Node(root), CHILD_ROLE, None, node, resources)
                .map_err(map_data)?;
        }
    } else {
        for name in ["head", "body"] {
            let child = add_measure_element(&mut builder, name, resources)?;
            builder
                .add_occurrence(LocalOwnerRef::Node(root), CHILD_ROLE, None, child, resources)
                .map_err(map_data)?;
        }
    }
    attach_document_level_facts(&mut builder, tree, root, resources)?;
    Ok((builder, root))
}

fn measure_child_ids(tree: &Tree, parent: HtmlNodeId) -> Vec<HtmlNodeId> {
    tree.nodes[parent.0]
        .children
        .iter()
        .copied()
        .filter(|child| matches!(tree.nodes[child.0].kind, NodeKind::Element | NodeKind::Text))
        .collect()
}

fn add_measure_element(
    builder: &mut AccountedDocumentBuilder<'static>,
    name: &str,
    resources: &mut ResourceContext<'_>,
) -> Result<NodeId, CodecError> {
    let id = builder
        .add_node(
            ELEMENT_KIND,
            AccountedSemanticNode::Array { item_role: CHILD_ROLE },
            None,
            resources,
        )
        .map_err(map_data)?;
    builder
        .add_fact(
            LocalOwnerRef::Node(id),
            NAME_FACT,
            NAME_FACT,
            1,
            &FactPayload::Text(String::from(name)),
            resources,
        )
        .map_err(map_data)?;
    Ok(id)
}

/// Mode, pragma language, and doctype on the document element. Doctype stays for encode preflight even when
/// per-element ATTRIBUTE_FACT tables skip.
fn attach_document_level_facts(
    builder: &mut AccountedDocumentBuilder<'static>,
    tree: &Tree,
    root: NodeId,
    resources: &mut ResourceContext<'_>,
) -> Result<(), CodecError> {
    let mode = match tree.quirks {
        QuirksMode::NoQuirks => "no-quirks",
        QuirksMode::LimitedQuirks => "limited-quirks",
        QuirksMode::Quirks => "quirks",
    };
    builder
        .add_fact(
            LocalOwnerRef::Node(root),
            MODE_FACT,
            MODE_FACT,
            1,
            &FactPayload::Text(String::from(mode)),
            resources,
        )
        .map_err(|error| {
            #[cfg(jqf_trace)]
            std::eprintln!("html document build {error:?}");
            map_data(error)
        })?;
    if let Some(language) = &tree.pragma_language {
        builder
            .add_fact(
                LocalOwnerRef::Node(root),
                PRAGMA_LANGUAGE_FACT,
                PRAGMA_LANGUAGE_FACT,
                1,
                &FactPayload::Text(language.clone()),
                resources,
            )
            .map_err(|error| {
                #[cfg(jqf_trace)]
                std::eprintln!("html document build {error:?}");
                map_data(error)
            })?;
    }
    if let Some(doctype) = tree.nodes[tree.document.0]
        .children
        .iter()
        .find_map(|child| tree.nodes[child.0].doctype.as_ref())
    {
        let mut map = Vec::new();
        map.push((
            String::from("name"),
            FactPayload::Text(doctype.name.clone().unwrap_or_default()),
        ));
        map.push((
            String::from("public"),
            FactPayload::Text(doctype.public_identifier.clone().unwrap_or_default()),
        ));
        map.push((
            String::from("system"),
            FactPayload::Text(doctype.system_identifier.clone().unwrap_or_default()),
        ));
        map.push((String::from("force-quirks"), FactPayload::Bool(doctype.force_quirks)));
        builder
            .add_fact(
                LocalOwnerRef::Node(root),
                DOCTYPE_FACT,
                DOCTYPE_FACT,
                1,
                &FactPayload::Map(map),
                resources,
            )
            .map_err(|error| {
                #[cfg(jqf_trace)]
                std::eprintln!("html document build {error:?}");
                map_data(error)
            })?;
    }
    Ok(())
}

/// Builds the document element (the html element) as the root node.
///
/// When the tree has NO element (a comment-only or doctype-only document), the root is a synthetic empty html element
/// with the empty head and body the parser would have implied — the same shape a re-decode of the serialized document
/// reproduces, which is what makes the round trip hold.
fn build_document_element(
    builder: &mut AccountedDocumentBuilder<'static>,
    tree: &Tree,
    resources: &mut ResourceContext<'_>,
    attach_attrs: bool,
) -> Result<NodeId, CodecError> {
    let Some(html) = tree.document_element() else {
        return build_synthetic_empty_document(builder, tree, resources, attach_attrs);
    };
    let id = build_node(builder, tree, html, resources, attach_attrs, None, PRUNE_ALL).map(|(id, _)| id)?;
    if attach_attrs {
        attach_document_edge_comments(builder, tree, id, resources)?;
    }
    Ok(id)
}

/// Builds the synthetic root for an element-less document: the html element with the implied empty head and body, and
/// the document-level comments as ROOT-role facts on it.
fn build_synthetic_empty_document(
    builder: &mut AccountedDocumentBuilder<'static>,
    tree: &Tree,
    resources: &mut ResourceContext<'_>,
    attach_attrs: bool,
) -> Result<NodeId, CodecError> {
    let html = builder
        .add_node(
            ELEMENT_KIND,
            AccountedSemanticNode::Array { item_role: CHILD_ROLE },
            None,
            resources,
        )
        .map_err(map_data)?;
    builder
        .add_fact(
            LocalOwnerRef::Node(html),
            NAME_FACT,
            NAME_FACT,
            1,
            &FactPayload::Text(String::from("html")),
            resources,
        )
        .map_err(map_data)?;
    for name in ["head", "body"] {
        let child = builder
            .add_node(
                ELEMENT_KIND,
                AccountedSemanticNode::Array { item_role: CHILD_ROLE },
                None,
                resources,
            )
            .map_err(map_data)?;
        builder
            .add_fact(
                LocalOwnerRef::Node(child),
                NAME_FACT,
                NAME_FACT,
                1,
                &FactPayload::Text(String::from(name)),
                resources,
            )
            .map_err(map_data)?;
        builder
            .add_occurrence(LocalOwnerRef::Node(html), CHILD_ROLE, None, child, resources)
            .map_err(map_data)?;
    }
    if attach_attrs {
        attach_document_edge_comments(builder, tree, html, resources)?;
    }
    Ok(html)
}

/// Document-node comments (before `<html>` / after `</html>`) attach as ROOT-role facts on the document element.
fn attach_document_edge_comments(
    builder: &mut AccountedDocumentBuilder<'static>,
    tree: &Tree,
    html: NodeId,
    resources: &mut ResourceContext<'_>,
) -> Result<(), CodecError> {
    let mut comments = Vec::new();
    for (position, child) in tree.nodes[tree.document.0].children.iter().enumerate() {
        if tree.nodes[child.0].kind == NodeKind::Comment {
            comments.push(FactPayload::Map(vec![
                (
                    String::from("text"),
                    FactPayload::Text(tree.nodes[child.0].data.clone()),
                ),
                (
                    String::from("position"),
                    FactPayload::Integer(Integer::from_i64(position as i64)),
                ),
            ]));
        }
    }
    if comments.is_empty() {
        return Ok(());
    }
    builder
        .add_fact(
            LocalOwnerRef::Node(html),
            COMMENT_FACT,
            COMMENT_ROLE_ROOT,
            1,
            &FactPayload::List(comments),
            resources,
        )
        .map_err(map_data)?;
    Ok(())
}

/// Builds one element (an array of its children) with its facts. See [`build_subtree_document`] for the Exact prune
/// law; Whole passes keep-all.
fn build_node(
    builder: &mut AccountedDocumentBuilder<'static>,
    tree: &Tree,
    index: HtmlNodeId,
    resources: &mut ResourceContext<'_>,
    attach_attrs: bool,
    prune: Option<&PruneLookup>,
    prune_id: u32,
) -> Result<(NodeId, String), CodecError> {
    let _depth = resources.enter_nesting_owned().map_err(CodecError::from)?;
    let element = &tree.nodes[index.0];
    let id = builder
        .add_node(
            ELEMENT_KIND,
            AccountedSemanticNode::Array { item_role: CHILD_ROLE },
            None,
            resources,
        )
        .map_err(|error| {
            #[cfg(jqf_trace)]
            std::eprintln!("html document build {error:?}");
            map_data(error)
        })?;
    builder
        .add_fact(
            LocalOwnerRef::Node(id),
            NAME_FACT,
            NAME_FACT,
            1,
            &FactPayload::Text(element.name.clone()),
            resources,
        )
        .map_err(|error| {
            #[cfg(jqf_trace)]
            std::eprintln!("html document build {error:?}");
            map_data(error)
        })?;
    if attach_attrs {
        for attribute in &element.attrs {
            // A WHATWG attribute name may carry ASCII control bytes (a parse error, but valid — the tokenizer keeps them).
            // The per-name `attribute` fact keys on the name as a jqf-data identity, and the identity grammar rejects ASCII
            // control/whitespace and the empty string. A name outside that grammar is still one attribute fact: the kind is
            // a reserved identity and the payload carries the original name plus value, so encode and `.@attrs` keep the
            // recovered pair.
            if is_valid_identity(&attribute.name) {
                builder
                    .add_fact(
                        LocalOwnerRef::Node(id),
                        ATTRIBUTE_FACT,
                        &attribute.name,
                        1,
                        &FactPayload::Text(attribute.value.clone()),
                        resources,
                    )
                    .map_err(map_data)?;
            } else {
                builder
                    .add_fact(
                        LocalOwnerRef::Node(id),
                        ATTRIBUTE_FACT,
                        HTML_ATTR_BYTES_KIND,
                        1,
                        &FactPayload::Map(vec![
                            (String::from("name"), FactPayload::Text(attribute.name.clone())),
                            (String::from("value"), FactPayload::Text(attribute.value.clone())),
                        ]),
                        resources,
                    )
                    .map_err(map_data)?;
            }
        }
        // Children are reached through the ARRAY MODEL (`.[]` over the element array), never a fact — matching XML, which
        // cut `xml.children@1` on the same redundancy. The child ELEMENTS are the array items;
        // `.@name`/`.@content`/`.@attrs` per item cover the projections. The comments: attached facts grouped by recovered
        // role, never child values. The role classification follows §4.10 as served: a before-child comment is leading on
        // the next recovered occurrence, and every after-child comment is inline on the preceding occurrence.
        let mut roles: [Vec<FactPayload>; 3] = [Vec::new(), Vec::new(), Vec::new()];
        for (position, child) in element.children.iter().enumerate() {
            if tree.nodes[child.0].kind != NodeKind::Comment {
                continue;
            }
            let role = comment_role(position);
            let slot = match role {
                COMMENT_ROLE_LEADING => 0,
                COMMENT_ROLE_INLINE => 1,
                COMMENT_ROLE_ROOT => 2,
                _ => unreachable!(),
            };
            // The payload carries the comment's TREE POSITION beside its text (a map per comment): the document-serialize
            // encoder reconstructs the in-place order from it, which is what keeps text runs on both sides of a comment
            // separate across a round trip. The role stays the §4.10 classification.
            roles[slot].push(FactPayload::Map(vec![
                (
                    String::from("text"),
                    FactPayload::Text(tree.nodes[child.0].data.clone()),
                ),
                (
                    String::from("position"),
                    FactPayload::Integer(Integer::from_i64(position as i64)),
                ),
            ]));
        }
        for (slot, role) in [
            (0usize, COMMENT_ROLE_LEADING),
            (1, COMMENT_ROLE_INLINE),
            (2, COMMENT_ROLE_ROOT),
        ] {
            if !roles[slot].is_empty() {
                builder
                    .add_fact(
                        LocalOwnerRef::Node(id),
                        COMMENT_FACT,
                        role,
                        1,
                        &FactPayload::List(core::mem::take(&mut roles[slot])),
                        resources,
                    )
                    .map_err(|error| {
                        #[cfg(jqf_trace)]
                        std::eprintln!("html document build {error:?}");
                        map_data(error)
                    })?;
            }
        }
    }
    // The children: text runs and child elements (comments are facts). The descendant text accumulates BOTTOM-UP — each
    // child element's own content is computed by its recursion and returned here, so this node's content is built from
    // its children's strings in one pass and the per-ancestor subtree RE-walk (O(text x depth) in CPU) is gone. The
    // content fact lands after the children (fact consumers read it by kind, never by position); the payload clone is
    // the arena copy the builder makes anyway, and the original string rides up to the parent.
    let mut content = String::new();
    for child in &element.children {
        let child_node = &tree.nodes[child.0];
        let built = match child_node.kind {
            NodeKind::Element => {
                let Some(child_prune) = PruneRef::root(prune).at(prune_id).member(child_node.name.as_bytes()) else {
                    continue;
                };
                let (built, child_content) =
                    build_node(builder, tree, *child, resources, attach_attrs, prune, child_prune)?;
                content.push_str(&child_content);
                built
            }
            NodeKind::Text => {
                content.push_str(&child_node.data);
                builder
                    .add_node(
                        TEXT_KIND,
                        AccountedSemanticNode::String(&child_node.data),
                        None,
                        resources,
                    )
                    .map_err(map_data)?
            }
            NodeKind::Comment | NodeKind::Doctype | NodeKind::Document => continue,
        };
        builder
            .add_occurrence(LocalOwnerRef::Node(id), CHILD_ROLE, None, built, resources)
            .map_err(|error| {
                #[cfg(jqf_trace)]
                std::eprintln!("html document build {error:?}");
                map_data(error)
            })?;
    }
    builder
        .add_fact(
            LocalOwnerRef::Node(id),
            CONTENT_FACT,
            CONTENT_FACT,
            1,
            &FactPayload::Text(content.clone()),
            resources,
        )
        .map_err(|error| {
            #[cfg(jqf_trace)]
            std::eprintln!("html document build {error:?}");
            map_data(error)
        })?;
    Ok((id, content))
}

/// The jqf-data identity grammar for fact kinds and roles: non-empty and free of ASCII control and whitespace bytes
/// (mirrors `jqf-data::identity::validate`, which is crate-private).
fn is_valid_identity(value: &str) -> bool {
    !value.is_empty()
        && !value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
}

/// The recovered role of one comment child (§4.10's classification as served): child index 0 is leading, every later
/// comment is inline.
fn comment_role(position: usize) -> &'static str {
    if position == 0 {
        return COMMENT_ROLE_LEADING;
    }
    COMMENT_ROLE_INLINE
}

pub(crate) fn fresh_builder(
    coverage: BuilderCoverage,
    _resources: &ResourceContext<'_>,
) -> Result<AccountedDocumentBuilder<'static>, CodecError> {
    let recipe = html_schema_recipe().map_err(map_data)?;
    let builder = AccountedDocumentBuilder::try_new_with_coverage(recipe.format(), recipe.dialect(), coverage)
        .map_err(map_data)?;
    Ok(builder)
}

/// Builds a fresh route document (no source binding) rooted at the located value: an element subtree or a text leaf. A
/// range is a stream of matches, not one Located document.
///
/// Exact prune is AFTER recover: `prune` is re-anchored at [`PruneTree::ROOT`] on the located element. Named child
/// elements are prune keys (the same tag names `.div` / `.span` locate with); [`PruneRef::member`] returning `None`
/// omits that child — it is not built, not added as an occurrence, and its text is not folded into `.@content`. An
/// element spine ([`PruneRef::element`]) keeps children the way a sequence does. Text leaves and comment facts stay.
/// `None` / keep-all is the full subtree.
pub(crate) fn build_subtree_document(
    tree: &Tree,
    located: &crate::locate::Located,
    prune: Option<&PruneLookup>,
    coverage: BuilderCoverage,
    resources: &mut ResourceContext<'_>,
) -> Result<(AccountedDocumentBuilder<'static>, NodeId), CodecError> {
    // Same split as [`build_document`]: NAME_FACT needs attached_facts;
    // attrs/comments skip when the demand named neither.
    let attach_attrs = coverage.attached_facts();
    let mut builder = fresh_builder(coverage.with_attached_facts(true), resources)?;
    let prune_id = prune.map_or(PRUNE_ALL, |_| PruneTree::ROOT);
    let root = match located {
        crate::locate::Located::Element(element) => {
            build_node(
                &mut builder,
                tree,
                HtmlNodeId(*element),
                resources,
                attach_attrs,
                prune,
                prune_id,
            )?
            .0
        }
        crate::locate::Located::Leaf { parent, position } => {
            let child = tree.nodes[*parent].children[*position];
            let data = tree.nodes[child.0].data.clone();
            builder
                .add_node(TEXT_KIND, AccountedSemanticNode::String(&data), None, resources)
                .map_err(map_data)?
        }
        crate::locate::Located::Range { .. } => return Err(decline_located_range()),
        crate::locate::Located::Missing { .. } | crate::locate::Located::TypeMismatch { .. } => {
            return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                contract: "subtree document built from a negative location",
            }));
        }
    };
    Ok((builder, root))
}

/// Located publishes one document. A member (or slice) that hits several children is a stream, so the scoped route
/// declines and the binder's whole-document floor plus engine navigation produce the items.
pub(crate) fn decline_located_range() -> CodecError {
    CodecError::new(CodecFailureKind::RequirementMismatch)
}

/// Builds the null product document (a single null scalar root).
pub(crate) fn build_null_document(
    resources: &mut ResourceContext<'_>,
) -> Result<(AccountedDocumentBuilder<'static>, NodeId), CodecError> {
    let mut builder = fresh_builder(BuilderCoverage::complete(), resources)?;
    let root = builder
        .add_node(NULL_KIND, AccountedSemanticNode::Null, None, resources)
        .map_err(|error| {
            #[cfg(jqf_trace)]
            std::eprintln!("html document build {error:?}");
            map_data(error)
        })?;
    Ok((builder, root))
}

#[cfg(test)]
mod kernel_receipt {
    #[test]
    fn mode_and_pragma_roles_are_html_crate_literals() {
        assert_eq!(super::MODE_FACT, "html.mode@1");
        assert_eq!(super::PRAGMA_LANGUAGE_FACT, "html.pragma-language@1");
    }
}

#[cfg(test)]
mod measure_build_tests {
    use super::*;
    use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

    #[test]
    fn measure_build_makes_root_and_cheap_children() {
        let mut resources = ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                .expect("account"),
            &ContinueControl,
            WorkMeter::try_new_v1(4_096).expect("work"),
        )
        .expect("resources");
        let tree = crate::tree::TreeBuilder::build("<a href=\"https://ex\">hi</a>");
        let _ = build_measure_document(&tree, &mut resources).expect("build");
    }
}

#[cfg(test)]
mod coverage_tests {
    use super::{ATTRIBUTE_FACT, NAME_FACT};
    use jqf_codec_core::{
        AccessFootprint, AccessGuarantees, AccessOutcome, AccessRequirement, CodecDemand, CodecRunContext,
        DecodeRequest, DemandClause, DiagnosticPolicy, ExactPath, ExactSelectionRecord, FactIntent, TopologyDemand,
        ValidationMode,
    };
    use jqf_data::{
        CountDemand, CountRow, DialectId, DocumentCapability, ElementDemand, ElementProbe, ElementRow, ExpandedName,
        NodeId,
    };
    use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
    use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

    static CONTROL: ContinueControl = ContinueControl;
    const INPUT: &[u8] = b"<a href=\"https://ex\">hi</a>";

    fn resources() -> ResourceContext<'static> {
        ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                .expect("account"),
            &CONTROL,
            WorkMeter::try_new_v1(4_096).expect("work"),
        )
        .expect("resources")
    }

    fn source(bytes: &[u8]) -> ResolvedSource<'_> {
        ResolvedSource::new(
            SourceRef::new(SourceId::new(1), SourceKind::Input),
            "test.html",
            bytes,
            0,
        )
    }

    fn whole_requirement(demand: CodecDemand, resources: &ResourceContext<'_>) -> AccessRequirement {
        AccessRequirement::try_whole(
            demand,
            AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            resources,
        )
        .expect("requirement")
    }

    fn decode_requirement<'bytes>(
        bytes: &'bytes [u8],
        requirement: &AccessRequirement,
        resources: &mut ResourceContext<'_>,
    ) -> jqf_codec_core::DocumentProduct<'bytes> {
        let dialect = DialectId::try_new(crate::HTML_DOCUMENT_DIALECT_ID).expect("dialect");
        let mut provider = crate::registration()
            .expect("registration")
            .decoder()
            .expect("decoder")
            .create_provider(
                source(bytes),
                DecodeRequest {
                    validation: ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect: &dialect,
                    options: None,
                    allow_adjacent_values: false,
                    value_separator: &[],
                },
                resources,
            )
            .expect("provider");
        let handle = provider.bind(requirement).expect("bind");
        let mut session = provider.open(&handle, resources).expect("open");
        let mut run = CodecRunContext::new(resources);
        run.set_cooperative_credits(4_096);
        let result = session.decode(&mut run).expect("decode");
        let AccessOutcome::FullDocument(product) = result.outcome() else {
            panic!("expected full document");
        };
        product.try_clone().expect("clone")
    }

    fn attribute_pairs(
        product: &jqf_codec_core::DocumentProduct<'_>,
    ) -> alloc::vec::Vec<(alloc::string::String, alloc::string::String)> {
        let document = product.document();
        let mut out = alloc::vec::Vec::new();
        for index in 0..document.node_count() {
            let Some(node) = NodeId::try_from_index(index) else {
                break;
            };
            for fact_id in document.owner_fact_ids(node) {
                let fact = document.fact(*fact_id).expect("fact");
                if fact.role().as_str() != ATTRIBUTE_FACT {
                    continue;
                }
                if let jqf_data::FactPayloadView::Text(text) = fact.payload() {
                    out.push((
                        alloc::string::String::from(fact.kind().as_str()),
                        alloc::string::String::from(text),
                    ));
                }
            }
        }
        out
    }

    fn named_elements(product: &jqf_codec_core::DocumentProduct<'_>) -> alloc::vec::Vec<alloc::string::String> {
        let document = product.document();
        let mut names = alloc::vec::Vec::new();
        for index in 0..document.node_count() {
            let Some(node) = NodeId::try_from_index(index) else {
                break;
            };
            for fact_id in document.owner_fact_ids(node) {
                let fact = document.fact(*fact_id).expect("fact");
                if fact.role().as_str() != NAME_FACT {
                    continue;
                }
                if let jqf_data::FactPayloadView::Text(text) = fact.payload() {
                    names.push(alloc::string::String::from(text));
                }
            }
        }
        names
    }

    fn has_name_fact(product: &jqf_codec_core::DocumentProduct<'_>) -> bool {
        let document = product.document();
        (0..document.node_count()).any(|index| {
            let Some(node) = NodeId::try_from_index(index) else {
                return false;
            };
            document.owner_fact_ids(node).iter().any(|fact_id| {
                document
                    .fact(*fact_id)
                    .is_ok_and(|fact| fact.role().as_str() == NAME_FACT)
            })
        })
    }

    fn exact_anchor_requirement(demand: CodecDemand, resources: &ResourceContext<'_>) -> AccessRequirement {
        let mut path = ExactPath::try_new(resources);
        path.try_push_semantic_member("body", resources).expect("body");
        path.try_push_semantic_member("a", resources).expect("a");
        let footprint = AccessFootprint::try_exact(path, resources);
        AccessRequirement::try_exact(
            footprint,
            demand,
            AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            resources,
        )
        .expect("requirement")
    }

    fn decode_exact<'bytes>(
        bytes: &'bytes [u8],
        requirement: &AccessRequirement,
        resources: &mut ResourceContext<'_>,
    ) -> jqf_codec_core::DocumentProduct<'bytes> {
        let dialect = DialectId::try_new(crate::HTML_DOCUMENT_DIALECT_ID).expect("dialect");
        let mut provider = crate::registration()
            .expect("registration")
            .decoder()
            .expect("decoder")
            .create_provider(
                source(bytes),
                DecodeRequest {
                    validation: ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect: &dialect,
                    options: None,
                    allow_adjacent_values: false,
                    value_separator: &[],
                },
                resources,
            )
            .expect("provider");
        let handle = provider.bind(requirement).expect("bind");
        let mut session = provider.open(&handle, resources).expect("open");
        let mut run = CodecRunContext::new(resources);
        run.set_cooperative_credits(4_096);
        let result = session.decode(&mut run).expect("decode");
        let AccessOutcome::Located(located) = result.outcome() else {
            panic!("expected Located, got {:?}", result.outcome());
        };
        let ExactSelectionRecord::Node { .. } = located.result() else {
            panic!(
                "Exact must republish the located element as root, got {:?}",
                located.result()
            );
        };
        located.product().try_clone().expect("clone")
    }

    #[test]
    fn identity_skips_attribute_facts_and_keeps_names() {
        let mut resources = resources();
        let requirement = whole_requirement(CodecDemand::try_new(&resources), &resources);
        let product = decode_requirement(INPUT, &requirement, &mut resources);
        assert!(
            attribute_pairs(&product).is_empty(),
            "identity must skip attribute facts"
        );
        assert!(has_name_fact(&product), "NAME_FACT is the HTML value model");
        assert!(
            !product.document().coverage().contains(DocumentCapability::Topology),
            "identity JSON projection omits occurrence topology"
        );
    }

    #[test]
    fn exact_identity_skips_attribute_facts_and_keeps_names() {
        let mut resources = resources();
        let requirement = exact_anchor_requirement(CodecDemand::try_new(&resources), &resources);
        let product = decode_exact(INPUT, &requirement, &mut resources);
        assert!(
            attribute_pairs(&product).is_empty(),
            "Exact identity must skip attribute facts the same way Whole does"
        );
        assert!(has_name_fact(&product), "NAME_FACT is the HTML value model");
    }

    #[test]
    fn empty_path_count_skips_attribute_facts() {
        let mut resources = resources();
        let requirement = whole_requirement(CodecDemand::try_new(&resources), &resources).with_count(CountDemand {
            row: CountRow::Container,
            path: Vec::new(),
            range: None,
            probe: Vec::new(),
            filter: None,
        });
        let product = decode_requirement(INPUT, &requirement, &mut resources);
        assert!(
            attribute_pairs(&product).is_empty(),
            "empty-path length must skip ATTRIBUTE_FACT tables"
        );
        assert!(has_name_fact(&product), "NAME_FACT is the HTML value model");
        assert!(
            !named_elements(&product).iter().any(|name| name == "a"),
            "measure skeleton must not project descendant elements: {:?}",
            named_elements(&product)
        );
    }

    #[test]
    fn empty_path_type_skips_attribute_facts() {
        let mut resources = resources();
        let requirement = whole_requirement(CodecDemand::try_new(&resources), &resources).with_type_demand();
        let product = decode_requirement(INPUT, &requirement, &mut resources);
        assert!(
            attribute_pairs(&product).is_empty(),
            "bare type must skip ATTRIBUTE_FACT tables"
        );
        assert!(has_name_fact(&product), "NAME_FACT is the HTML value model");
        assert!(
            !named_elements(&product).iter().any(|name| name == "a"),
            "measure skeleton must not project descendant elements"
        );
        let kind = product
            .document()
            .value_view(product.document().root_handle())
            .expect("root view")
            .kind()
            .expect("root kind");
        assert_eq!(kind, jqf_data::ValueKind::Array, "root kind is array");
    }

    #[test]
    fn empty_path_element_projects_recovered_children() {
        let mut resources = resources();
        let requirement = whole_requirement(CodecDemand::try_new(&resources), &resources).with_element(ElementDemand {
            row: ElementRow::FanOut,
            path: Vec::new(),
            range: None,
            probe: ElementProbe::Path(Vec::new()),
            increment: None,
            filter: None,
        });
        let product = decode_requirement(INPUT, &requirement, &mut resources);
        assert!(
            attribute_pairs(&product).is_empty(),
            "empty-path element must skip ATTRIBUTE_FACT tables"
        );
        assert!(has_name_fact(&product), "NAME_FACT is the HTML value model");
        assert!(
            named_elements(&product).iter().any(|name| name == "a"),
            "element demand must project recovered children, not measure stubs: {:?}",
            named_elements(&product)
        );
    }

    #[test]
    fn preserve_attaches_attribute_facts() {
        let mut resources = resources();
        let requirement =
            whole_requirement(CodecDemand::try_new(&resources), &resources).with_fact_intent(FactIntent::Preserve);
        let product = decode_requirement(INPUT, &requirement, &mut resources);
        assert!(
            attribute_pairs(&product)
                .iter()
                .any(|(name, value)| name == "href" && value == "https://ex"),
            "Preserve must keep @attr"
        );
        assert!(
            product.document().coverage().contains(DocumentCapability::Topology),
            "Preserve is identity re-encode; encode reads occurrence topology"
        );
    }

    #[test]
    fn topology_clause_keeps_occurrence_topology() {
        let mut resources = resources();
        let mut demand = CodecDemand::try_new(&resources);
        demand
            .try_insert(&DemandClause::Topology(TopologyDemand::Children))
            .expect("insert");
        let product = decode_requirement(INPUT, &whole_requirement(demand, &resources), &mut resources);
        assert!(
            product.document().coverage().contains(DocumentCapability::Topology),
            "xpath/css Topology clause retains occurrence topology"
        );
        assert!(
            attribute_pairs(&product).is_empty(),
            "Topology without Preserve still skips attribute facts"
        );
        assert!(has_name_fact(&product), "NAME_FACT is the HTML value model");
    }

    #[test]
    fn attribute_clause_attaches_attribute_facts() {
        let mut resources = resources();
        let mut demand = CodecDemand::try_new(&resources);
        demand
            .try_insert(&DemandClause::Attribute(
                ExpandedName::try_new("", "href").expect("href"),
            ))
            .expect("insert");
        let product = decode_requirement(INPUT, &whole_requirement(demand, &resources), &mut resources);
        assert!(
            attribute_pairs(&product)
                .iter()
                .any(|(name, value)| name == "href" && value == "https://ex"),
            ".&href must keep attrs"
        );
    }
}

#[cfg(test)]
mod exact_prune_tests {
    use super::{CONTENT_FACT, NAME_FACT};
    use crate::locate::{self, Located};
    use crate::tree::NodeKind;
    use jqf_codec_core::{
        AccessFootprint, AccessGuarantees, AccessOutcome, AccessRequirement, CodecDemand, CodecRunContext,
        DecodeRequest, DemandClause, DiagnosticPolicy, ExactPath, ExactSelectionRecord, PortableStep, PruneTree,
        ValidationMode,
    };
    use jqf_data::{DialectId, Document, NodeId, Value};
    use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
    use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

    static CONTROL: ContinueControl = ContinueControl;

    /// One unique `<section>` with a kept `<span>` and a fat unread `<p>` sibling.
    const FAT: &[u8] =
        b"<body><section><span>keep</span><p>unread nested <em>junk</em> filler filler filler filler</p></section></body>";
    /// The omitted sibling is malformed; recover rewrites it (a `<div>` cannot stay inside `<p>`).
    const BROKEN_OMITTED: &[u8] = b"<body><section><span>keep</span><p>unread <div>nested junk</section></body>";

    fn resources() -> ResourceContext<'static> {
        ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                .expect("account"),
            &CONTROL,
            WorkMeter::try_new_v1(4_096).expect("work"),
        )
        .expect("resources")
    }

    fn source(bytes: &[u8]) -> ResolvedSource<'_> {
        ResolvedSource::new(
            SourceRef::new(SourceId::new(1), SourceKind::Input),
            "test.html",
            bytes,
            0,
        )
    }

    fn demand(resources: &ResourceContext<'_>) -> CodecDemand {
        let mut demand = CodecDemand::try_new(resources);
        demand.try_insert(&DemandClause::SemanticRoot).expect("semantic root");
        demand.try_insert(&DemandClause::ValueShape).expect("value shape");
        demand
    }

    fn exact_section_requirement(demand: CodecDemand, resources: &ResourceContext<'_>) -> AccessRequirement {
        let mut path = ExactPath::try_new(resources);
        path.try_push_semantic_member("body", resources).expect("body");
        path.try_push_semantic_member("section", resources).expect("section");
        let footprint = AccessFootprint::try_exact(path, resources);
        AccessRequirement::try_exact(
            footprint,
            demand,
            AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            resources,
        )
        .expect("requirement")
    }

    fn keep_member_tree(name: &str, resources: &ResourceContext<'_>) -> PruneTree {
        let mut tree = PruneTree::try_new(resources).expect("tree");
        let keep = tree.try_push_node(true).expect("keep");
        tree.try_push_key(PruneTree::ROOT, name, keep).expect("key");
        tree
    }

    fn decode_exact<'bytes>(
        bytes: &'bytes [u8],
        requirement: &AccessRequirement,
        resources: &mut ResourceContext<'_>,
    ) -> jqf_codec_core::DocumentProduct<'bytes> {
        let dialect = DialectId::try_new(crate::HTML_DOCUMENT_DIALECT_ID).expect("dialect");
        let mut provider = crate::registration()
            .expect("registration")
            .decoder()
            .expect("decoder")
            .create_provider(
                source(bytes),
                DecodeRequest {
                    validation: ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect: &dialect,
                    options: None,
                    allow_adjacent_values: false,
                    value_separator: &[],
                },
                resources,
            )
            .expect("provider");
        let handle = provider.bind(requirement).expect("bind");
        let mut session = provider.open(&handle, resources).expect("open");
        let mut run = CodecRunContext::new(resources);
        run.set_cooperative_credits(4_096);
        let result = session.decode(&mut run).expect("decode");
        let AccessOutcome::Located(located) = result.outcome() else {
            panic!("expected Located, got {:?}", result.outcome());
        };
        let ExactSelectionRecord::Node { node, .. } = located.result() else {
            panic!(
                "Exact must republish the located element as root, got {:?}",
                located.result()
            );
        };
        assert_eq!(
            *node,
            located.product().document().root_handle(),
            "native Exact republishes the selection as root"
        );
        located.product().try_clone().expect("clone")
    }

    fn named_elements(document: &Document<'_>) -> alloc::vec::Vec<alloc::string::String> {
        let mut names = alloc::vec::Vec::new();
        for index in 0..document.node_count() {
            let Some(node) = NodeId::try_from_index(index) else {
                break;
            };
            for fact_id in document.owner_fact_ids(node) {
                let fact = document.fact(*fact_id).expect("fact");
                if fact.role().as_str() != NAME_FACT {
                    continue;
                }
                if let jqf_data::FactPayloadView::Text(text) = fact.payload() {
                    names.push(alloc::string::String::from(text));
                }
            }
        }
        names
    }

    fn content_of(document: &Document<'_>, node: NodeId) -> alloc::string::String {
        for fact_id in document.owner_fact_ids(node) {
            let fact = document.fact(*fact_id).expect("fact");
            if fact.role().as_str() != CONTENT_FACT {
                continue;
            }
            if let jqf_data::FactPayloadView::Text(text) = fact.payload() {
                return alloc::string::String::from(text);
            }
        }
        panic!("missing content fact");
    }

    fn recovered_section_child_elements(bytes: &[u8]) -> alloc::vec::Vec<alloc::string::String> {
        let text = core::str::from_utf8(bytes).expect("utf8");
        let tree = crate::tree::TreeBuilder::build(text);
        let steps = locate::own_steps(&[
            PortableStep::SemanticMember(alloc::string::String::from("body")),
            PortableStep::SemanticMember(alloc::string::String::from("section")),
        ])
        .expect("steps");
        let located = locate::locate(&tree, &steps);
        let Located::Element(id) = located else {
            panic!("expected located section, got {located:?}");
        };
        tree.nodes[id]
            .children
            .iter()
            .filter(|child| tree.nodes[child.0].kind == NodeKind::Element)
            .map(|child| tree.nodes[child.0].name.clone())
            .collect()
    }

    /// Exact prune omits unread named child elements of the located subtree. Recover still ran: the omitted sibling is
    /// in the recovered tree before materialize.
    #[test]
    fn exact_prune_omits_unread_members_of_the_located_element() {
        let mut resources = resources();
        let recovered = recovered_section_child_elements(FAT);
        assert!(
            recovered.iter().any(|name| name == "span") && recovered.iter().any(|name| name == "p"),
            "recover must still produce the omitted sibling before prune: {recovered:?}"
        );

        let pruned_requirement =
            exact_section_requirement(demand(&resources), &resources).with_prune(keep_member_tree("span", &resources));
        let pruned = decode_exact(FAT, &pruned_requirement, &mut resources);
        let pruned_names = named_elements(pruned.document());
        assert_eq!(
            pruned_names,
            ["section", "span"],
            "located section republished as root keeps only the span child element"
        );
        let Value::Array(items) = pruned.document().materialize_root(&mut resources).expect("materialize") else {
            panic!("located section is an array");
        };
        assert_eq!(items.len(), 1, "omitted p is not an array item: {items:?}");
        assert!(
            !content_of(pruned.document(), pruned.document().root()).contains("unread"),
            "omitted p text must not fold into .@content"
        );

        let full = decode_exact(
            FAT,
            &exact_section_requirement(demand(&resources), &resources),
            &mut resources,
        );
        let full_names = named_elements(full.document());
        assert!(
            full_names.iter().any(|name| name == "span") && full_names.iter().any(|name| name == "p"),
            "unpruned Exact of the same bytes still has both span and p: {full_names:?}"
        );
        assert!(
            pruned.document().node_count() < full.document().node_count(),
            "pruned node count {} must be smaller than unpruned {}",
            pruned.document().node_count(),
            full.document().node_count()
        );

        let broken_recovered = recovered_section_child_elements(BROKEN_OMITTED);
        assert!(
            broken_recovered.iter().any(|name| name == "p"),
            "WHATWG recover must still produce the omitted sibling, not skip recover: {broken_recovered:?}"
        );
        let broken = decode_exact(BROKEN_OMITTED, &pruned_requirement, &mut resources);
        let broken_names = named_elements(broken.document());
        assert_eq!(
            broken_names,
            ["section", "span"],
            "prune still omits the recovered unread sibling: {broken_names:?}"
        );
    }

    /// No-prune Exact stays the full located subtree (the keep-all / absent-hint regression).
    #[test]
    fn no_prune_exact_stays_the_full_subtree() {
        let mut resources = resources();
        let requirement = exact_section_requirement(demand(&resources), &resources);
        let product = decode_exact(FAT, &requirement, &mut resources);
        let names = named_elements(product.document());
        assert!(
            names.iter().any(|name| name == "span")
                && names.iter().any(|name| name == "p")
                && names.iter().any(|name| name == "em"),
            "no-prune Exact must keep the full subtree: {names:?}"
        );
        assert_eq!(names.first().map(alloc::string::String::as_str), Some("section"));
        assert!(
            content_of(product.document(), product.document().root()).contains("unread"),
            "no-prune Exact must fold the unread sibling into .@content"
        );
    }
}
