//! Reading a deferred XML child-element span back into a value.
//!
//! The count-skeleton document defers each child ELEMENT of the
//! document element as a container span; this is the reader that turns a
//! child's validated source text back into an owned value when something
//! touches it. The span is one complete element — a standalone XML document —
//! so the ordinary parser rebuilds it exactly as the whole-document route
//! would; the value is materialized through the same build-and-finish path.
//!
//! Text/comment/PI children are never spans: the measure build materializes
//! them as built leaves, so this reader only ever sees element spans.

use jqf_codec_core::{CodecError, map_span_materialization_error};
use jqf_data::{DataError, LazySpanMaterializer, Value};
use jqf_resource::ResourceContext;

use crate::document;
use crate::parse::{ParseOutput, ParsePoll, XmlParseState};

/// The XML reader for deferred element spans.
pub(crate) struct XmlSpanMaterializer;

/// The one installed reader.
pub(crate) static XML_SPAN_MATERIALIZER: XmlSpanMaterializer = XmlSpanMaterializer;

impl LazySpanMaterializer for XmlSpanMaterializer {
    fn materialize_span(&self, text: &str, resources: &mut ResourceContext<'_>) -> Result<Value, DataError> {
        materialize(text, resources).map_err(|error| map_span_materialization_error(&error))
    }
}

fn materialize(text: &str, resources: &mut ResourceContext<'_>) -> Result<Value, CodecError> {
    // The parameter is already `&str`, and the measure parse that recorded
    // this span already validated these exact bytes with the identical
    // grammar. Skip the redundant whole-slice UTF-8 check.
    let mut parse = XmlParseState::try_new_prevalidated(text)?.without_spans();
    let output = loop {
        match parse.poll(text.as_bytes(), resources)? {
            ParsePoll::Pending => {
                resources.try_begin_next_cooperative_entry(1)?;
            }
            ParsePoll::Ready(output) => break output,
        }
    };
    let ParseOutput::Tree(tree) = output else {
        return Err(jqf_codec_core::data_contract(
            "XML span materializer received a non-tree parse",
        ));
    };
    // The re-parse builds a TEMPORARY document whose only consumer is this
    // materialized owned value; spans are not bound (a document with
    // unsealed spans cannot finalize without a source authority, and
    // nothing would consume them here).
    let (builder, root) = document::build_document(&tree, resources, false)?;
    let mut finalizer = builder.begin_finish(root, resources).map_err(document::map_data)?;
    let document = loop {
        match finalizer.poll(resources).map_err(document::map_data)? {
            jqf_data::DocumentFinalizationPoll::Pending => {
                resources.try_begin_next_cooperative_entry(1)?;
            }
            jqf_data::DocumentFinalizationPoll::Ready(document) => break document,
        }
    };
    document.materialize_root(resources).map_err(crate::document::map_data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reparse_of_span_is_owned() {
        let span = r#"<item id="0" name="item-000000" color="red"/>"#;
        // The span reader builds through the ordinary session path; the
        // materialized value is the element's array-of-children shape (an
        // empty element is an empty array).
        let resources = jqf_resource::ResourceContext::new(
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
        let value = materialize(span, &mut resources).expect("materializes");
        assert!(matches!(value.kind(), jqf_data::ValueKind::Array));
    }
}
