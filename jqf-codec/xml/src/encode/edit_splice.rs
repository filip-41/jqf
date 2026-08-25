//! Source-preserving edit-splice rendering for the `--edit` lane.
//!
//! This product renders owned values as bare byte fragments at retained source
//! spans: leaf text/attribute escapes, child growth before an end tag, comment
//! replacement, and attribute insertion. It does not build a synthetic
//! document or run [`super::DeterministicSerializer`]; child growth uses the
//! §1 value-mapping grammar via [`render_splice_children`] only.

use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::String;

use jqf_codec_core::{CodecError, CodecFailureKind, EditInsertion, EditRemoval, EditRemoveMembers};
use jqf_data::{BatchLimit, Document, LocalOwnerRef, NodeId, ReaderPoll, TopologyBatch, Value, ValueKind};
use jqf_resource::ResourceContext;

use crate::document::COMMENT_KIND;

use super::value_mapping::{VALUE_ITEM_NAME, invalid_element_key_for_splice, valid_element_name, value_number_text};
use super::{escape_attr, escape_text, unsupported};

/// The edit lane's LEAF seam: renders one owned
/// value exactly as it must appear at the patch site's POSITION. The
/// position comes from the node's own semantic and the authored bytes:
/// an attribute write passes the element with the fact's quoted-value span,
/// so the render wraps the value in the authored quote character; a TEXT
/// leaf is a string node and renders as character data. The authored bytes
/// select the quote style (`'…'` stays single-quoted); the escape set is
/// fixed per position (an attribute value escapes `"` and the whitespace
/// characters, character data does not).
pub(super) fn render_leaf(
    document: &Document<'_>,
    node: NodeId,
    value: &Value,
    authored: Option<&[u8]>,
) -> Result<alloc::vec::Vec<u8>, CodecError> {
    let handle = document.node_handle(node).map_err(crate::document::map_data)?;
    let view = document.value_view(handle).map_err(crate::document::map_data)?;
    let text = leaf_text(value)?;
    let mut out = alloc::vec::Vec::new();
    let kind = view.kind().map_err(crate::document::map_data)?;
    // Attribute position: a Null addressing node (historical) or an element
    // whose authored span is the quoted value bytes the fact now carries.
    let quote = attribute_quote(kind, authored);
    if let Some(quote) = quote {
        out.push(quote);
        escape_attr(&text, &mut out);
        out.push(quote);
    } else {
        escape_text(&text, &mut out);
    }
    Ok(out)
}

/// The quote character of an attribute value span (`"value"` / `'value'`).
fn attribute_quote(kind: ValueKind, authored: Option<&[u8]>) -> Option<u8> {
    let quote = authored.and_then(|bytes| {
        let first = *bytes.first()?;
        if (first == b'"' || first == b'\'') && bytes.last() == Some(&first) && bytes.len() >= 2 {
            Some(first)
        } else {
            None
        }
    });
    match kind {
        ValueKind::Null => Some(quote.unwrap_or(b'"')),
        ValueKind::Array => quote,
        _ => None,
    }
}

/// The canonical text of a leaf VALUE: a string itself, a boolean's
/// `true`/`false`, a number's canonical text. XML leaves hold text, so this
/// is the only value→text mapping the leaf seam serves; bytes, temporals,
/// and tagged values are unrepresentable in a leaf position.
fn leaf_text(value: &Value) -> Result<String, CodecError> {
    match value {
        Value::String(text) => Ok(text.as_str().to_owned()),
        Value::Bool(boolean) => Ok(if *boolean { "true" } else { "false" }.to_owned()),
        Value::Number(number) => value_number_text(number)
            .ok_or_else(|| unsupported("a number with no canonical text cannot be spliced into XML")),
        Value::Bytes(_)
        | Value::LocalDate(_)
        | Value::LocalTime(_)
        | Value::LocalDateTime(_)
        | Value::OffsetDateTime(_)
        | Value::Tagged { .. }
        | Value::Null
        | Value::Array(_)
        | Value::Object(_) => Err(unsupported(
            "a byte string, temporal, tagged, null, or container value has no XML leaf text",
        )),
    }
}

/// The edit lane's GROWTH splice: renders added children with
/// [`render_splice_children`] and splices them immediately before the element's
/// end tag. The splice is
/// a zero-length insertion at the end tag's start; each added child carries
/// the whitespace run observed between the last child and the end tag, so
/// the file's own layout survives and the pre-existing run separates the
/// first added child from the last existing one.
pub(super) fn render_edit_append(
    document: &Document<'_>,
    container: NodeId,
    source: &[u8],
    items: &[&Value],
) -> Result<alloc::vec::Vec<EditInsertion>, CodecError> {
    let handle = document.node_handle(container).map_err(crate::document::map_data)?;
    // A self-closing element (`<a/>`) has no end tag an insertion could
    // precede: removing the `/>` is a replacement, not an insertion, so the
    // splice declines and the whole-document floor rewrites the element in
    // explicit form.
    let Some(span) = document
        .node_source_span(container)
        .map_err(crate::document::map_data)?
    else {
        return Ok(alloc::vec::Vec::new());
    };
    let start = span.start() as usize;
    let end = span.end() as usize;
    if end >= 2 && source.get(end - 2) == Some(&b'/') && source.get(end - 1) == Some(&b'>') {
        return Ok(alloc::vec::Vec::new());
    }
    // The end tag's `<` is the LAST `<` in the element's extent (nothing
    // follows the end tag inside the element).
    let Some(relative) = source[start..end].iter().rposition(|byte| *byte == b'<') else {
        return Ok(alloc::vec::Vec::new());
    };
    let end_tag = start + relative;
    // A scalar appended after a TEXT leaf would MERGE into it
    // (`<a>x</a>` + `y` is the single text `xy`, never two children), so the
    // splice declines unless the preceding child is an element (or the
    // element is empty). An element or array member never merges and is
    // always safe. The merge check reads the element's LAST child's kind.
    // Note there is deliberately NO separator handling: whitespace before
    // the end tag is itself a text child in the value model (XML keeps it),
    // so the splice inserts exactly the rendered children — any separator
    // byte would become an extra text child and fail verification.
    let first_is_scalar = items
        .first()
        .is_some_and(|item| matches!(item, Value::String(_) | Value::Number(_) | Value::Bool(_)));
    if first_is_scalar {
        let view = document.value_view(handle).map_err(crate::document::map_data)?;
        if let Some(array) = view.array().map_err(crate::document::map_data)?
            && let Some(last) = array.iter().last()
            && last.kind().map_err(crate::document::map_data)? == ValueKind::String
        {
            return Ok(alloc::vec::Vec::new());
        }
    }
    let mut bytes: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    for item in items {
        render_splice_children(&mut bytes, item)?;
    }
    Ok(alloc::vec::Vec::from([EditInsertion {
        at: end_tag,
        bytes,
        replace: None,
    }]))
}

/// The edit lane's SHRINK cuts: each removed
/// child's OWN extent span only. Adjacent whitespace is a separate text
/// child in the value model — the deleting program's output keeps it, so a
/// splice that cut it would fail verification. A removed comment or
/// processing-instruction child has no bound span and declines to the
/// whole-document floor.
pub(super) fn render_edit_remove(
    document: &Document<'_>,
    members: EditRemoveMembers<'_>,
) -> Result<alloc::vec::Vec<EditRemoval>, CodecError> {
    let mut removals: alloc::vec::Vec<EditRemoval> = alloc::vec::Vec::new();
    for node in members.nodes() {
        let Some(span) = document.node_source_span(node).map_err(crate::document::map_data)? else {
            return Ok(alloc::vec::Vec::new());
        };
        removals.push(EditRemoval {
            start: span.start() as usize,
            end: span.end() as usize,
            replacement: alloc::vec::Vec::new(),
        });
    }
    Ok(removals)
}

/// Inserts a missing attribute immediately before the start-tag close.
///
/// The element's extent starts at its `<`. The walk uses the same quote
/// rules as `read_attribute`: a `"` or `'` opens a quoted run, and the
/// matching quote closes it. The first out-of-quote `>` or `/>` is the
/// terminator; the splice is ` name="escaped"` at that offset (before
/// `/` on a self-closing tag). Existing attributes stay on the leaf
/// path — a name already present here is a rewrite, not an insert.
pub(super) fn render_attribute_insert(
    document: &Document<'_>,
    node: NodeId,
    source: &[u8],
    kind: &str,
    payload: &Value,
    resources: &mut ResourceContext<'_>,
) -> Result<Option<alloc::vec::Vec<jqf_codec_core::FactEditPatch>>, CodecError> {
    let Value::String(text) = payload else {
        return Err(unsupported(
            "an attribute write must be a single text value (use null only to \
             delete, which this splice policy does not name)",
        ));
    };
    insertable_attribute_name(kind)?;
    if element_has_attribute(document, node, kind, resources)? {
        return Err(unsupported(&format!(
            "the element already has an attribute named {kind}"
        )));
    }
    let Some(span) = document.node_source_span(node).map_err(crate::document::map_data)? else {
        return Err(unsupported(
            "this element has no retained source span, so a new attribute cannot be inserted",
        ));
    };
    let start = span.start() as usize;
    let end = span.end() as usize;
    let Some(at) = start_tag_terminator(source, start, end) else {
        return Err(unsupported("the element's start tag has no close to insert before"));
    };
    let mut replacement = alloc::vec![b' '];
    replacement.extend_from_slice(kind.as_bytes());
    replacement.extend_from_slice(b"=\"");
    escape_attr(text.as_str(), &mut replacement);
    replacement.push(b'"');
    Ok(Some(alloc::vec::Vec::from([jqf_codec_core::FactEditPatch {
        start: at,
        end: at,
        replacement,
    }])))
}

/// One unprefixed XML Name: no `:`, no `xmlns`, no clark `{uri}local`.
/// A name that is already present is a rewrite, not an insert.
pub(super) fn insertable_attribute_name(name: &str) -> Result<(), CodecError> {
    if name.starts_with('{') || name.contains(':') {
        return Err(unsupported(
            "a namespaced attribute cannot be inserted; the write accepts an unprefixed XML Name",
        ));
    }
    if name == "xmlns" {
        return Err(unsupported("xmlns cannot be inserted as an attribute"));
    }
    if !valid_unprefixed_name(name) {
        return Err(unsupported(&format!(
            "the attribute name {name} is not a valid XML Name"
        )));
    }
    Ok(())
}

/// An XML `Name` that is not a `QName` (`:` is a `NameStartChar`; the insert
/// law excludes it).
fn valid_unprefixed_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first != ':' && crate::parse::is_name_start(first) && chars.all(|c| c != ':' && crate::parse::is_name_char(c))
}

/// Offset of the start-tag terminator: the first out-of-quote `>` or the
/// `/` of `/>`. The insert lands immediately before this byte. Quote
/// state matches `read_attribute`: only the matching quote closes a run.
pub(super) fn start_tag_terminator(source: &[u8], start: usize, end: usize) -> Option<usize> {
    if start >= end || start >= source.len() || source[start] != b'<' {
        return None;
    }
    let limit = end.min(source.len());
    let mut quote: Option<u8> = None;
    let mut i = start + 1;
    while i < limit {
        let c = source[i];
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            b'"' | b'\'' => {
                quote = Some(c);
                i += 1;
            }
            b'>' => return Some(i),
            b'/' if source.get(i + 1) == Some(&b'>') => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// Whether `name` is already an attribute fact on `element`. Unprefixed
/// attributes store their local name as the kind, so a hit means rewrite.
fn element_has_attribute(
    document: &Document<'_>,
    element: NodeId,
    name: &str,
    resources: &mut ResourceContext<'_>,
) -> Result<bool, CodecError> {
    if document.fact_owner_indexed() {
        for fact_id in document.owner_fact_ids(element) {
            let fact = document.fact(*fact_id).map_err(crate::document::map_data)?;
            if fact.role().as_str() == jqf_codec_core::markup::ATTRIBUTE_FACT && fact.kind().as_str() == name {
                return Ok(true);
            }
        }
        return Ok(false);
    }
    let owner = LocalOwnerRef::Node(element);
    let limit = BatchLimit::new(usize::MAX).ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?;
    let mut reader = match document.fact_reader(resources) {
        Ok(reader) => reader,
        Err(jqf_data::DataError::CapabilityUnavailable {
            capability: jqf_data::DocumentCapability::AttachedFacts,
        }) => return Ok(false),
        Err(error) => return Err(crate::document::map_data(error)),
    };
    loop {
        let poll = reader.poll_batch(limit, resources).map_err(crate::document::map_data)?;
        match poll {
            ReaderPoll::Batch(batch) => {
                for fact in batch.iter() {
                    if fact.owner() == owner
                        && fact.role().as_str() == jqf_codec_core::markup::ATTRIBUTE_FACT
                        && fact.kind().as_str() == name
                    {
                        return Ok(true);
                    }
                }
            }
            ReaderPoll::Pending => {
                resources
                    .try_begin_next_cooperative_entry(4_096)
                    .map_err(CodecError::from)?;
            }
            ReaderPoll::End(_) => break,
        }
    }
    Ok(false)
}

/// The XML comment-write splice — XML's own comment-bytes
/// renderer, beside the seam's shared `#`-line renderer, reached through
/// [`EncoderFactoryImpl::render_fact_delta`]'s HEAD comment role (the one
/// role-carrying fact-write seam). XML comments are `<!-- … -->`
/// children of an element, and `.@comment` reads exactly those
/// children, so a comment WRITE replaces them with `<!-- line -->`
/// comments, each replacing its child's authored span.
///
/// The value-identity LAW is the one constraint the `#`-line formats do not
/// have: an XML comment child is a VALUE child (a member of the element's
/// mixed-content array), so a write whose lines differ from the element's
/// existing comment children — a changed text, an added comment, a deleted
/// comment — changes the document VALUE, which the fact-write lane forbids
/// (it is value-identity by construction: a whole re-encode would re-emit
/// the OLD facts, so a fact write is exact or it is an error). The splice
/// therefore serves the ROUND-TRIP write — the payload's lines equal the
/// element's comment children, so the re-rendered bytes are byte-identical
/// and the value is unchanged — and refuses with prose anything that would
/// change the value, rather than hand the caller a patch that can never
/// verify. A `null`/empty payload over a comment-less element is a no-op
/// (`Some` of an empty vec; the value is unchanged and verification
/// passes).
pub(super) fn render_comment_write(
    document: &Document<'_>,
    node: NodeId,
    _source: &[u8],
    payload: &Value,
    resources: &mut ResourceContext<'_>,
) -> Result<Option<alloc::vec::Vec<jqf_codec_core::FactEditPatch>>, CodecError> {
    let lines = comment_payload_lines(payload)?;
    let (texts, spans) = comment_children(document, node, resources)?;
    if lines.is_empty() {
        if texts.is_empty() {
            // Nothing to write and nothing to remove: no patches, and the
            // value is unchanged, so verification passes on the original
            // bytes.
            return Ok(Some(alloc::vec::Vec::new()));
        }
        return Err(unsupported(
            "a comment write that deletes the element's comment children would change the \
             document value (XML comments are value children), which the value-identity edit \
             lane cannot splice; the fact-write lane is exact or it is an error",
        ));
    }
    if texts.is_empty() {
        return Err(unsupported(
            "a comment write that adds comment children would change the document value (XML \
             comments are value children), which the value-identity edit lane cannot splice",
        ));
    }
    if lines != texts {
        return Err(unsupported(
            "a comment write whose lines differ from the element's existing comment children \
             would change the document value (XML comments are value children), which the \
             value-identity edit lane cannot splice; write back the read value \
             (`.@comment = .@comment`) to round-trip the comments in place",
        ));
    }
    let mut removals: alloc::vec::Vec<jqf_codec_core::FactEditPatch> = alloc::vec::Vec::new();
    for (line, (start, end)) in lines.iter().zip(spans.iter()) {
        removals.push(jqf_codec_core::FactEditPatch {
            start: *start,
            end: *end,
            replacement: render_comment_bytes(line),
        });
    }
    Ok(Some(removals))
}

/// The comment payload's text lines, in the seam's canonical shape: a list
/// of strings, or `null`/empty for a deletion. The engine's fact-write
/// normalization already splits strings on line boundaries, so each item is
/// one line.
fn comment_payload_lines(payload: &Value) -> Result<alloc::vec::Vec<String>, CodecError> {
    match payload {
        Value::Null => Ok(alloc::vec::Vec::new()),
        Value::Array(items) => {
            let mut lines: alloc::vec::Vec<String> = alloc::vec::Vec::new();
            for item in items {
                let Value::String(text) = item else {
                    return Err(unsupported("a comment fact payload must be a list of text lines"));
                };
                lines.push(String::from(text.as_str()));
            }
            Ok(lines)
        }
        _ => Err(unsupported(
            "a comment fact payload must be a list of text lines (or null to delete)",
        )),
    }
}

/// Renders one comment text as its authored `<!-- … -->` bytes. The text
/// came from the decoder (which rejects `--` and a trailing `-`), so the
/// guard is defensive: a comment with an internal `--` is not expressible
/// in XML, exactly as the deterministic serializer refuses it.
fn render_comment_bytes(text: &str) -> alloc::vec::Vec<u8> {
    let mut out: alloc::vec::Vec<u8> = alloc::vec::Vec::with_capacity(text.len() + 7);
    out.extend_from_slice(b"<!--");
    out.extend_from_slice(text.as_bytes());
    out.extend_from_slice(b"-->");
    out
}

/// One comment child's text and authored span, for the comment-write splice.
type CommentChild = (String, (usize, usize));

/// The element's comment children as parallel TEXT and SPAN lists.
type CommentChildren = (alloc::vec::Vec<String>, alloc::vec::Vec<(usize, usize)>);

/// The element's comment children, in source order: each child's TEXT (the
/// comment leaf's semantic) and its authored `<!-- … -->` span. Enumerated
/// from the topology — the element's CHILD-ROLE occurrences name its
/// children, and a child whose kind is the comment kind is a comment. A
/// comment child without a bound span (a prolog/epilog comment, which bind
/// none) is a refusal: the splice can only replace spans it can name.
fn comment_children(
    document: &Document<'_>,
    element: NodeId,
    resources: &mut ResourceContext<'_>,
) -> Result<CommentChildren, CodecError> {
    let limit = BatchLimit::new(usize::MAX)
        .ok_or(jqf_data::DataError::ArithmeticOverflow)
        .map_err(crate::document::map_data)?;
    let Ok(mut reader) = document.topology_reader(resources) else {
        return Err(jqf_codec_core::data_contract(
            "XML comment-write topology read over a valid document",
        ));
    };
    // The Nodes phase precedes the Occurrences phase, so the comment-kind
    // set is complete before the first occurrence is processed.
    let mut comment_nodes: alloc::collections::BTreeSet<NodeId> = alloc::collections::BTreeSet::new();
    let mut comments: alloc::vec::Vec<CommentChild> = alloc::vec::Vec::new();
    loop {
        let poll = reader.poll_batch(limit, resources).map_err(crate::document::map_data)?;
        match poll {
            ReaderPoll::Batch(TopologyBatch::Nodes(nodes)) => {
                for node in &nodes {
                    let node = node.map_err(crate::document::map_data)?;
                    if node.kind().as_str() == COMMENT_KIND {
                        comment_nodes.insert(node.id());
                    }
                }
            }
            ReaderPoll::Batch(TopologyBatch::Occurrences(occurrences)) => {
                for occurrence in &occurrences {
                    let occurrence = occurrence.map_err(crate::document::map_data)?;
                    if occurrence.owner() != LocalOwnerRef::Node(element) {
                        continue;
                    }
                    if occurrence.role().as_str() != crate::document::CHILD_ROLE {
                        continue;
                    }
                    let target = occurrence.target();
                    if !comment_nodes.contains(&target) {
                        continue;
                    }
                    let handle = document.node_handle(target).map_err(crate::document::map_data)?;
                    let view = document.value_view(handle).map_err(crate::document::map_data)?;
                    let Some(jqf_data::ScalarView::String(text)) = view.scalar().map_err(crate::document::map_data)?
                    else {
                        return Err(unsupported(
                            "a comment child whose semantic is not a text run cannot be \
                             rewritten",
                        ));
                    };
                    let Some(span) = document.node_source_span(target).map_err(crate::document::map_data)? else {
                        return Err(unsupported(
                            "a comment child without a retained source span cannot be rewritten \
                             in place (a prolog/epilog comment)",
                        ));
                    };
                    comments.push((String::from(text), (span.start() as usize, span.end() as usize)));
                }
            }
            ReaderPoll::Pending => {
                resources
                    .try_begin_next_cooperative_entry(4_096)
                    .map_err(CodecError::from)?;
            }
            ReaderPoll::End(_) => break,
        }
    }
    let texts: alloc::vec::Vec<String> = comments.iter().map(|(text, _)| text.clone()).collect();
    let spans: alloc::vec::Vec<(usize, usize)> = comments.iter().map(|(_, span)| *span).collect();
    Ok((texts, spans))
}

/// Renders one value as splice CHILDREN bytes under the §1 mapping: a
/// scalar a text run, an array item an `<item>` element, an object its member
/// elements, `null` nothing. No document frame, namespaces, or trailing
/// newline — those belong to [`super::owned_value::serialize_owned_value`].
fn render_splice_children(out: &mut alloc::vec::Vec<u8>, value: &Value) -> Result<(), CodecError> {
    match value {
        Value::Null => {}
        Value::Bool(boolean) => escape_text(if *boolean { "true" } else { "false" }, out),
        Value::Number(number) => {
            let text = value_number_text(number)
                .ok_or_else(|| unsupported("a number with no canonical text cannot be spliced into XML"))?;
            escape_text(&text, out);
        }
        Value::String(text) => escape_text(text.as_str(), out),
        Value::Object(object) => {
            for entry in object {
                let key = entry.key();
                if !valid_element_name(key) {
                    return Err(invalid_element_key_for_splice(key));
                }
                out.push(b'<');
                out.extend_from_slice(key.as_bytes());
                out.push(b'>');
                render_splice_children(out, entry.value())?;
                out.extend_from_slice(b"</");
                out.extend_from_slice(key.as_bytes());
                out.push(b'>');
            }
        }
        Value::Array(array) => {
            for item in array {
                out.push(b'<');
                out.extend_from_slice(VALUE_ITEM_NAME.as_bytes());
                out.push(b'>');
                render_splice_children(out, item)?;
                out.extend_from_slice(b"</");
                out.extend_from_slice(VALUE_ITEM_NAME.as_bytes());
                out.push(b'>');
            }
        }
        Value::Bytes(_)
        | Value::LocalDate(_)
        | Value::LocalTime(_)
        | Value::LocalDateTime(_)
        | Value::OffsetDateTime(_)
        | Value::Tagged { .. } => {
            return Err(unsupported(
                "a byte string, temporal, or tagged value has no XML content representation",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use jqf_codec_core::{AccessInput, AccessOutcome, AccessSession, CodecFailureKind, CodecRunContext};
    use jqf_data::{Array, TagId, Value};
    use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
    use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

    static CONTROL: ContinueControl = ContinueControl;

    fn resources() -> ResourceContext<'static> {
        ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                .expect("account"),
            &CONTROL,
            WorkMeter::try_new_v1(4_096).expect("work"),
        )
        .expect("resources")
    }

    fn decoded_root_element<'s>(
        xml: &'s [u8],
        resources: &mut ResourceContext<'_>,
    ) -> (jqf_codec_core::DocumentProduct<'s>, NodeId, ResolvedSource<'s>) {
        let source = ResolvedSource::new(SourceRef::new(SourceId::new(1), SourceKind::Input), "test.xml", xml, 0);
        let mut session = crate::session::XmlSession::new(source, false, true).expect("session");
        let mut context = CodecRunContext::new(resources);
        let result = session
            .decode(AccessInput::Source(source), &mut context)
            .expect("decode");
        let AccessOutcome::FullDocument(product) = result.outcome() else {
            panic!("expected full document");
        };
        let product = product.try_clone().expect("clone");
        let root = product.document().root_handle();
        let node = product.document().value_view(root).expect("view").node();
        (product, node, source)
    }

    fn tagged_string(text: &str, tag: &str) -> Value {
        let tag = TagId::try_new_unaccounted(tag).expect("tag");
        let payload = Value::try_string(text).expect("string");
        Value::try_tagged(tag, payload).expect("tagged")
    }

    #[test]
    fn a_tagged_scalar_is_unrepresentable_in_edit_splice_children() {
        let tagged = tagged_string("5", "!money");
        let mut out = alloc::vec::Vec::new();
        let error =
            render_splice_children(&mut out, &tagged).expect_err("tagged scalars must not strip to bare splice text");
        assert_eq!(error.kind(), CodecFailureKind::UnsupportedRepresentation);
        assert!(out.is_empty(), "a refused splice publishes no bytes");
    }

    #[test]
    fn a_tagged_string_is_refused_for_attribute_insert() {
        let mut resources = resources();
        let (product, node, source) = decoded_root_element(b"<r/>", &mut resources);
        let payload = tagged_string("v", "!money");
        let error = render_attribute_insert(product.document(), node, source.bytes(), "id", &payload, &mut resources)
            .expect_err("tagged attribute payloads must not unwrap to bare text");
        assert_eq!(error.kind(), CodecFailureKind::UnsupportedRepresentation);
    }

    #[test]
    fn a_tagged_comment_payload_is_refused() {
        let tag = TagId::try_new_unaccounted("!note").expect("tag");
        let lines = Value::Array(Array::try_from_vec(vec![Value::try_string("line").expect("string")]).expect("array"));
        let payload = Value::try_tagged(tag, lines).expect("tagged");
        let error = comment_payload_lines(&payload).expect_err("tagged comment payloads must not unwrap to bare lines");
        assert_eq!(error.kind(), CodecFailureKind::UnsupportedRepresentation);
    }

    #[test]
    fn a_tagged_comment_line_is_refused() {
        let payload = Value::Array(Array::try_from_vec(vec![tagged_string("line", "!note")]).expect("array"));
        let error = comment_payload_lines(&payload).expect_err("tagged comment lines must not unwrap to bare text");
        assert_eq!(error.kind(), CodecFailureKind::UnsupportedRepresentation);
    }
}
