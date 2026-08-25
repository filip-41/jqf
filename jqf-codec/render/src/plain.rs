//! `render.plain@1`: one UTF-8 frame per core scalar item.
//!
//! Every projectable scalar is spelled NATIVELY here — bytes and all four temporals have their own formatter text —
//! so the shared encode-projection layer has nothing to project in this shape. A non-core TAG has no spelling in plain
//! text, so it publishes its payload under the shared publish law and records the event.

use alloc::string::String;

use jqf_codec_core::{CodecError, TagLayer, project_tag, value_tag_layer};
use jqf_data::Value;
use jqf_resource::ResourceContext;

use super::error::unsupported;
use super::scalar::{StringStyle, write_scalar};

/// Renders one item as plain text.
///
/// The frame is the scalar's formatter text with strings UNQUOTED; the facade appends the trailing LF and no BOM. A
/// non-core tag publishes its payload; arrays and objects return `UnsupportedShape` before any byte of this item's
/// frame is published.
///
/// # Errors
///
/// Returns an `UnsupportedShape` reject for a non-scalar item, an allocation failure, or an internal-contract error.
pub(crate) fn render(value: &Value, resources: &ResourceContext<'_>) -> Result<String, CodecError> {
    if let TagLayer::Tagged(_) = value_tag_layer(value) {
        project_tag(resources);
    }
    match value.untagged() {
        Value::Array(_) | Value::Object(_) => Err(unsupported(
            "plain-shape",
            "render.plain@1 renders scalars only, not containers",
        )),
        _ => {
            let mut out = String::new();
            write_scalar(&mut out, value.untagged(), StringStyle::Raw)?;
            Ok(out)
        }
    }
}
