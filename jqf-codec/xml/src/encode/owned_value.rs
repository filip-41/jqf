//! Owned [`Value`] serialization for the deterministic encode profile.
//!
//! This product lowers an owned value into a synthetic XML-shaped document,
//! renders it through [`super::DeterministicSerializer`], and publishes the
//! full document frame with namespace rebinding and a trailing newline. It
//! does not retain or splice source bytes.

use alloc::borrow::ToOwned;

use jqf_codec_core::CodecError;
use jqf_data::{AccountedDocumentBuilder, AccountedSemanticNode, FactPayload, LocalOwnerRef, NodeId, Value};
use jqf_resource::ResourceContext;

use super::DeterministicSerializer;
use super::unsupported;
use super::value_mapping::{
    VALUE_ITEM_NAME, VALUE_ROOT_NAME, invalid_element_key_for_encode, valid_element_name, value_number_text,
};

/// Lowers one owned value into the element model and renders the deterministic
/// byte law (frame + trailing newline).
pub(super) fn serialize_owned_value(
    value: &Value,
    resources: &mut ResourceContext<'_>,
) -> Result<alloc::vec::Vec<u8>, CodecError> {
    let mut builder = crate::document::fresh_builder(resources)?;
    let root = build_value_element(&mut builder, VALUE_ROOT_NAME, value, resources)?;
    let document = builder.finish(root, resources).map_err(crate::document::map_data)?;
    let node = document.root_handle();
    let mut serializer = DeterministicSerializer::new();
    serializer.prepare(&document, resources)?;
    serializer.gather_node(&document, node, resources)?;
    serializer.assign_prefixes()?;
    let mut out = alloc::vec::Vec::new();
    serializer.render(&document, node, resources, &mut out)?;
    Ok(out)
}

/// Lowers one owned value into the XML element/attribute model (§1): returns
/// the root element node of a synthetic document built with the decoder's own
/// `AccountedDocumentBuilder` machinery.
///
/// The mapping is stated and lossy on purpose: a value carries no element
/// names, so object keys name their elements, array items get the fixed
/// `item` name, and the document element is the fixed `root` name. A key
/// that is not a valid XML `Name` (the decoder's own rule) is refused, never
/// renamed.
fn build_value_element(
    builder: &mut AccountedDocumentBuilder<'static>,
    name: &str,
    value: &Value,
    resources: &mut ResourceContext<'_>,
) -> Result<NodeId, CodecError> {
    let _depth = resources.enter_nesting_owned().map_err(CodecError::from)?;
    let id = builder
        .add_node(
            crate::document::ELEMENT_KIND,
            AccountedSemanticNode::Array {
                item_role: crate::document::CHILD_ROLE,
            },
            None,
            resources,
        )
        .map_err(crate::document::map_data)?;
    builder
        .add_fact(
            LocalOwnerRef::Node(id),
            crate::document::NAME_FACT,
            crate::document::NAME_FACT,
            1,
            &FactPayload::Text(name.to_owned()),
            resources,
        )
        .map_err(crate::document::map_data)?;
    match value {
        Value::Null => {}
        Value::Bool(boolean) => add_value_text(builder, id, if *boolean { "true" } else { "false" }, resources)?,
        Value::Number(number) => {
            let text = value_number_text(number)
                .ok_or_else(|| unsupported("a number with no canonical text cannot be represented as XML"))?;
            add_value_text(builder, id, &text, resources)?;
        }
        Value::String(text) => add_value_text(builder, id, text, resources)?,
        Value::Object(object) => {
            for entry in object {
                let key = entry.key();
                if !valid_element_name(key) {
                    return Err(invalid_element_key_for_encode(key));
                }
                let child = build_value_element(builder, key, entry.value(), resources)?;
                add_value_child(builder, id, child, resources)?;
            }
        }
        Value::Array(array) => {
            for item in array {
                let child = build_value_element(builder, VALUE_ITEM_NAME, item, resources)?;
                add_value_child(builder, id, child, resources)?;
            }
        }
        Value::Bytes(_)
        | Value::LocalDate(_)
        | Value::LocalTime(_)
        | Value::LocalDateTime(_)
        | Value::OffsetDateTime(_)
        | Value::Tagged { .. } => {
            return Err(unsupported(
                "a byte string, temporal, or tagged value has no XML element representation",
            ));
        }
    }
    Ok(id)
}

/// Adds one text-run child (a `xml.text@1` leaf) to an element node.
fn add_value_text(
    builder: &mut AccountedDocumentBuilder<'static>,
    parent: NodeId,
    text: &str,
    resources: &mut ResourceContext<'_>,
) -> Result<(), CodecError> {
    let child = builder
        .add_node(
            crate::document::TEXT_KIND,
            AccountedSemanticNode::String(text),
            None,
            resources,
        )
        .map_err(crate::document::map_data)?;
    add_value_child(builder, parent, child, resources)
}

/// Adds one child occurrence to an element node.
fn add_value_child(
    builder: &mut AccountedDocumentBuilder<'static>,
    parent: NodeId,
    child: NodeId,
    resources: &mut ResourceContext<'_>,
) -> Result<(), CodecError> {
    builder
        .add_occurrence(
            LocalOwnerRef::Node(parent),
            crate::document::CHILD_ROLE,
            None,
            child,
            resources,
        )
        .map_err(crate::document::map_data)?;
    Ok(())
}
