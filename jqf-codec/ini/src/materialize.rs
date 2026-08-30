//! Materializing the span skeleton into a document.
//!
//! Every entry's COOKED value becomes a core `String` member (strings only, always — no type inference), attached to
//! the root object — or, under the INI grammar, to the one legal nesting level of section objects. Leading comment
//! lines attach as the dialect's comment fact on the VALUE node, and every value's authored span is recorded.
//!
//! Duplicate keys follow the object-builder law: the first duplicate fixes position and the final occurrence supplies
//! the value. See CONTRACTS.md.
//!
//! Member ORDER is canonical, not authored: every section object attaches to the root before any root-level scalar
//! regardless of file position, and the encoder re-emits root scalars first (then sections) — so decode → encode is
//! stable even where the decoded key order differs from the file's.

use alloc::string::String;
use alloc::vec::Vec;

use jqf_codec_core::CodecError;
use jqf_data::{
    AccountedDocumentBuilder, AccountedOccurrenceKey, AuthoritativeEmptyFamilies, BuilderCoverage, DataError,
    DiagnosticCoverage, DocumentCapabilityFamily, DocumentSchemaRecipe, FactPayload, LocalOwnerRef, NodeId,
    PreparedDocumentSchema, PreparedNodeKind, PreparedOccurrenceRole, PreparedSemanticNode,
};
use jqf_resource::ResourceContext;
use jqf_source::Span;

use crate::options::Grammar;
use crate::scan::Skeleton;

/// The two node kinds of one grammar's document (object first, then string).
fn node_kinds(grammar: Grammar) -> &'static [&'static str] {
    match grammar {
        Grammar::Properties => &["properties.object@1", "properties.string@1"],
        Grammar::Ini => &["ini.object@1", "ini.string@1"],
        Grammar::Dotenv => &["dotenv.object@1", "dotenv.string@1"],
    }
}

/// The member occurrence role of one grammar's document.
fn member_role(grammar: Grammar) -> &'static [&'static str] {
    match grammar {
        Grammar::Properties => &["properties.member@1"],
        Grammar::Ini => &["ini.member@1"],
        Grammar::Dotenv => &["dotenv.member@1"],
    }
}

/// The comment-fact identities of one grammar's document (leading + foot).
fn comment_facts(grammar: Grammar) -> &'static [&'static str] {
    match grammar {
        Grammar::Properties => &["properties.comment@1", "properties.comment_foot@1"],
        Grammar::Ini => &["ini.comment@1", "ini.comment_foot@1"],
        Grammar::Dotenv => &["dotenv.comment@1", "dotenv.comment_foot@1"],
    }
}

/// The ROOT's foot-comment fact identity of one grammar's document (the document trailer, the FOOT position).
pub(crate) fn comment_foot_kind(grammar: Grammar) -> &'static str {
    match grammar {
        Grammar::Properties => "properties.comment_foot@1",
        Grammar::Ini => "ini.comment_foot@1",
        Grammar::Dotenv => "dotenv.comment_foot@1",
    }
}

fn schema_recipe(grammar: Grammar) -> Result<DocumentSchemaRecipe<'static>, DataError> {
    DocumentSchemaRecipe::try_new(
        grammar.format_id(),
        Some(grammar.input_dialect_id()),
        node_kinds(grammar),
        member_role(grammar),
        // The fact-kind/role lists name every fact the builder may attach: the leading comment fact (per entry) and the
        // root's foot-comment fact (the document trailer).
        comment_facts(grammar),
        comment_facts(grammar),
    )
}

/// The prepared node-kind and occurrence-role handles of one document.
fn resolve_handles(
    schema: &PreparedDocumentSchema,
) -> Result<(PreparedNodeKind, PreparedNodeKind, PreparedOccurrenceRole), CodecError> {
    Ok((
        schema.node_kind(0).ok_or_else(data_contract)?,
        schema.node_kind(1).ok_or_else(data_contract)?,
        schema.occurrence_role(0).ok_or_else(data_contract)?,
    ))
}

/// Copies one owned string into the builder's stored text.
fn add_text(
    builder: &mut AccountedDocumentBuilder<'static>,
    text: &str,
    resources: &mut ResourceContext<'_>,
) -> Result<jqf_data::DocumentTextId, CodecError> {
    let mut stage = builder.begin_text(resources).map_err(map_data)?;
    builder.append_text(&mut stage, text, resources).map_err(map_data)?;
    builder.finish_text(stage, resources).map_err(map_data)
}

/// Records one authored source span on the builder: the semantic is stored exactly as without the span; the span only
/// names the authored bytes the edit lane echoes verbatim or replaces.
#[allow(
    unsafe_code,
    reason = "the late-sealing span contract is an unsafe fn by jqf-data's design; the spans were produced by this codec's own scan over the exact session-owned source authority"
)]
fn record_authored_span(
    builder: &mut AccountedDocumentBuilder<'static>,
    node: NodeId,
    span: Span,
    resources: &mut ResourceContext<'_>,
) -> Result<(), CodecError> {
    // SAFETY: the span was produced by this codec's own scan over the exact
    // source authority the session binds to the builder, and its bytes are the validated UTF-8 of the authored
    // key/value region — the `record_authored_span` contract.
    unsafe { builder.record_authored_span(node, span, resources) }.map_err(map_data)
}

/// Attaches one comment run as the grammar's comment fact on `owner` (list-of-text, exactly like `toml.comment@1`).
/// `role` names the position's fact identity: the leading `comment@1` for an entry's own run, the foot `comment_foot@1`
/// for the root's document trailer.
fn attach_comments(
    builder: &mut AccountedDocumentBuilder<'static>,
    role: &str,
    comments: &[String],
    owner: NodeId,
    attach_facts: bool,
    resources: &mut ResourceContext<'_>,
) -> Result<(), CodecError> {
    if !attach_facts || comments.is_empty() {
        return Ok(());
    }
    let payload = FactPayload::List(comments.iter().map(|text| FactPayload::Text(text.clone())).collect());
    builder
        .add_fact(LocalOwnerRef::Node(owner), role, role, 1, &payload, resources)
        .map_err(map_data)?;
    Ok(())
}

/// Builds the complete document from the validated skeleton.
///
/// `spans_committed` is always true here — the span receipts are a standing law of this codec, so every document
/// carries authored spans and must seal its source before finishing.
pub(crate) fn build_document(
    skeleton: &Skeleton,
    grammar: Grammar,
    coverage: BuilderCoverage,
    resources: &mut ResourceContext<'_>,
) -> Result<(AccountedDocumentBuilder<'static>, NodeId), CodecError> {
    let recipe = schema_recipe(grammar).map_err(map_data)?;
    let (mut builder, schema) =
        AccountedDocumentBuilder::try_new_prepared_with_coverage(&recipe, coverage).map_err(map_data)?;
    builder.set_authoritative_empty_families(AuthoritativeEmptyFamilies::from_family(
        DocumentCapabilityFamily::Attributes,
    ));
    builder.set_diagnostic_coverage(DiagnosticCoverage::NotRequested);
    let (object, string, member) = resolve_handles(&schema)?;
    let root = builder
        .add_prepared_node(&schema, object, PreparedSemanticNode::Object(member), resources)
        .map_err(map_data)?;
    // The ROOT carries the whole document's authored span — the edit lane's trailer-write anchor (the FOOT arm walks
    // from the root's span end to the last content line). Recorded FIRST because the span table must stay in node order
    // and the root is node zero.
    let root_span = jqf_source::Span::try_from_usize(0, skeleton.source_len)
        .map_err(|_| CodecError::new(jqf_codec_core::CodecFailureKind::Overflow))?;
    record_authored_span(&mut builder, root, root_span, resources)?;
    // INI sections: one object node per section, in first-definition order.
    let mut section_nodes: Vec<NodeId> = Vec::with_capacity(skeleton.sections.len());
    for section in &skeleton.sections {
        let node = builder
            .add_prepared_node(&schema, object, PreparedSemanticNode::Object(member), resources)
            .map_err(map_data)?;
        record_authored_span(&mut builder, node, section.header_span, resources)?;
        builder
            .add_prepared_occurrence(
                &schema,
                LocalOwnerRef::Node(root),
                member,
                Some(AccountedOccurrenceKey::Text(section.name.as_str())),
                node,
                resources,
            )
            .map_err(map_data)?;
        section_nodes.push(node);
    }
    for entry in &skeleton.entries {
        let owner = match entry.section {
            Some(index) => section_nodes[index as usize],
            None => root,
        };
        let text = add_text(&mut builder, &entry.value, resources)?;
        let value_node = builder
            .add_prepared_stored_string_node(&schema, string, text, resources)
            .map_err(map_data)?;
        record_authored_span(&mut builder, value_node, entry.value_span, resources)?;
        attach_comments(
            &mut builder,
            comment_facts(grammar)[0],
            &entry.leading_comments,
            value_node,
            coverage.attached_facts(),
            resources,
        )?;
        builder
            .add_prepared_occurrence(
                &schema,
                LocalOwnerRef::Node(owner),
                member,
                Some(AccountedOccurrenceKey::Text(entry.key.as_str())),
                value_node,
                resources,
            )
            .map_err(map_data)?;
    }
    // The document trailer: comment lines after the last entry attach to the ROOT as its foot-comment fact (the
    // ownership-precedence rule — no next node follows, so the whole document owns them). The encoder re-emits the
    // trailer after the body so a trailing comment survives a re-encode.
    attach_comments(
        &mut builder,
        comment_foot_kind(grammar),
        &skeleton.trailing_comments,
        root,
        coverage.attached_facts(),
        resources,
    )?;
    Ok((builder, root))
}

fn map_data(error: DataError) -> CodecError {
    jqf_codec_core::map_data(error, "flat-config builder rejected document construction")
}

fn data_contract() -> CodecError {
    jqf_codec_core::data_contract("flat-config authoritative document construction")
}
