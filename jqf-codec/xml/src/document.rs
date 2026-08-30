//! XML semantic document construction (whole-document route).
//!
//! Per §4.9 the document is a tagged directed graph whose truth is an ordered
//! mixed-content tree. This projection models an element
//! as an ARRAY of its ordered children (each child element, text run, comment,
//! or processing instruction is one array item) — "the document is the
//! truth": order and duplicate siblings survive byte-faithfully, which is the
//! invariant `xml.source@1` echo and `xml.jqf-deterministic@1` rewrite are
//! built on. The element's resolved expanded name and its resolved semantic
//! attribute map are carried as attached facts, NOT as object members, so the
//! `@attrs`/`@children`/`@content` accessor surface stays computed rather
//! than stored.

use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::vec::Vec;

use jqf_codec_core::{CodecError, CodecFailureKind};
use jqf_data::{
    AccountedDocumentBuilder, AccountedSemanticNode, BuilderCoverage, DataError, DocumentSchemaRecipe, FactPayload,
    LocalOwnerRef, NodeId,
};
use jqf_resource::ResourceContext;

use crate::locate::LocatedHit;
use crate::value::{ContentEvent, Tree};

pub(crate) const ELEMENT_KIND: &str = "xml.element@1";
pub(crate) const TEXT_KIND: &str = jqf_codec_core::markup::TEXT_KIND;
pub(crate) const COMMENT_KIND: &str = jqf_codec_core::markup::COMMENT_KIND;
pub(crate) const PI_KIND: &str = jqf_codec_core::markup::PI_KIND;
pub(crate) const NULL_KIND: &str = "xml.null@1";
pub(crate) const ATTRIBUTE_KIND: &str = "xml.attribute@1";
pub(crate) const CHILD_ROLE: &str = "xml.child@1";
pub(crate) const NAME_FACT: &str = jqf_codec_core::markup::NAME_FACT;
pub(crate) const ATTRS_FACT: &str = jqf_codec_core::markup::ATTRS_FACT;
pub(crate) const CONTENT_FACT: &str = jqf_codec_core::markup::CONTENT_FACT;
/// One fact PER ATTRIBUTE, so the `.&name` markup-attribute accessor serves
/// each expanded-name attribute. The engine's `.&` selector matches exactly
/// this role with the attribute's expanded name as the fact kind.
pub(crate) const ATTRIBUTE_FACT: &str = jqf_codec_core::markup::ATTRIBUTE_FACT;
/// The element's direct comment children, in order, as a list payload. The
/// cross-format comment model reads it through `.@comment`.
pub(crate) const COMMENT_FACT: &str = "xml.comment@1";
/// A document-level fact marking a doctype-bearing source, attached to the
/// root: the deterministic encoder's preflight rejects doctype-bearing
/// documents rather than rewriting an internal declaration language.
pub(crate) const DOCTYPE_FACT: &str = "xml.doctype@1";

fn xml_schema_recipe() -> Result<DocumentSchemaRecipe<'static>, DataError> {
    DocumentSchemaRecipe::try_new(
        "xml",
        Some("xml"),
        &[ELEMENT_KIND, TEXT_KIND, COMMENT_KIND, PI_KIND, ATTRIBUTE_KIND],
        &[CHILD_ROLE],
        &[
            NAME_FACT,
            ATTRS_FACT,
            CONTENT_FACT,
            ATTRIBUTE_FACT,
            COMMENT_FACT,
            DOCTYPE_FACT,
        ],
        &[
            NAME_FACT,
            ATTRS_FACT,
            CONTENT_FACT,
            ATTRIBUTE_FACT,
            COMMENT_FACT,
            DOCTYPE_FACT,
        ],
    )
}

/// The schema recipe shared by the located (scoped) route builder: the four
/// projection kinds plus the
/// null scalar the negative and stand-in products build.
fn route_schema_recipe() -> Result<DocumentSchemaRecipe<'static>, DataError> {
    DocumentSchemaRecipe::try_new(
        "xml",
        Some("xml"),
        &[
            ELEMENT_KIND,
            TEXT_KIND,
            COMMENT_KIND,
            PI_KIND,
            NULL_KIND,
            ATTRIBUTE_KIND,
        ],
        &[CHILD_ROLE],
        &[
            NAME_FACT,
            ATTRS_FACT,
            CONTENT_FACT,
            ATTRIBUTE_FACT,
            COMMENT_FACT,
            DOCTYPE_FACT,
        ],
        &[
            NAME_FACT,
            ATTRS_FACT,
            CONTENT_FACT,
            ATTRIBUTE_FACT,
            COMMENT_FACT,
            DOCTYPE_FACT,
        ],
    )
}

pub(crate) fn map_data(error: DataError) -> CodecError {
    // A builder can raise an UNREPRESENTABLE shape on the XML tree; that arm
    // is the codec's own, everything else is the shared mapping.
    match error {
        DataError::UnrepresentableSemantic | DataError::CyclicSemanticGraph => {
            CodecError::new(CodecFailureKind::UnsupportedRepresentation)
        }
        other => jqf_codec_core::map_data(other, "XML builder rejected document construction"),
    }
}

/// Builds the semantic document from the parsed tree. The root is
/// `tree.root` — the document element. The session seals and binds the
/// retained source authority cooperatively AFTER the build (the XML session's
/// Seal phase), so the source-echo encoder can reuse the original bytes.
pub(crate) fn build_document(
    tree: &Tree,
    resources: &mut ResourceContext<'_>,
    bind_spans: bool,
) -> Result<(AccountedDocumentBuilder<'static>, NodeId), CodecError> {
    build_document_with_content(tree, resources, bind_spans, true, BuilderCoverage::complete())
}

pub(crate) fn build_document_with_content(
    tree: &Tree,
    resources: &mut ResourceContext<'_>,
    bind_spans: bool,
    attach_content: bool,
    coverage: BuilderCoverage,
) -> Result<(AccountedDocumentBuilder<'static>, NodeId), CodecError> {
    // Names, content, and occurrence topology are the XML value model (the
    // encoder reads topology). Attribute and comment facts skip when the
    // demand named neither (checked before attached_facts is forced). NAME_FACT
    // writes need attached_facts; topology follows the requirement.
    let attach_attrs = coverage.attached_facts();
    let mut builder = fresh_builder(coverage.with_attached_facts(true), resources)?;
    let mut flat = String::new();
    let (root, _) = build_node(
        &mut builder,
        tree,
        tree.root,
        resources,
        bind_spans,
        attach_content,
        attach_attrs,
        &mut flat,
    )?;
    if tree.had_doctype {
        // The deterministic encoder's preflight rejects doctype-bearing
        // documents; carry the fact on the root so a located document can be
        // asked without re-scanning the source.
        builder
            .add_fact(
                LocalOwnerRef::Node(root),
                DOCTYPE_FACT,
                DOCTYPE_FACT,
                1,
                &FactPayload::Bool(true),
                resources,
            )
            .map_err(map_data)?;
    }
    Ok((builder, root))
}

pub(crate) fn fresh_builder(
    coverage: BuilderCoverage,
    _resources: &ResourceContext<'_>,
) -> Result<AccountedDocumentBuilder<'static>, CodecError> {
    let recipe = xml_schema_recipe().map_err(map_data)?;
    let builder = AccountedDocumentBuilder::try_new_with_coverage(recipe.format(), recipe.dialect(), coverage)
        .map_err(map_data)?;
    Ok(builder)
}

#[expect(
    clippy::too_many_arguments,
    reason = "one element walk: builder, tree, coverage flags, and the flat-text buffer are the whole shape"
)]
fn build_node(
    builder: &mut AccountedDocumentBuilder<'static>,
    tree: &Tree,
    index: usize,
    resources: &mut ResourceContext<'_>,
    bind_spans: bool,
    attach_content: bool,
    attach_attrs: bool,
    flat: &mut String,
) -> Result<(NodeId, (usize, usize)), CodecError> {
    let _depth = resources.enter_nesting_owned().map_err(CodecError::from)?;
    let element = &tree.elements[index];
    let name_text = element.name.clark(&tree.intern);
    let id = builder
        .add_node(
            ELEMENT_KIND,
            AccountedSemanticNode::Array { item_role: CHILD_ROLE },
            None,
            resources,
        )
        .map_err(map_data)?;
    // The element's authored extent (start tag through end tag): the edit
    // lane's structural splice reads it to find the end tag. Spans are bound
    // on the WHOLE-DOCUMENT build only (the edit lane's product; the same
    // route split TOML keeps) — a span-bound document needs a sealed source
    // authority, which the scoped route builder does not retain.
    if bind_spans && element.end > element.start {
        record_span(builder, id, element.start, element.end, resources)?;
    }
    builder
        .add_fact(
            LocalOwnerRef::Node(id),
            NAME_FACT,
            NAME_FACT,
            1,
            &FactPayload::Text(name_text),
            resources,
        )
        .map_err(map_data)?;
    if attach_attrs {
        let attr_clarks: Vec<String> = element
            .attributes
            .iter()
            .map(|(expanded, _)| expanded.clark(&tree.intern))
            .collect();
        // One per-attribute fact so `.&name` can serve each expanded-name
        // attribute directly: the role is the engine's exact `.&` contract and
        // the kind is the attribute's clark name. The quoted-value authored span
        // lives on this fact (not a minted Null node): `--edit` splices those
        // bytes, and `.@attrs` is the map projection of the same table.
        for (index, ((_, value), clark)) in element.attributes.iter().zip(attr_clarks.iter()).enumerate() {
            let fact = builder
                .add_fact(
                    LocalOwnerRef::Node(id),
                    ATTRIBUTE_FACT,
                    clark,
                    1,
                    &FactPayload::Text(value.clone()),
                    resources,
                )
                .map_err(map_data)?;
            if bind_spans && let Some(&(start, end)) = element.attribute_spans.get(index) {
                record_fact_span(builder, fact, start, end, resources)?;
            }
        }
        // The element's direct comment children, in order — the `.@comment` fact.
        // The comments remain ordinary array children (byte identity owns the
        // mixed-content array); the fact is the cross-format comment projection.
        let comments: Vec<FactPayload> = element
            .content
            .iter()
            .filter_map(|event| match event {
                ContentEvent::Comment(text) => Some(FactPayload::Text(text.clone())),
                _ => None,
            })
            .collect();
        if !comments.is_empty() {
            builder
                .add_fact(
                    LocalOwnerRef::Node(id),
                    COMMENT_FACT,
                    COMMENT_FACT,
                    1,
                    &FactPayload::List(comments),
                    resources,
                )
                .map_err(map_data)?;
        }
    }
    // Descendant text is appended once into `flat` in document order. Each
    // element's content is the slice it covered; a parent does not copy a
    // child's buffer. The CONTENT fact is sliced from that range (one copy
    // into the payload) so ancestor memcpy is O(text), not O(text x depth).
    let content_start = flat.len();
    for (position, event) in element.content.iter().enumerate() {
        let child = match event {
            ContentEvent::Text(text) => {
                if attach_content {
                    flat.push_str(text);
                }
                let node = add_leaf(builder, TEXT_KIND, AccountedSemanticNode::String(text), resources)?;
                // The text node's authored span (its exact text bytes,
                // entities preserved): the edit lane's leaf patch seam. The
                // aligned span is always present for a text event.
                if bind_spans && let Some(Some((start, end))) = element.content_spans.get(position) {
                    record_span(builder, node, *start, *end, resources)?;
                }
                node
            }
            ContentEvent::Comment(text) => {
                let node = add_leaf(builder, COMMENT_KIND, AccountedSemanticNode::String(text), resources)?;
                // The comment's authored extent (`<!--` through `-->`): the
                // comment-write seam replaces comment children by their
                // spans, so a comment leaf binds one exactly like a text
                // leaf (prolog/epilog comments carry no
                // span and decline to the floor, like processing
                // instructions).
                if bind_spans && let Some(Some((start, end))) = element.content_spans.get(position) {
                    record_span(builder, node, *start, *end, resources)?;
                }
                node
            }
            ContentEvent::ProcessingInstruction(payload) => {
                let (target, data) = payload.as_ref();
                let spelling = crate::value::pi_spelling(target, data);
                add_leaf(builder, PI_KIND, AccountedSemanticNode::String(&spelling), resources)?
            }
            ContentEvent::Element(child_index) => {
                let (child_id, _) = build_node(
                    builder,
                    tree,
                    *child_index,
                    resources,
                    bind_spans,
                    attach_content,
                    attach_attrs,
                    flat,
                )?;
                child_id
            }
        };
        builder
            .add_occurrence(LocalOwnerRef::Node(id), CHILD_ROLE, None, child, resources)
            .map_err(map_data)?;
    }
    let content_end = flat.len();
    if attach_content {
        let payload = FactPayload::Text(flat[content_start..content_end].to_owned());
        builder
            .add_fact(
                LocalOwnerRef::Node(id),
                CONTENT_FACT,
                CONTENT_FACT,
                1,
                &payload,
                resources,
            )
            .map_err(map_data)?;
    }
    Ok((id, (content_start, content_end)))
}

fn add_leaf(
    builder: &mut AccountedDocumentBuilder<'static>,
    kind: &'static str,
    semantic: AccountedSemanticNode<'_>,
    resources: &ResourceContext<'_>,
) -> Result<NodeId, CodecError> {
    builder.add_node(kind, semantic, None, resources).map_err(map_data)
}

/// Records one authored source span on the builder: the edit lane's
/// addressing channel. The semantic is stored exactly as without the span;
/// the span names the authored bytes the edit lane echoes verbatim or
/// patches. Nodes must be authored in strictly increasing id order, which
/// the sequential build guarantees.
fn record_span(
    builder: &mut AccountedDocumentBuilder<'static>,
    node: NodeId,
    start: usize,
    end: usize,
    resources: &ResourceContext<'_>,
) -> Result<(), CodecError> {
    let span = jqf_source::Span::try_new(
        u32::try_from(start).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?,
        u32::try_from(end).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?,
    )
    .ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?;
    // SAFETY: the span was produced by this codec's own token walk over the
    // exact source authority bound to the builder (the session seals and
    // binds it after the build), and its bytes re-decode to the node's stored
    // semantic — the `record_authored_span` contract.
    unsafe { builder.record_authored_span(node, span, resources) }.map_err(map_data)
}

/// Records one authored quoted-value span on an attribute fact.
fn record_fact_span(
    builder: &mut AccountedDocumentBuilder<'static>,
    fact: jqf_data::FactId,
    start: usize,
    end: usize,
    resources: &ResourceContext<'_>,
) -> Result<(), CodecError> {
    let span = jqf_source::Span::try_new(
        u32::try_from(start).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?,
        u32::try_from(end).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?,
    )
    .ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?;
    // SAFETY: the span was produced by this codec's own token walk over the
    // exact source authority bound to the builder; the bytes are the
    // attribute's authored quoted value, which re-decode to the fact payload.
    unsafe { builder.record_fact_authored_span(fact, span, resources) }.map_err(map_data)
}

/// Builds the scoped product from a locate-during-parse hit. Element extents
/// are re-parsed (they are standalone XML documents); leaves assemble
/// without a whole-input Tree. A range hit is a stream, not one located
/// document: it declines to the whole-document floor.
pub(crate) fn build_from_hit(
    bytes: &[u8],
    hit: &LocatedHit,
    resources: &mut ResourceContext<'_>,
) -> Result<(AccountedDocumentBuilder<'static>, NodeId), CodecError> {
    match hit {
        LocatedHit::Element { start, end } => {
            let tree = parse_span_tree(&bytes[*start..*end], resources)?;
            build_subtree_document(&tree, tree.root, resources)
        }
        LocatedHit::Leaf { kind, value } => {
            let mut builder = fresh_route_builder(resources)?;
            let root = add_leaf(&mut builder, kind, AccountedSemanticNode::String(value), resources)?;
            Ok((builder, root))
        }
        LocatedHit::Range { .. } => Err(decline_located_range()),
        LocatedHit::Missing { .. } | LocatedHit::TypeMismatch { .. } => Err(locate_contract()),
    }
}

fn parse_span_tree(bytes: &[u8], resources: &mut ResourceContext<'_>) -> Result<Tree, CodecError> {
    let span =
        core::str::from_utf8(bytes).map_err(|_| jqf_codec_core::data_contract("XML located span is not UTF-8"))?;
    let mut parse = crate::parse::XmlParseState::try_new_prevalidated(span)?.without_spans();
    loop {
        match parse.poll(span.as_bytes(), resources)? {
            crate::parse::ParsePoll::Pending => {
                resources.try_begin_next_cooperative_entry(1)?;
            }
            crate::parse::ParsePoll::Ready(crate::parse::ParseOutput::Tree(tree)) => {
                return Ok(tree);
            }
            crate::parse::ParsePoll::Ready(_) => {
                return Err(jqf_codec_core::data_contract(
                    "XML located span re-parse was not a tree",
                ));
            }
        }
    }
}

/// Builds a fresh document whose root is ONE element of the tree: the
/// scoped route's product for an element hit. The scoped/product builders
/// share [`build_node`] but do NOT bind spans: they finish without a sealed
/// source authority (the same route split TOML keeps), and the edit lane
/// consumes whole-document products.
pub(crate) fn build_subtree_document(
    tree: &Tree,
    element: usize,
    resources: &mut ResourceContext<'_>,
) -> Result<(AccountedDocumentBuilder<'static>, NodeId), CodecError> {
    let mut builder = fresh_route_builder(resources)?;
    let mut flat = String::new();
    let root = build_node(&mut builder, tree, element, resources, false, true, true, &mut flat)?.0;
    Ok((builder, root))
}

/// Builds the null product document (a single null scalar root), used by
/// the scoped route's negative observations.
pub(crate) fn build_null_document(
    resources: &mut ResourceContext<'_>,
) -> Result<(AccountedDocumentBuilder<'static>, NodeId), CodecError> {
    let mut builder = fresh_route_builder(resources)?;
    let root = builder
        .add_node(NULL_KIND, AccountedSemanticNode::Null, None, resources)
        .map_err(map_data)?;
    Ok((builder, root))
}

fn fresh_route_builder(_resources: &ResourceContext<'_>) -> Result<AccountedDocumentBuilder<'static>, CodecError> {
    let recipe = route_schema_recipe().map_err(map_data)?;
    let builder =
        AccountedDocumentBuilder::try_new_with_coverage(recipe.format(), recipe.dialect(), BuilderCoverage::complete())
            .map_err(map_data)?;
    Ok(builder)
}

fn locate_contract() -> CodecError {
    jqf_codec_core::data_contract("XML route built a subtree for a negative located shape")
}

/// Located publishes one document. A member (or slice) that hits several
/// children is a stream, so the scoped route declines and the binder's
/// whole-document floor plus engine navigation produce the items.
pub(crate) fn decline_located_range() -> CodecError {
    CodecError::new(CodecFailureKind::RequirementMismatch)
}

/// Builds the COUNT-SKELETON document from a measure parse: the
/// document element as an array of its direct children, each child ELEMENT as
/// a deferred container span (the child's validated source extent) and each
/// text/comment/PI child as a built leaf. Only the root element's NAME fact is
/// carried — a child element's descendant text is unknowable without parsing
/// its span, so the CONTENT fact is deliberately absent on the measure
/// document. The count consumer answers `length` over the root from the child
/// count; a decline falls back to the whole program, whose own reads
/// materialize whatever they touch.
pub(crate) fn build_measure_document(
    children: alloc::vec::Vec<crate::parse::MeasureChild>,
    resources: &mut ResourceContext<'_>,
) -> Result<(AccountedDocumentBuilder<'static>, NodeId), CodecError> {
    let recipe = xml_schema_recipe().map_err(map_data)?;
    let prototype = jqf_data::DocumentSchemaPrototype::try_new(&recipe).map_err(map_data)?;
    let (mut builder, schema) = prototype
        .try_new_builder_with_coverage(BuilderCoverage::complete())
        .map_err(map_data)?;
    // The span children must be materializable; the XML span reader re-parses
    // one child element's validated source text.
    builder.bind_span_materializer(&crate::lazy::XML_SPAN_MATERIALIZER);
    // The recipe's kind slots: ELEMENT_KIND is slot 0, TEXT/COMMENT/PI 1-3.
    let element_kind = schema
        .node_kind(0)
        .ok_or_else(|| jqf_codec_core::data_contract("XML measure schema has no element kind"))?;
    // Reserve the child ledger up front so the arenas grow once instead of
    // doubling (the count-gate's RSS ceiling is measured).
    let _ = builder.try_reserve(
        jqf_data::DocumentCapacity {
            nodes: children.len().saturating_add(1),
            occurrences: children.len(),
            ..jqf_data::DocumentCapacity::default()
        },
        resources,
    );
    let child_role = schema
        .occurrence_role(0)
        .ok_or_else(|| jqf_codec_core::data_contract("XML measure schema has no child role"))?;
    let root = builder
        .add_prepared_node(
            &schema,
            element_kind,
            jqf_data::PreparedSemanticNode::Array(child_role),
            resources,
        )
        .map_err(map_data)?;
    for child in children {
        let node = match child {
            crate::parse::MeasureChild::Element { start, end } => {
                let span = jqf_source::Span::try_new(
                    u32::try_from(start).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?,
                    u32::try_from(end).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?,
                )
                .ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?;
                // SAFETY: the measure parse validated the child's extent as
                // one complete element of this session's exact immutable
                // source authority.
                unsafe {
                    builder.add_prepared_bound_container_span_node(
                        &schema,
                        element_kind,
                        span,
                        jqf_data::ContainerSpanKind::Array,
                        resources,
                    )
                }
                .map_err(map_data)?
            }
            crate::parse::MeasureChild::Text(text) => {
                let text = builder.store_text(&text, resources).map_err(map_data)?;
                builder
                    .add_prepared_stored_string_node(
                        &schema,
                        schema
                            .node_kind(1)
                            .ok_or_else(|| jqf_codec_core::data_contract("XML measure schema has no text kind"))?,
                        text,
                        resources,
                    )
                    .map_err(map_data)?
            }
            crate::parse::MeasureChild::Comment(text) => {
                let text = builder.store_text(&text, resources).map_err(map_data)?;
                builder
                    .add_prepared_stored_string_node(
                        &schema,
                        schema
                            .node_kind(2)
                            .ok_or_else(|| jqf_codec_core::data_contract("XML measure schema has no comment kind"))?,
                        text,
                        resources,
                    )
                    .map_err(map_data)?
            }
            crate::parse::MeasureChild::ProcessingInstruction { target, data } => {
                let spelling = crate::value::pi_spelling(&target, &data);
                let text = builder.store_text(&spelling, resources).map_err(map_data)?;
                builder
                    .add_prepared_stored_string_node(
                        &schema,
                        schema
                            .node_kind(3)
                            .ok_or_else(|| jqf_codec_core::data_contract("XML measure schema has no PI kind"))?,
                        text,
                        resources,
                    )
                    .map_err(map_data)?
            }
        };
        builder
            .add_prepared_occurrence(&schema, LocalOwnerRef::Node(root), child_role, None, node, resources)
            .map_err(map_data)?;
    }
    Ok((builder, root))
}

#[cfg(test)]
mod measure_build_tests {
    use super::*;

    #[test]
    fn measure_build_makes_root_and_span_children() {
        let resources = ResourceContext::new(
            jqf_resource::RequestAccount::try_new(jqf_resource::ResourceLimits::new(
                u64::MAX,
                u64::MAX,
                u64::MAX,
                u64::MAX,
                u32::MAX,
            ))
            .expect("account"),
            &jqf_resource::ContinueControl,
            jqf_resource::WorkMeter::try_new_v1(4096).expect("work"),
        )
        .expect("resources");
        let mut resources = resources;
        let children = vec![
            crate::parse::MeasureChild::Element { start: 9, end: 22 },
            crate::parse::MeasureChild::Text(alloc::string::String::from("leaf")),
        ];
        // The build succeeds without a source binding; begin_finish needs the
        // bound seal (the spans reference the source) which the session's Seal
        // phase provides — the session test covers the full flow.
        let _ = build_measure_document(children, &mut resources).expect("build");
    }
}

#[cfg(test)]
mod coverage_tests {
    use super::{ATTRIBUTE_FACT, NAME_FACT};
    use jqf_codec_core::{
        AccessGuarantees, AccessOutcome, AccessRequirement, CodecDemand, CodecRunContext, DecodeRequest, DemandClause,
        DiagnosticPolicy, FactIntent, TopologyDemand, ValidationMode,
    };
    use jqf_data::{DialectId, DocumentCapability, ExpandedName, NodeId};
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
            "test.xml",
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
        let dialect = DialectId::try_new(crate::XML_DOCUMENT_DIALECT_ID).expect("dialect");
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

    #[test]
    fn identity_skips_attribute_facts_and_keeps_names() {
        let mut resources = resources();
        let requirement = whole_requirement(CodecDemand::try_new(&resources), &resources);
        let product = decode_requirement(INPUT, &requirement, &mut resources);
        assert!(
            attribute_pairs(&product).is_empty(),
            "identity must skip attribute facts"
        );
        assert!(has_name_fact(&product), "NAME_FACT is the XML value model");
        assert!(
            !product.document().coverage().contains(DocumentCapability::Topology),
            "identity JSON projection omits occurrence topology"
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
        assert!(has_name_fact(&product), "NAME_FACT is the XML value model");
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
