//! `render.terminal@1`: terminal-safe styled text.
//!
//! The terminal renderer shares its frame boundary with its selected Plain/Table/Tree text shape and applies the
//! terminal escaping law so no source scalar can inject a terminal control. In the v1 encoder surface the frame is
//! BYTES — the concatenation of the styled spans, with styles dropped — because the `EncoderSession` ABI carries
//! bytes only. The span roles (`text`, `border`, `header`, `key`, `index`, `string`, `number`, `boolean`, `null`,
//! `bytes`, `temporal`, `tag`, `diagnostic`) are the documented future styled-span ABI; the byte law they concatenate
//! to is implemented here today.

use alloc::string::String;

use jqf_codec_core::{CodecError, TagLayer, project_tag, value_tag_layer};
use jqf_data::Value;
use jqf_resource::ResourceContext;

use super::error::unsupported;
use super::options::RenderEncodeOptions;
use super::scalar::{StringStyle, write_scalar};
use crate::table::TableRenderer;
use crate::tree;

/// Renders one item under the terminal renderer's bound shape.
///
/// # Errors
///
/// Returns an `UnsupportedShape` reject for an item the bound shape cannot present, an allocation failure, or an
/// internal-contract error.
pub(crate) fn render(
    value: &Value,
    options: RenderEncodeOptions,
    resources: &ResourceContext<'_>,
) -> Result<String, CodecError> {
    match options.terminal_shape {
        super::options::TerminalShape::Plain => render_plain(value, resources),
        super::options::TerminalShape::Table => super::table::render(value, TableRenderer::Grid, options, resources),
        super::options::TerminalShape::Tree => tree::render(value),
    }
}

/// The terminal Plain shape: one core scalar with terminal-safe text.
///
/// A non-core tag has no terminal spelling, so it publishes its payload under the shared publish law and records the
/// event.
fn render_plain(value: &Value, resources: &ResourceContext<'_>) -> Result<String, CodecError> {
    if let TagLayer::Tagged(_) = value_tag_layer(value) {
        project_tag(resources);
    }
    match value.untagged() {
        Value::Array(_) | Value::Object(_) => Err(unsupported(
            "terminal-shape",
            "the terminal Plain shape renders scalars only; use render-shape table or tree for containers",
        )),
        _ => {
            let mut out = String::new();
            write_scalar(&mut out, value.untagged(), StringStyle::Terminal)?;
            Ok(out)
        }
    }
}
