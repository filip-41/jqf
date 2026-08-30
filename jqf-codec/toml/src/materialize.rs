//! Document construction from a located subtree: the scoped route's exact value, plus the null product a negative
//! observation publishes.
//!
//! Both routes build from the parse-direct flat state. Located documents are copy mode: no source span, retained memory
//! bounded by the subtree.

use jqf_codec_core::CodecError;
use jqf_data::{
    AccountedDocumentBuilder, AccountedOccurrenceKey, AccountedSemanticNode, BuilderCoverage, LocalOwnerRef, NodeId,
};
use jqf_resource::ResourceContext;

use alloc::string::String;

use crate::grammar::{ChildKind, Key, TableTree, TextSource, Tree};
use crate::locate::Located;
use crate::parse::{ARRAY_ROLE, INLINE_ROLE, TABLE_ROLE, map_data};

/// Builds a fresh document whose root is the located subtree (or the null product a negative observation publishes),
/// returning the builder and root.
pub(crate) fn build_located_document(
    located: &Located<'_>,
    names: &[String],
    bytes: &[u8],
    coverage: BuilderCoverage,
    resources: &ResourceContext<'_>,
) -> Result<(AccountedDocumentBuilder<'static>, NodeId), CodecError> {
    let (mut builder, _) = crate::parse::fresh_builder(coverage, resources)?;
    let root = match located {
        Located::Value(value) => build_value(&mut builder, value, names, bytes, coverage, resources)?,
        Located::Table(table) => build_table(&mut builder, table, names, bytes, coverage, resources)?,
        Located::ArrayOfTables(elements) => {
            let array = add_array_node(&mut builder, resources)?;
            for element in *elements {
                let element_node = build_table(&mut builder, element, names, bytes, coverage, resources)?;
                add_item_occurrence(&mut builder, array, element_node, resources)?;
            }
            array
        }
        Located::Missing { .. } => add_null_node(&mut builder, resources)?,
    };
    Ok((builder, root))
}

/// Builds one table (root, child table, or array-of-tables element) into a fresh node: its members in authored order
/// — direct assignments first, then child tables/arrays in first-definition order — exactly the whole route's
/// member order.
fn build_table(
    builder: &mut AccountedDocumentBuilder<'_>,
    table: &TableTree,
    names: &[String],
    bytes: &[u8],
    coverage: BuilderCoverage,
    resources: &ResourceContext<'_>,
) -> Result<NodeId, CodecError> {
    let node = add_table_node(builder, resources)?;
    for (key, value) in &table.assignments {
        let value_node = build_value(builder, value, names, bytes, coverage, resources)?;
        add_member_occurrence(builder, node, key, names, value_node, resources)?;
    }
    for (key, child) in &table.children {
        let child_node = match child {
            ChildKind::Table(table) => build_table(builder, table, names, bytes, coverage, resources)?,
            ChildKind::ArrayOfTables(elements) => {
                let array = add_array_node(builder, resources)?;
                for element in elements {
                    let element_node = build_table(builder, element, names, bytes, coverage, resources)?;
                    add_item_occurrence(builder, array, element_node, resources)?;
                }
                array
            }
        };
        add_member_occurrence(builder, node, key, names, child_node, resources)?;
    }
    Ok(node)
}

/// Builds one exact value node from the located tree.
#[allow(
    clippy::too_many_lines,
    reason = "one semantic construction dispatch: each TOML scalar and container kind is a few lines of the same shape"
)]
fn build_value(
    builder: &mut AccountedDocumentBuilder<'_>,
    value: &Tree,
    names: &[String],
    bytes: &[u8],
    coverage: BuilderCoverage,
    resources: &ResourceContext<'_>,
) -> Result<NodeId, CodecError> {
    match value {
        Tree::Array { items, .. } => {
            let array = add_array_node(builder, resources)?;
            for item in items {
                let item_node = build_value(builder, item, names, bytes, coverage, resources)?;
                add_item_occurrence(builder, array, item_node, resources)?;
            }
            Ok(array)
        }
        Tree::InlineTable { entries, .. } => {
            let table = add_inline_table_node(builder, resources)?;
            for (key, entry) in entries {
                let entry_node = build_value(builder, entry, names, bytes, coverage, resources)?;
                add_inline_member_occurrence(builder, table, key, names, entry_node, resources)?;
            }
            Ok(table)
        }
        Tree::String(source) => {
            let text = match source {
                TextSource::Copied(text) => text.clone(),
                TextSource::Span(span) => {
                    // The located build is copy-mode: a verbatim string's text is copied here out of the validated
                    // source instead of naming the span, so the document needs no source seal.
                    let start = span.start() as usize;
                    let end = span.end() as usize;
                    String::from_utf8(bytes[start..end].to_vec()).expect("validated UTF-8")
                }
            };
            builder
                .add_node("toml.scalar@1", AccountedSemanticNode::String(&text), None, resources)
                .map_err(map_data)
        }
        Tree::Integer { value: number, .. } => {
            let text = alloc::format!("{number}");
            builder
                .add_node("toml.scalar@1", AccountedSemanticNode::Integer(&text), None, resources)
                .map_err(map_data)
        }
        Tree::Float(float, _span) => builder
            .add_node("toml.scalar@1", AccountedSemanticNode::Float(*float), None, resources)
            .map_err(map_data),
        Tree::Decimal(coefficient, scale, _span) => builder
            .add_node(
                "toml.scalar@1",
                AccountedSemanticNode::Decimal {
                    coefficient,
                    scale: *scale,
                },
                None,
                resources,
            )
            .map_err(map_data),
        Tree::Bool(value, _span) => builder
            .add_node("toml.scalar@1", AccountedSemanticNode::Bool(*value), None, resources)
            .map_err(map_data),
        Tree::LocalDate(date, ..) => builder
            .add_node(
                "toml.scalar@1",
                AccountedSemanticNode::LocalDate(*date),
                None,
                resources,
            )
            .map_err(map_data),
        Tree::LocalTime(time, ..) => builder
            .add_node(
                "toml.scalar@1",
                AccountedSemanticNode::LocalTime(time.as_ref()),
                None,
                resources,
            )
            .map_err(map_data),
        Tree::LocalDateTime(datetime, ..) => builder
            .add_node(
                "toml.scalar@1",
                AccountedSemanticNode::LocalDateTime(datetime.as_ref()),
                None,
                resources,
            )
            .map_err(map_data),
        Tree::OffsetDateTime(datetime, ..) => builder
            .add_node(
                "toml.scalar@1",
                AccountedSemanticNode::OffsetDateTime(datetime.as_ref()),
                None,
                resources,
            )
            .map_err(map_data),
        Tree::Commented { value, leading, inline } => {
            let node = build_value(builder, value, names, bytes, coverage, resources)?;
            crate::parse::attach_comments(builder, leading, inline, node, coverage.attached_facts(), resources)?;
            Ok(node)
        }
    }
}

fn add_table_node(
    builder: &mut AccountedDocumentBuilder<'_>,
    resources: &ResourceContext<'_>,
) -> Result<NodeId, CodecError> {
    builder
        .add_node(
            "toml.table@1",
            AccountedSemanticNode::Object {
                member_role: TABLE_ROLE,
            },
            None,
            resources,
        )
        .map_err(map_data)
}

fn add_inline_table_node(
    builder: &mut AccountedDocumentBuilder<'_>,
    resources: &ResourceContext<'_>,
) -> Result<NodeId, CodecError> {
    builder
        .add_node(
            "toml.inline-table@1",
            AccountedSemanticNode::Object {
                member_role: INLINE_ROLE,
            },
            None,
            resources,
        )
        .map_err(map_data)
}

fn add_array_node(
    builder: &mut AccountedDocumentBuilder<'_>,
    resources: &ResourceContext<'_>,
) -> Result<NodeId, CodecError> {
    builder
        .add_node(
            "toml.array@1",
            AccountedSemanticNode::Array { item_role: ARRAY_ROLE },
            None,
            resources,
        )
        .map_err(map_data)
}

fn add_null_node(
    builder: &mut AccountedDocumentBuilder<'_>,
    resources: &ResourceContext<'_>,
) -> Result<NodeId, CodecError> {
    builder
        .add_node("toml.scalar@1", AccountedSemanticNode::Null, None, resources)
        .map_err(map_data)
}

fn add_member_occurrence(
    builder: &mut AccountedDocumentBuilder<'_>,
    owner: NodeId,
    key: &Key,
    names: &[String],
    target: NodeId,
    resources: &ResourceContext<'_>,
) -> Result<(), CodecError> {
    builder
        .add_occurrence(
            LocalOwnerRef::Node(owner),
            TABLE_ROLE,
            Some(AccountedOccurrenceKey::Text(&names[key.id as usize])),
            target,
            resources,
        )
        .map_err(map_data)?;
    Ok(())
}

fn add_inline_member_occurrence(
    builder: &mut AccountedDocumentBuilder<'_>,
    owner: NodeId,
    key: &Key,
    names: &[String],
    target: NodeId,
    resources: &ResourceContext<'_>,
) -> Result<(), CodecError> {
    builder
        .add_occurrence(
            LocalOwnerRef::Node(owner),
            INLINE_ROLE,
            Some(AccountedOccurrenceKey::Text(&names[key.id as usize])),
            target,
            resources,
        )
        .map_err(map_data)?;
    Ok(())
}

fn add_item_occurrence(
    builder: &mut AccountedDocumentBuilder<'_>,
    owner: NodeId,
    target: NodeId,
    resources: &ResourceContext<'_>,
) -> Result<(), CodecError> {
    builder
        .add_occurrence(LocalOwnerRef::Node(owner), ARRAY_ROLE, None, target, resources)
        .map_err(map_data)?;
    Ok(())
}
