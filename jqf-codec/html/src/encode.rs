//! The HTML encoder (output profiles).
//!
//! `html.source@1` returns the ORIGINAL bytes for a sealed, semantically unchanged complete HTML source: the located
//! item must be the document root of a document decoded from this source. Any other item is `UnsupportedRepresentation`
//! — the design's law: no serializer fallback for the source profile.
//!
//! `html.document-serialize@1` emits exactly one UTF-8 BOM (`EF BB BF`) followed by the document element: void elements
//! without end tags, and state-specific escaping. A doctype-bearing document is refused (`UnsupportedRepresentation`):
//! the byte law writes the document element only, and re-decoding such output would recover quirks mode.
//!
//! The document-serialize profile also serves arbitrary VALUES (§1): an owned value or a located node of a non-HTML
//! document is lowered into the element model — a synthetic document built with the decoder's own
//! `AccountedDocumentBuilder` machinery — and serialized by the same algorithm. The mapping is stated in
//! [`build_value_element`]: the document element is `root`, an object member `k` a child element named `k`, an array
//! item a child element named `item`, a string/number/boolean a text run, and `null` an empty element; a key that is
//! not a WHATWG tag name and byte/temporal/tagged values are refused, never renamed.

use alloc::borrow::ToOwned;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use jqf_codec_core::{
    ByteSink, CodecError, CodecFailureKind, CodecRunContext, EncodeItem, EncodeRequest, EncoderFactoryImpl,
    EncoderSession, ErasedEncoderFactory, ErasedEncoderSession, PhysicalRouteId, PreservationOutcome,
    PreservationReport, PreservationRequest, RecycledSessionState,
};
use jqf_data::{
    AccountedDocumentBuilder, AccountedSemanticNode, DecimalText, Document, DocumentFact, FactPayload, FactPayloadView,
    LocalOwnerRef, MaterializeWorkspace, NodeHandle, NodeId, ReaderPoll, Value, ValueKind, unbounded_batch_limit,
};
use jqf_resource::{ResourceContext, WorkAdmission};
use jqf_source::Namespace;

use crate::document::{ATTRIBUTE_FACT, COMMENT_FACT, DOCTYPE_FACT, NAME_FACT};
use crate::options::HtmlProfile;

/// The HTML diagnostics namespace.
const HTML: Namespace = Namespace::new("html");

/// Constructs an `UnsupportedRepresentation` failure carrying a message that names the shape problem.
pub(crate) fn unsupported(message: &str) -> CodecError {
    // The plain carrier builds fallibly; on refusal the bare failure survives (an unrepresentable document never gets
    // worse).
    let base = CodecError::new(CodecFailureKind::UnsupportedRepresentation);
    let Some(diagnostic) =
        jqf_source::Diagnostic::try_new(HTML.code("representation"), jqf_source::Severity::Error, message)
    else {
        return base;
    };
    base.with_diagnostic(diagnostic)
}

/// The output chunk offered to the host.
const OFFER_BYTES: usize = 64 * 1024;

/// The stable identity of the HTML encoder factory.
pub(crate) fn create_factory(
    request: EncodeRequest<'_, '_>,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedEncoderFactory, CodecError> {
    if request.format.as_str() != crate::FORMAT_ID {
        return Err(CodecError::new(CodecFailureKind::RequirementMismatch));
    }
    let profile = match request.dialect.as_str() {
        crate::HTML_SOURCE_DIALECT_ID => HtmlProfile::Source,
        crate::HTML_DOCUMENT_SERIALIZE_DIALECT_ID => HtmlProfile::DocumentSerialize,
        _ => return Err(CodecError::new(CodecFailureKind::RequirementMismatch)),
    };
    ErasedEncoderFactory::try_new_factory(request.preservation, resources, || Ok(HtmlEncoderFactory { profile }))
}

struct HtmlEncoderFactory {
    profile: HtmlProfile,
}

impl EncoderFactoryImpl for HtmlEncoderFactory {
    fn physical_encoder(&self) -> PhysicalRouteId {
        crate::ENCODE_PHYSICAL_ROUTE_ID
    }

    fn start<'item, 'source>(
        &self,
        item: EncodeItem<'item, 'source>,
        preservation: PreservationRequest,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedEncoderSession<'item, 'source>, CodecError> {
        ErasedEncoderSession::try_new(item, preservation, || {
            Ok(HtmlEncoder {
                bytes: Vec::new(),
                profile: self.profile,
                root_done: false,
                workspace: MaterializeWorkspace::new(),
            })
        })
    }

    fn try_restart(
        &self,
        state: &mut RecycledSessionState<'_>,
        _item: EncodeItem<'_, '_>,
        _preservation: PreservationRequest,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        let Some(encoder) = state.downcast_mut::<HtmlEncoder>() else {
            return Ok(false);
        };
        encoder.reset();
        Ok(true)
    }
}

struct HtmlEncoder {
    bytes: Vec<u8>,
    profile: HtmlProfile,
    root_done: bool,
    /// Document-independent materialization scratch, reused across every item of this (possibly recycled) session so
    /// the document-sized cycle bitmap is allocated once, not once per item. The workspace is uncharged scratch that
    /// jqf-data leaves all-clear after every materialization, success and error alike, so `reset` need not touch it.
    workspace: MaterializeWorkspace,
}

impl HtmlEncoder {
    /// Reinitializes one recycled encoder for one more ordered item: drops every byte and flag a previous item may have
    /// left behind — including one that aborted mid-offer, whose partial staging must never reach the next item —
    /// leaving exactly the state a fresh [`EncoderFactoryImpl::start`] would have produced.
    ///
    /// This reset owns only the session lifecycle fields — bytes and root_done. Staging-buffer and fact-read surfaces
    /// have their own owners.
    fn reset(&mut self) {
        self.bytes.clear();
        self.root_done = false;
    }

    fn push(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    fn encode_item(&mut self, item: EncodeItem<'_, '_>, resources: &mut ResourceContext<'_>) -> Result<(), CodecError> {
        match item {
            EncodeItem::Located { product, node } => {
                let document = product.document();
                match self.profile {
                    HtmlProfile::Source => {
                        // The whole-document root echoes the exact retained source. Everything else is unrepresentable
                        // under the source profile (no serializer fallback).
                        if node == document.root_handle()
                            && let Some(segment) = document.source_segment()
                        {
                            self.push(segment);
                            return Ok(());
                        }
                        Err(unsupported(
                            "the html.source@1 profile can only echo an unchanged whole HTML \
                             document; a subtree or edited value has no source echo under it \
                             (select --output-dialect html.document-serialize@1 for a rewritten \
                             document)",
                        ))
                    }
                    HtmlProfile::DocumentSerialize => {
                        if document.format().as_str() == crate::FORMAT_ID {
                            if node != document.root_handle() {
                                return Err(unsupported(
                                    "html.document-serialize@1 requires the document root; a subtree \
                                     needs a fragment serializer",
                                ));
                            }
                            let mut out = Vec::new();
                            // Exactly one UTF-8 BOM (the profile's byte law).
                            out.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
                            // One fact pass for the whole document, then the serialization walk.
                            let facts = ElementFacts::read(document, resources)?;
                            if facts.doctype {
                                // The byte law writes the document element only; dropping the doctype would flip the
                                // re-decode into quirks mode.
                                return Err(unsupported(
                                    "a document with a doctype cannot be represented in \
                                     html.document-serialize@1 output: the serializer writes \
                                     the document element only, and re-decoding it would \
                                     recover quirks mode",
                                ));
                            }
                            serialize_element(document, node, &facts, &mut out)?;
                            self.push(out.as_slice());
                            Ok(())
                        } else {
                            // A document decoded by another codec carries no HTML element names; the §1 value mapping
                            // lowers the located value into the element model instead.
                            let value = document
                                .materialize_node_with(&mut self.workspace, node, resources)
                                .map_err(|_| {
                                    CodecError::new(CodecFailureKind::InternalContractViolation {
                                        contract: "HTML value encode materialization",
                                    })
                                })?;
                            self.encode_value(&value, resources)
                        }
                    }
                }
            }
            EncodeItem::Owned(value) => match self.profile {
                // The source profile has no serializer fallback; the document-serialize profile lowers the owned value
                // into the element model (§1).
                HtmlProfile::Source => Err(unsupported(
                    "the html.source@1 profile can only echo an unchanged whole HTML document; \
                     an owned value has no source echo under it (select --output-dialect \
                     html.document-serialize@1 for a rewritten value)",
                )),
                HtmlProfile::DocumentSerialize => self.encode_value(value, resources),
            },
        }
    }

    /// The §1 value-encode path: lowers an owned `Value` into the codec's element model — a synthetic HTML-shaped
    /// document built with the same `AccountedDocumentBuilder` machinery the decoder uses — and serializes it with the
    /// pinned WHATWG fragment algorithm (one UTF-8 BOM, then the document-serialize byte law).
    ///
    /// The stated mapping mirrors XML's: the document element is named `root`, an object member `k` a child element
    /// named `k`, an array item a child element named `item`, a string/number/boolean a text run, and `null` an empty
    /// element. A key that is not a WHATWG tag name and byte/temporal/tagged values are refused, never renamed.
    fn encode_value(&mut self, value: &Value, resources: &mut ResourceContext<'_>) -> Result<(), CodecError> {
        let mut builder = crate::document::fresh_builder(jqf_data::BuilderCoverage::complete(), resources)?;
        let root = build_value_element(&mut builder, VALUE_ROOT_NAME, value, resources)?;
        let document = builder.finish(root, resources).map_err(crate::document::map_data)?;
        let node = document.root_handle();
        let mut out = Vec::new();
        // Exactly one UTF-8 BOM (the profile's byte law).
        out.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
        let facts = ElementFacts::read(&document, resources)?;
        serialize_element(&document, node, &facts, &mut out)?;
        self.push(out.as_slice());
        Ok(())
    }

    fn report(&self) -> PreservationReport {
        match self.profile {
            HtmlProfile::Source => PreservationReport::new(
                PreservationOutcome::Exact,
                PreservationOutcome::Exact,
                PreservationOutcome::Exact,
                PreservationOutcome::Exact,
            ),
            HtmlProfile::DocumentSerialize => PreservationReport::new(
                PreservationOutcome::Exact,
                PreservationOutcome::Exact,
                PreservationOutcome::Normalized,
                PreservationOutcome::Normalized,
            ),
        }
    }
}

impl EncoderSession for HtmlEncoder {
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
    fn encode(
        &mut self,
        item: EncodeItem<'_, '_>,
        sink: &mut dyn ByteSink,
        context: &mut CodecRunContext<'_, '_>,
    ) -> Result<PreservationReport, CodecError> {
        loop {
            if self.root_done {
                if !self.bytes.is_empty() {
                    sink.write_all(&self.bytes, context.resources())?;
                    self.bytes.clear();
                }
                return Ok(self.report());
            }
            if self.bytes.len() >= OFFER_BYTES {
                sink.write_all(&self.bytes, context.resources())?;
                self.bytes.clear();
                continue;
            }
            let remaining = context.resources().remaining_work() as usize;
            match context.resources().admit_work_transitions(remaining.max(1))? {
                WorkAdmission::Pending => context.replenish_work()?,
                WorkAdmission::Granted(granted) => {
                    for _ in 0..granted {
                        if self.root_done || self.bytes.len() >= OFFER_BYTES {
                            break;
                        }
                        self.encode_item(item, context.resources())?;
                        self.root_done = true;
                    }
                }
            }
        }
    }
}

/// The void elements (no end tag).
const VOID_ELEMENTS: &[&str] = &[
    "area", "base", "basefont", "bgsound", "br", "col", "embed", "frame", "hr", "img", "input", "keygen", "link",
    "meta", "param", "source", "track", "wbr",
];

/// The raw-text elements (escaped with the text escaping rules).
const RAW_TEXT_ELEMENTS: &[&str] = &["style", "script", "xmp", "iframe", "noembed", "noframes", "plaintext"];

fn is_named(names: &[&str], name: &str) -> bool {
    names.iter().any(|candidate| name.eq_ignore_ascii_case(candidate))
}

/// Serializes one element subtree with the pinned WHATWG fragment serialization algorithm. The output is staged in a
/// plain `Vec<u8>`.
fn serialize_element(
    document: &Document<'_>,
    node: NodeHandle,
    facts: &ElementFacts,
    out: &mut Vec<u8>,
) -> Result<(), CodecError> {
    let id = document.resolve_node_handle(node).map_err(|_| {
        CodecError::new(CodecFailureKind::InternalContractViolation {
            contract: "HTML serializer node",
        })
    })?;
    let name = facts
        .names
        .get(&id.get())
        .ok_or_else(|| CodecError::new(CodecFailureKind::UnsupportedRepresentation))?;
    out.extend_from_slice(b"<");
    out.extend_from_slice(name.as_bytes());
    let attrs = facts.attrs.get(&id.get());
    if let Some(attrs) = attrs {
        for (key, value) in attrs {
            out.extend_from_slice(b" ");
            out.extend_from_slice(key.as_bytes());
            out.extend_from_slice(b"=\"");
            for ch in value.chars() {
                match ch {
                    '&' => out.extend_from_slice(b"&amp;"),
                    '\u{00A0}' => out.extend_from_slice(b"&nbsp;"),
                    '"' => out.extend_from_slice(b"&quot;"),
                    _ => push_utf8(ch, out),
                }
            }
            out.extend_from_slice(b"\"");
        }
    }
    out.extend_from_slice(b">");
    if is_named(VOID_ELEMENTS, name) {
        return Ok(());
    }
    // The children: text and element children from the located array, interleaved with the recovered comments at their
    // tree positions (a comment between two text runs keeps them separate on a re-decode).
    let mut children = Vec::new();
    collect_element_children(document, node, &mut children)?;
    // Borrowed, not cloned: the fact table is immutable for the whole walk.
    let comments = facts.comments.get(&id.get()).map(Vec::as_slice).unwrap_or(&[]);
    // The build writes the comment positions ascending (the roles are grouped leading-then-inline over an enumerate of
    // the children), so no per-element re-sort is owed here; the assertion keeps that producer law checked wherever
    // tests run.
    debug_assert!(
        comments.is_sorted_by_key(|(position, _)| *position),
        "comment fact positions must arrive ascending"
    );
    let raw_text = is_named(RAW_TEXT_ELEMENTS, name);
    let mut comment_index = 0usize;
    let mut child_index = 0usize;
    for tree_position in 0..children.len() + comments.len() {
        if comment_index < comments.len() && comments[comment_index].0 == tree_position {
            let (_, text) = &comments[comment_index];
            out.extend_from_slice(b"<!--");
            out.extend_from_slice(text.as_bytes());
            out.extend_from_slice(b"-->");
            comment_index += 1;
            continue;
        }
        let Some(child) = children.get(child_index) else {
            continue;
        };
        child_index += 1;
        match child {
            Child::Text(text) => {
                if raw_text {
                    out.extend_from_slice(text.as_bytes());
                } else {
                    for ch in text.chars() {
                        match ch {
                            '&' => out.extend_from_slice(b"&amp;"),
                            '\u{00A0}' => out.extend_from_slice(b"&nbsp;"),
                            '<' => out.extend_from_slice(b"&lt;"),
                            '>' => out.extend_from_slice(b"&gt;"),
                            _ => push_utf8(ch, out),
                        }
                    }
                }
            }
            Child::Element(child) => {
                if *child == node {
                    continue;
                }
                // The recursive call emits the child's OWN end tag (or none for a void element) — the parent must never
                // append a second one.
                serialize_element(document, *child, facts, out)?;
            }
        }
    }
    out.extend_from_slice(b"</");
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(b">");
    Ok(())
}

enum Child {
    Text(String),
    Element(NodeHandle),
}

/// Collects the element children and text runs of one element node.
fn collect_element_children(document: &Document<'_>, node: NodeHandle, out: &mut Vec<Child>) -> Result<(), CodecError> {
    let view = document.value_view(node).map_err(|_| {
        CodecError::new(CodecFailureKind::InternalContractViolation {
            contract: "HTML serializer view",
        })
    })?;
    let array = view
        .array()
        .map_err(|_| {
            CodecError::new(CodecFailureKind::InternalContractViolation {
                contract: "HTML serializer array",
            })
        })?
        .ok_or_else(|| CodecError::new(CodecFailureKind::UnsupportedRepresentation))?;
    for index in 0..array.len() {
        let child = array.get(index).ok_or_else(|| {
            CodecError::new(CodecFailureKind::InternalContractViolation {
                contract: "HTML serializer child index",
            })
        })?;
        match child.kind().map_err(|_| {
            CodecError::new(CodecFailureKind::InternalContractViolation {
                contract: "HTML serializer child kind",
            })
        })? {
            ValueKind::String => {
                let scalar = child
                    .scalar()
                    .map_err(|_| {
                        CodecError::new(CodecFailureKind::InternalContractViolation {
                            contract: "HTML serializer scalar",
                        })
                    })?
                    .ok_or_else(|| CodecError::new(CodecFailureKind::UnsupportedRepresentation))?;
                let text = match scalar {
                    jqf_data::ScalarView::String(text) => text.to_string(),
                    _ => {
                        return Err(CodecError::new(CodecFailureKind::UnsupportedRepresentation));
                    }
                };
                out.push(Child::Text(text));
            }
            ValueKind::Array => {
                let node = child.node();
                let handle = document.node_handle(node).map_err(|_| {
                    CodecError::new(CodecFailureKind::InternalContractViolation {
                        contract: "HTML serializer child handle",
                    })
                })?;
                out.push(Child::Element(handle));
            }
            _ => {}
        }
    }
    Ok(())
}

fn html_attribute_pair(fact: &DocumentFact<'_>) -> Option<(String, String)> {
    if fact.role().as_str() != ATTRIBUTE_FACT {
        return None;
    }
    match fact.payload() {
        FactPayloadView::Text(text) => Some((fact.kind().as_str().to_string(), text.to_string())),
        FactPayloadView::Map(map) => {
            let mut name = None;
            let mut value = None;
            for (key, payload) in map.iter() {
                match (key, payload) {
                    ("name", FactPayloadView::Text(text)) => name = Some(text.to_string()),
                    ("value", FactPayloadView::Text(text)) => value = Some(text.to_string()),
                    _ => {}
                }
            }
            Some((name?, value?))
        }
        _ => None,
    }
}

/// The element facts of one document, gathered in ONE fact pass and keyed by node id so the serialization walk never
/// re-scans the fact table.
struct ElementFacts {
    /// node id -> element name.
    names: BTreeMap<u64, String>,
    /// node id -> attribute pairs.
    attrs: BTreeMap<u64, Vec<(String, String)>>,
    /// node id -> recovered comments as (tree position, text) pairs. The payload law: each `html.comment@1` fact holds
    /// a list of `{"text": ..., "position": ...}` maps, where the position is the comment's child index in the
    /// RECOVERED TREE — the serialize interleave reconstructs the in-place order from it.
    comments: BTreeMap<u64, Vec<(usize, String)>>,
    /// Whether the document carries the decoder's doctype fact. The document-serialize byte law writes the document
    /// element only, so a doctype-bearing document would re-decode into quirks mode — it is refused before any byte is
    /// published.
    doctype: bool,
}

impl ElementFacts {
    fn ingest(&mut self, fact: &DocumentFact<'_>) {
        let LocalOwnerRef::Node(node) = fact.owner() else {
            return;
        };
        let key = node.get();
        match fact.role().as_str() {
            NAME_FACT => {
                if let FactPayloadView::Text(text) = fact.payload() {
                    self.names.insert(key, text.to_string());
                }
            }
            ATTRIBUTE_FACT => {
                if let Some(pair) = html_attribute_pair(fact) {
                    self.attrs.entry(key).or_default().push(pair);
                }
            }
            COMMENT_FACT => {
                let FactPayloadView::List(items) = fact.payload() else {
                    return;
                };
                let mut comments = Vec::new();
                for item in items.iter() {
                    let FactPayloadView::Map(members) = item else {
                        continue;
                    };
                    let mut text = String::new();
                    let mut position = None;
                    for (member, value) in members.iter() {
                        match (member, value) {
                            ("text", FactPayloadView::Text(value)) => text.push_str(value),
                            ("position", FactPayloadView::Integer(value)) => {
                                position = value.parse::<usize>().ok();
                            }
                            _ => {}
                        }
                    }
                    if let Some(position) = position {
                        comments.push((position, text));
                    }
                }
                let slot = self.comments.entry(key).or_default();
                slot.extend(comments);
                slot.sort_by_key(|(position, _)| *position);
            }
            DOCTYPE_FACT => self.doctype = true,
            _ => {}
        }
    }

    /// One attached-fact pass, via the owner index when `finish`/`publish` built it.
    fn read(document: &Document<'_>, resources: &mut ResourceContext<'_>) -> Result<Self, CodecError> {
        let mut facts = ElementFacts {
            names: BTreeMap::new(),
            attrs: BTreeMap::new(),
            comments: BTreeMap::new(),
            doctype: false,
        };
        if document.fact_owner_indexed() {
            for index in 0..document.node_count() {
                let Some(node) = NodeId::try_from_index(index) else {
                    break;
                };
                for fact_id in document.owner_fact_ids(node) {
                    let fact = document.fact(*fact_id).map_err(|_| {
                        CodecError::new(CodecFailureKind::InternalContractViolation {
                            contract: "HTML serializer facts",
                        })
                    })?;
                    facts.ingest(&fact);
                }
            }
            return Ok(facts);
        }
        let limit = unbounded_batch_limit();
        let mut reader = document
            .fact_reader(resources)
            .map_err(|_| CodecError::new(CodecFailureKind::UnsupportedRepresentation))?;
        loop {
            match reader.poll_batch(limit, resources).map_err(|_| {
                CodecError::new(CodecFailureKind::InternalContractViolation {
                    contract: "HTML serializer facts",
                })
            })? {
                ReaderPoll::Batch(batch) => {
                    for fact in batch.iter() {
                        facts.ingest(&fact);
                    }
                }
                ReaderPoll::Pending => {
                    resources
                        .try_begin_next_cooperative_entry(4096)
                        .map_err(CodecError::from)?;
                }
                ReaderPoll::End(_) => break,
            }
        }
        Ok(facts)
    }
}

/// Pushes one scalar's UTF-8 bytes.
fn push_utf8(ch: char, out: &mut Vec<u8>) {
    let mut buffer = [0u8; 4];
    out.extend_from_slice(ch.encode_utf8(&mut buffer).as_bytes());
}

/// The root and array-item element names of the §1 value mapping.
const VALUE_ROOT_NAME: &str = "root";
const VALUE_ITEM_NAME: &str = "item";

/// Lowers one owned value into the HTML element model (§1): returns the root element node of a synthetic document built
/// with the decoder's own `AccountedDocumentBuilder` machinery.
///
/// The mapping is stated and lossy on purpose: a value carries no element names, so object keys name their elements,
/// array items get the fixed `item` name, and the document element is the fixed `root` name. A key that is not a WHATWG
/// tag name (ASCII alpha start, then ASCII alnum or hyphen) is refused, never renamed.
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
    if is_named(VOID_ELEMENTS, name) && !matches!(value, Value::Null) {
        return Err(unsupported(
            "a void element cannot carry a child value; only null serializes as an empty void tag",
        ));
    }
    match value {
        Value::Null => {}
        Value::Bool(boolean) => add_value_text(builder, id, if *boolean { "true" } else { "false" }, resources)?,
        Value::Number(number) => {
            let text = value_number_text(number)
                .ok_or_else(|| unsupported("a number with no canonical text cannot be represented as HTML"))?;
            add_value_text(builder, id, &text, resources)?;
        }
        Value::String(text) => add_value_text(builder, id, text, resources)?,
        Value::Object(object) => {
            for entry in object {
                let key = entry.key();
                if !valid_tag_name(key) {
                    return Err(unsupported(&alloc::format!(
                        "object key {key:?} is not a valid HTML tag name; the value cannot be \
                             represented as HTML (the value mapping never renames keys)"
                    )));
                }
                let child = build_value_element(builder, key, entry.value(), resources)?;
                add_value_child(builder, id, child, resources)?;
            }
        }
        Value::Array(array) => {
            for item in array.iter() {
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
                "a byte string, temporal, or tagged value has no HTML element representation",
            ));
        }
    }
    Ok(id)
}

/// Adds one text-run child (a `html.text@1` leaf) to an element node.
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

/// Whether `name` is a valid WHATWG tag name for the value mapping: ASCII alpha start, then ASCII alphanumerics or
/// hyphens. The WHATWG tokenizer itself is more lenient; the mapping states a conservative rule so a synthetic name
/// never produces ambiguous markup.
fn valid_tag_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_alphabetic() && chars.all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// The canonical number text of a value: an integer's own digits, a decimal's scientific-string form, and a binary64's
/// float rendering (NaN renders `null`, an infinity the clamped widest finite — the shared number-text law, not a codec
/// decision).
fn value_number_text(number: &jqf_data::Number) -> Option<String> {
    // The inline machine arm renders its canonical spelling on demand; the boxed arm borrows its retained one.
    if let Some(machine) = number.as_machine() {
        let integer = jqf_data::Integer::from_i64(machine);
        return Some(integer.as_str().to_owned());
    }
    if let Some(integer) = number.as_integer() {
        return Some(integer.as_str().to_owned());
    }
    if let Some(decimal) = number.as_decimal() {
        let rendered = DecimalText::new(decimal.coefficient().as_str(), decimal.scale())?;
        let mut out = String::new();
        for piece in rendered.pieces() {
            out.push_str(core::str::from_utf8(piece).ok()?);
        }
        return Some(out);
    }
    let float = number.as_float()?;
    Some(jqf_data::format_binary64(float.get())?.as_str().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use jqf_codec_core::{CodecRunContext, DiagnosticPolicy, EncodeRequest};
    use jqf_data::{Array, DialectId, FormatId, Integer, Number, ObjectBuilder, ObjectKey, Shared, Value};
    use jqf_resource::{ContinueControl, RequestAccount, ResourceLimits, WorkMeter};

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

    fn one() -> Value {
        Value::Number(Number::try_integer_unaccounted(Integer::parse("1").expect("int")).expect("number"))
    }

    fn text(value: &str) -> Value {
        Value::String(Shared::try_from_str(value).expect("string"))
    }

    /// Encodes one owned value through the document-serialize profile, returning the published bytes.
    fn encode_owned(value: &Value) -> Result<Vec<u8>, CodecError> {
        let registration = crate::registration().expect("registration");
        let mut resources = resources();
        let format = FormatId::try_new(crate::FORMAT_ID).expect("format");
        let dialect = DialectId::try_new(crate::HTML_DOCUMENT_SERIALIZE_DIALECT_ID).expect("dialect");
        let request = EncodeRequest {
            format: &format,
            dialect: &dialect,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            preservation: PreservationRequest::None,
            options: None,
        };
        let factory = registration
            .encoder()
            .expect("encoder")
            .create_factory(request, &mut resources)
            .expect("factory");
        let mut session = factory
            .start(EncodeItem::Owned(value), PreservationRequest::None, &mut resources)
            .expect("start");
        let mut out = Vec::new();
        {
            let mut sink = jqf_codec_core::VecByteSink::new(&mut out);
            let mut context = CodecRunContext::new(&mut resources);
            session.encode(&mut sink, &mut context)?;
        }
        Ok(out)
    }

    /// The §1 value mapping under the document-serialize profile: one UTF-8 BOM, then the WHATWG serialization of the
    /// lowered `root` element.
    #[test]
    fn the_document_serialize_profile_encodes_owned_values() {
        let mut builder = ObjectBuilder::try_with_capacity(2).expect("builder");
        builder
            .try_insert_last(ObjectKey::try_from_str("a").expect("key"), one())
            .expect("insert");
        builder
            .try_insert_last(ObjectKey::try_from_str("b").expect("key"), text("x&y"))
            .expect("insert");
        let object = Value::Object(builder.try_finish().expect("object"));
        let bytes = encode_owned(&object).expect("encode");
        let out = core::str::from_utf8(&bytes).expect("utf8");
        assert!(out.starts_with('\u{FEFF}'), "exactly one UTF-8 BOM first, got {out:?}");
        assert_eq!(out, "\u{FEFF}<root><a>1</a><b>x&amp;y</b></root>", "object lowering");

        let array = Value::Array(Array::try_from_vec(vec![one(), text("z")]).expect("array"));
        let bytes = encode_owned(&array).expect("encode");
        let out = core::str::from_utf8(&bytes).expect("utf8");
        assert_eq!(
            out, "\u{FEFF}<root><item>1</item><item>z</item></root>",
            "array lowering"
        );
    }

    /// A key that is not a WHATWG tag name is refused, never renamed.
    #[test]
    fn an_invalid_tag_key_is_unrepresentable() {
        let mut builder = ObjectBuilder::try_with_capacity(1).expect("builder");
        builder
            .try_insert_last(ObjectKey::try_from_str("1x").expect("key"), text("v"))
            .expect("insert");
        let object = Value::Object(builder.try_finish().expect("object"));
        let error = encode_owned(&object).expect_err("unrepresentable key");
        assert_eq!(error.kind(), CodecFailureKind::UnsupportedRepresentation);
    }

    /// A void tag name with a non-null value is refused, never dropped.
    #[test]
    fn a_void_element_with_a_child_is_unrepresentable() {
        let mut builder = ObjectBuilder::try_with_capacity(1).expect("builder");
        builder
            .try_insert_last(ObjectKey::try_from_str("br").expect("key"), text("hello"))
            .expect("insert");
        let object = Value::Object(builder.try_finish().expect("object"));
        let error = encode_owned(&object).expect_err("void child stays unrepresentable");
        assert_eq!(error.kind(), CodecFailureKind::UnsupportedRepresentation);
    }

    /// A byte string has no HTML element representation.
    #[test]
    fn a_byte_value_is_unrepresentable() {
        let value = Value::Bytes(Shared::try_from_slice(b"raw").expect("bytes"));
        let error = encode_owned(&value).expect_err("bytes stay unrepresentable");
        assert_eq!(error.kind(), CodecFailureKind::UnsupportedRepresentation);
    }
}
