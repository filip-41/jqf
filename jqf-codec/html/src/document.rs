//! HTML semantic document construction (§4.10's projection).
//!
//! The recovered WHATWG tree projects into the format-neutral [`jqf_data::Document`] exactly as XML's does: an element
//! is an ARRAY of its recovered children (text runs and child elements; comments are ATTACHED FACTS, never child values
//! — the HTML comment model), its normalized expanded name and recovered semantic attribute map are facts, and the
//! document carries its recovered mode, pragma-set default language, and doctype as document-level facts.

use alloc::string::String;
use alloc::vec::Vec;

use alloc::vec;
use jqf_codec_core::{CodecError, CodecFailureKind};

use jqf_data::{
    AccountedDocumentBuilder, AccountedSemanticNode, BuilderCoverage, DataError, DocumentSchemaRecipe, FactPayload,
    Integer, LocalOwnerRef, NodeId,
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
    // A builder can raise an UNREPRESENTABLE shape on the HTML tree; that arm is the codec's own, everything else is
    // the shared mapping.
    match error {
        DataError::UnrepresentableSemantic | DataError::CyclicSemanticGraph => {
            CodecError::new(CodecFailureKind::UnsupportedRepresentation)
        }
        other => {
            #[cfg(jqf_trace)]
            std::eprintln!("HTML builder data error: {other:?}");
            #[cfg(not(jqf_trace))]
            let _ = &other;
            jqf_codec_core::map_data(other, "HTML builder rejected document construction")
        }
    }
}

/// Builds the semantic document from the recovered tree. The root is the document element (the `html` element). The
/// session seals and binds the retained source authority cooperatively AFTER the build (the HTML session's Seal phase),
/// so the source-echo encoder can read the sealed source segment.
pub(crate) fn build_document(
    tree: &Tree,
    resources: &mut ResourceContext<'_>,
) -> Result<(AccountedDocumentBuilder<'static>, NodeId), CodecError> {
    let mut builder = fresh_builder(resources)?;
    // The document element: the html element. An element-less tree (a comment-only or doctype-only document — the
    // "<!--x-->" corpus row) still decodes: the semantic root is a SYNTHETIC empty html element, because the WHATWG
    // parser accepts such documents (the tree has no document element for them) and the document's facts need a home.
    // The recovered comments/doctype stay document-level facts on that root.
    let root = build_document_element(&mut builder, tree, resources)?;

    // The document-level facts: mode, pragma language, doctype.
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
            std::eprintln!("doc build site L152 {error:?}");
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
                std::eprintln!("doc build site L186 {error:?}");
                map_data(error)
            })?;
    }
    // The doctype: the document node's doctype child.
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
                std::eprintln!("doc build site L223 {error:?}");
                map_data(error)
            })?;
    }
    Ok((builder, root))
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
) -> Result<NodeId, CodecError> {
    let Some(html) = tree.document_element() else {
        return build_synthetic_empty_document(builder, tree, resources);
    };
    let id = build_node(builder, tree, html, resources).map(|(id, _)| id)?;
    attach_document_edge_comments(builder, tree, id, resources)?;
    Ok(id)
}

/// Builds the synthetic root for an element-less document: the html element with the implied empty head and body, and
/// the document-level comments as ROOT-role facts on it.
fn build_synthetic_empty_document(
    builder: &mut AccountedDocumentBuilder<'static>,
    tree: &Tree,
    resources: &mut ResourceContext<'_>,
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
    attach_document_edge_comments(builder, tree, html, resources)?;
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

/// Builds one element (an array of its children) with its facts.
fn build_node(
    builder: &mut AccountedDocumentBuilder<'static>,
    tree: &Tree,
    index: HtmlNodeId,
    resources: &mut ResourceContext<'_>,
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
            std::eprintln!("doc build site L248 {error:?}");
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
            std::eprintln!("doc build site L273 {error:?}");
            map_data(error)
        })?;
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
                    std::eprintln!("doc build site L397 {error:?}");
                    map_data(error)
                })?;
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
                let (built, child_content) = build_node(builder, tree, *child, resources)?;
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
                std::eprintln!("doc build site L441 {error:?}");
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
            std::eprintln!("doc build site L328 {error:?}");
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

pub(crate) fn fresh_builder(_resources: &ResourceContext<'_>) -> Result<AccountedDocumentBuilder<'static>, CodecError> {
    let recipe = html_schema_recipe().map_err(map_data)?;
    let builder =
        AccountedDocumentBuilder::try_new_with_coverage(recipe.format(), recipe.dialect(), BuilderCoverage::complete())
            .map_err(map_data)?;
    Ok(builder)
}

/// Builds a fresh route document (no source binding) rooted at the located value: an element subtree or a text leaf. A
/// range is a stream of matches, not one Located document.
pub(crate) fn build_subtree_document(
    tree: &Tree,
    located: &crate::locate::Located,
    resources: &mut ResourceContext<'_>,
) -> Result<(AccountedDocumentBuilder<'static>, NodeId), CodecError> {
    let mut builder = fresh_builder(resources)?;
    let root = match located {
        crate::locate::Located::Element(element) => build_node(&mut builder, tree, HtmlNodeId(*element), resources)?.0,
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
    let mut builder = fresh_builder(resources)?;
    let root = builder
        .add_node(NULL_KIND, AccountedSemanticNode::Null, None, resources)
        .map_err(|error| {
            #[cfg(jqf_trace)]
            std::eprintln!("doc build site L551 {error:?}");
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
