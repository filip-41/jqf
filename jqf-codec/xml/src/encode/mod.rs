//! XML encoder (output profiles).
//!
//! Two output profiles implement the §4.9 encode law.
//!
//! `xml.source@1` returns the ORIGINAL bytes for an unchanged valid document:
//! a located item whose node is the document root reuses the exact retained
//! source authority the decode sealed. V1 gives it no serializer fallback —
//! any other item (a subtree, an edited value) is `UnsupportedRepresentation`,
//! and the caller must select the deterministic profile instead.
//!
//! `xml.jqf-deterministic@1` is a jqf byte profile, not W3C Canonical XML:
//! UTF-8 without BOM, no XML declaration, LF, one trailing LF, every element
//! with explicit start and end tags, gathered namespace URIs re-bound as
//! `n0`, `n1`, ... on the document element (`xml` keeps its predeclared
//! prefix), declarations before attributes, attribute values double-quoted
//! with the named references, and character data with `&`, `<`, `>` and CR
//! replaced. Doctype-bearing documents are unrepresentable (the decoder
//! carries the fact); prefix reallocation is a preservation entry, not an
//! error.
//!
//! The deterministic profile also serves arbitrary VALUES (§1), not
//! only XML-decoded documents: an owned value or a located node of a
//! non-XML document is lowered into the element/attribute model — a
//! synthetic document built with the decoder's own `AccountedDocumentBuilder`
//! machinery — and rendered by [`owned_value::serialize_owned_value`]
//! through [`DeterministicSerializer`] (document frame + trailing newline).
//! The mapping is stated in [`value_mapping`] and implemented separately by
//! [`owned_value`] (synthetic document + serializer) and [`edit_splice`]
//! (bare splice bytes): the document element is `root`, an
//! object member `k` a child element named `k`, an array item a child
//! element named `item`, a string/number/boolean a text run, and `null` an
//! empty element; a key that is not a valid XML `Name` and byte/temporal/
//! tagged values are refused, never renamed.
//!
//! All `UnsupportedRepresentation` raises go through the `unsupported`
//! helper, which attaches a message-only diagnostic naming the shape problem.
//!
//! # The edit splice policy
//!
//! [`EncoderFactoryImpl::render_leaf`] is the `--edit` leaf seam, and it is
//! POSITION-DEPENDENT by construction: XML's escaping is a function of the
//! authored position, not of the value. The bound spans name the position
//! exactly — a TEXT leaf's authored span is its character data (entities
//! preserved), an ATTRIBUTE fact's span is its QUOTED value bytes — and
//! the node's own semantic plus those authored bytes tell the seam which
//! grammar to render in:
//!
//! 1. **A text leaf renders as character data** — `&`, `<`, `>` and CR
//!    escaped, TAB and LF literal (`escape_text`, the §4.9 text law).
//! 2. **An attribute value renders quoted and attribute-escaped** — `&`,
//!    `<`, `>`, `"` and the whitespace hex references (`escape_attr`),
//!    wrapped in the authored span's quote character (a `'…'` site stays
//!    single-quoted). The quotes are part of the addressed span, so the
//!    rendered leaf includes them.
//!
//! [`EncoderFactoryImpl::render_fact_delta`] serves two XML fact writes:
//! HEAD comment children (value-identity only) and a missing-attribute
//! INSERT. The insert walks the start tag from the element's extent start
//! with the attribute-value quote rules and splices ` name="escaped"`
//! immediately before the first out-of-quote `>` or `/>`. Existing
//! attributes stay on the leaf path. The name law accepts one unprefixed
//! XML Name: no `:`, no `xmlns`, no clark `{uri}local`, and not a name
//! the element already carries. A `null` payload is a deletion and is
//! refused — the splice names the value span, not the ` name="value"`
//! token.
//!
//! [`EncoderFactoryImpl::render_edit_append`] grows an element's children:
//! XML has no comments to preserve on the value-identity splice (comment
//! children are a fact-write surface), but it
//! HAS the element/attribute structure, and the value model keeps every
//! child as a separate array item — inter-child whitespace is itself a text
//! child, never a free separator. The laws:
//!
//! 1. **An append grows inside the last element's extent.** The added
//!    members render through [`edit_splice::render_splice_children`] (the §1
//!    value-mapping grammar as bare splice bytes) and splice immediately before end tag
//!    (`</name>`). The splice adds NO separator bytes: whitespace before the
//!    end tag is itself a text child in the value model, so an inserted
//!    separator would become an extra text child and fail verification —
//!    the file's own layout survives because nothing around the insertion
//!    moves.
//! 2. **A scalar append needs a non-text predecessor.** Adjacent character
//!    data MERGES in XML (`<a>x</a>` + `y` is the one text `xy`, never two
//!    children), so a scalar member whose preceding child is itself a text
//!    leaf declines to the floor; an element predecessor (or an empty
//!    element) makes the text a separate child and splices.
//! 3. **A self-closing element declines to append**: an insertion is a
//!    zero-length splice and cannot remove the `/>`; the whole-document
//!    floor rewrites the element in explicit form.
//! 4. **A member the value grammar cannot carry** — bytes, temporals,
//!    tagged values, an invalid element-name key — is refused, never
//!    renamed.
//!
//! [`EncoderFactoryImpl::render_edit_remove`] cuts each removed child's OWN
//! extent span only. A removal never takes adjacent whitespace, because
//! that whitespace is a separate text child in the value model: the program
//! that deleted the element kept the whitespace text in its output, so a
//! splice that cut it would fail verification. A removed comment or
//! processing-instruction child has no bound span (spans bind text, attribute
//! values, and element extents only) and declines to the floor.
//!
//! Every splice is verified by re-decode before publication; a declined or
//! wrong splice falls to the whole-document floor rather than corrupting
//! the file.
use alloc::borrow::ToOwned;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use jqf_codec_core::byte_scan::{StopSet, prefix_len};
use jqf_codec_core::{
    ByteSink, CodecError, CodecFailureKind, CodecRunContext, EditAppendMembers, EditInsertion, EditRemoval,
    EditRemoveMembers, EncodeItem, EncodeRequest, EncoderFactoryImpl, EncoderSession, ErasedEncoderFactory,
    ErasedEncoderSession, PhysicalRouteId, PreservationOutcome, PreservationReport, PreservationRequest,
    RecycledSessionState,
};
use jqf_data::{
    Document, DocumentFact, FactPayloadView, LocalOwnerRef, MaterializeWorkspace, NodeHandle, NodeId, ReaderPoll,
    TopologyBatch, Value, unbounded_batch_limit,
};
use jqf_resource::{ResourceContext, WorkAdmission};
use jqf_source::{Namespace, Severity};

use crate::document::{COMMENT_KIND, DOCTYPE_FACT, NAME_FACT, PI_KIND, TEXT_KIND};
use crate::options::XmlProfile;

mod edit_splice;
mod owned_value;
mod value_mapping;

/// The XML diagnostics namespace, shared by the decoder and the encoder.
const XML: Namespace = Namespace::new("xml");

/// One markup-attribute fact as a (name, value) pair. Text payload uses the
/// fact kind as the name; a map payload carries `name`/`value` keys (HTML
/// names that are not jqf-data identities).
fn attribute_text_pair(fact: &DocumentFact<'_>) -> Option<(String, String)> {
    if fact.role().as_str() != jqf_codec_core::markup::ATTRIBUTE_FACT {
        return None;
    }
    match fact.payload() {
        FactPayloadView::Text(text) => Some((fact.kind().as_str().to_owned(), text.to_owned())),
        FactPayloadView::Map(map) => {
            let mut name = None;
            let mut value = None;
            for (key, payload) in map.iter() {
                match (key, payload) {
                    ("name", FactPayloadView::Text(text)) => name = Some(text.to_owned()),
                    ("value", FactPayloadView::Text(text)) => value = Some(text.to_owned()),
                    _ => {}
                }
            }
            Some((name?, value?))
        }
        _ => None,
    }
}

/// Constructs an `UnsupportedRepresentation` failure carrying a message that
/// names the shape problem: a document the XML value set cannot hold fails
/// with words about the DOCUMENT (which value, which fact, why), not with the
/// bare class name. The diagnostic is message-only: encode-side failures
/// describe the value, not a source span. If diagnostic construction is
/// refused on resource grounds the bare failure survives, so the error path
/// never makes an unrepresentable document worse.
pub(crate) fn unsupported(message: &str) -> CodecError {
    // The plain carrier builds fallibly; on refusal the bare
    // failure survives (an unrepresentable document never gets worse).
    let base = CodecError::new(CodecFailureKind::UnsupportedRepresentation);
    let Some(diagnostic) = jqf_source::Diagnostic::try_new(XML.code("representation"), Severity::Error, message) else {
        return base;
    };
    base.with_diagnostic(diagnostic)
}

/// The output chunk offered to the host.
const OFFER_BYTES: usize = 64 * 1024;

/// Constructs the encoder factory for the request's output profile.
pub(crate) fn create_factory(
    request: EncodeRequest<'_, '_>,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedEncoderFactory, CodecError> {
    if request.format.as_str() != crate::FORMAT_ID {
        return Err(CodecError::new(CodecFailureKind::RequirementMismatch));
    }
    let profile = match request.dialect.as_str() {
        crate::XML_SOURCE_DIALECT_ID => XmlProfile::Source,
        crate::XML_DETERMINISTIC_DIALECT_ID => XmlProfile::Deterministic,
        _ => return Err(CodecError::new(CodecFailureKind::RequirementMismatch)),
    };
    ErasedEncoderFactory::try_new_factory(request.preservation, resources, || Ok(XmlEncoderFactory { profile }))
}

struct XmlEncoderFactory {
    profile: XmlProfile,
}

impl EncoderFactoryImpl for XmlEncoderFactory {
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
            Ok(XmlEncoder {
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
        let Some(encoder) = state.downcast_mut::<XmlEncoder>() else {
            return Ok(false);
        };
        encoder.reset();
        Ok(true)
    }

    fn render_leaf(
        &self,
        document: &Document<'_>,
        node: NodeId,
        _path: &[String],
        _source: &[u8],
        value: &Value,
        authored: Option<&[u8]>,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<alloc::vec::Vec<u8>, CodecError> {
        edit_splice::render_leaf(document, node, value, authored)
    }

    fn render_edit_append(
        &self,
        document: &Document<'_>,
        container: NodeId,
        _path: &[String],
        source: &[u8],
        members: EditAppendMembers<'_>,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<alloc::vec::Vec<EditInsertion>, CodecError> {
        // An XML element's value is a children ARRAY; a table-shaped growth
        // cannot arise from the XML document model, so only the array arm is
        // expressible (defensive: decline rather than guess).
        let EditAppendMembers::Array(items) = members else {
            return Ok(alloc::vec::Vec::new());
        };
        edit_splice::render_edit_append(document, container, source, items)
    }

    fn render_edit_remove(
        &self,
        document: &Document<'_>,
        _container: NodeId,
        _path: &[String],
        _source: &[u8],
        members: EditRemoveMembers<'_>,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<alloc::vec::Vec<EditRemoval>, CodecError> {
        edit_splice::render_edit_remove(document, members)
    }

    fn render_fact_delta(
        &self,
        document: &Document<'_>,
        node: NodeId,
        source: &[u8],
        role: &str,
        kind: &str,
        payload: &Value,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<alloc::vec::Vec<jqf_codec_core::FactEditPatch>>, CodecError> {
        // XML's own fact-write surface is the HEAD comment position and
        // a missing-attribute INSERT. Comments are `<!-- … -->` CHILDREN
        // of the element, so `.@comment` reads exactly those children and
        // a write replaces them in place. A missing `.&name` inserts
        // ` name="…"` before the start-tag close. Every other role — the
        // inline and foot comment positions, the metadata facts — is not
        // an XML fact surface and declines to the seam's shared handling.
        match role {
            role if role == jqf_codec_core::comment::HEAD => {
                edit_splice::render_comment_write(document, node, source, payload, resources)
            }
            role if role == jqf_codec_core::markup::ATTRIBUTE_FACT => {
                edit_splice::render_attribute_insert(document, node, source, kind, payload, resources)
            }
            _ => Ok(None),
        }
    }
}

struct XmlEncoder {
    bytes: Vec<u8>,
    profile: XmlProfile,
    /// Whether this encoder has consumed its single item and may only flush.
    root_done: bool,
    /// Document-independent materialization scratch, reused across every item
    /// of this (possibly recycled) session so the document-sized cycle
    /// bitmap is allocated once, not once per item. The
    /// workspace is uncharged scratch that jqf-data leaves all-clear after
    /// every materialization, success and error alike, so `reset` need not
    /// touch it.
    workspace: MaterializeWorkspace,
}

impl XmlEncoder {
    /// Reinitializes one recycled encoder for one more ordered item: drops
    /// every byte and flag a previous item may have left behind — including
    /// one that aborted mid-offer, whose partial staging must never reach the
    /// next item — leaving exactly the state a fresh [`EncoderFactoryImpl::start`]
    /// would have produced.
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
                    XmlProfile::Source => {
                        // The whole-document root echoes the exact retained
                        // source. Everything else — a subtree, an edited
                        // value — is unrepresentable under the source profile
                        // (no serializer fallback).
                        if node == document.root_handle()
                            && let Some(segment) = document.source_segment()
                        {
                            self.push(segment);
                            return Ok(());
                        }
                        Err(unsupported(
                            "the xml.source@1 profile can only echo an unchanged whole XML document; \
                             a subtree or edited value has no XML serialization under it (select \
                             --output-dialect xml.jqf-deterministic@1 for a rewritten document)",
                        ))
                    }
                    XmlProfile::Deterministic => {
                        if has_doctype(document, resources)? {
                            return Err(unsupported(
                                "a document with a doctype cannot be represented in XML output",
                            ));
                        }
                        if document.format().as_str() == crate::FORMAT_ID {
                            // A document decoded by THIS codec: the serializer
                            // byte law runs over its own facts, unchanged.
                            self.serialize_located(document, node, resources)
                        } else {
                            // A document decoded by another codec (JSON, ...)
                            // carries no XML element names. The §1 value
                            // mapping lowers the located value into the
                            // element/attribute model instead — the
                            // deterministic profile now serves arbitrary
                            // values, not only XML-decoded documents.
                            let value = document
                                .materialize_node_with(&mut self.workspace, node, resources)
                                .map_err(|_| jqf_codec_core::data_contract("XML value encode materialization"))?;
                            self.encode_owned_value(&value, resources)
                        }
                    }
                }
            }
            EncodeItem::Owned(value) => match self.profile {
                // The source profile has no serializer fallback (§4.9:
                // V1 gives it none); the deterministic profile lowers the
                // owned value into the element/attribute model.
                XmlProfile::Source => Err(unsupported(
                    "the xml.source@1 profile can only echo an unchanged whole XML document; \
                     an owned value has no XML serialization under it (select \
                     --output-dialect xml.jqf-deterministic@1 for a rewritten value)",
                )),
                XmlProfile::Deterministic => self.encode_owned_value(value, resources),
            },
        }
    }

    /// Serializes one located node of an XML-decoded document (the existing
    /// fact-based byte law).
    fn serialize_located(
        &mut self,
        document: &Document<'_>,
        node: NodeHandle,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        let mut serializer = DeterministicSerializer::new();
        serializer.prepare(document, resources)?;
        serializer.gather_node(document, node, resources)?;
        serializer.assign_prefixes()?;
        serializer.render(document, node, resources, &mut self.bytes)?;
        Ok(())
    }

    /// Owned-value encode: synthetic document + [`DeterministicSerializer`]
    /// (document frame and trailing newline). See [`owned_value`].
    fn encode_owned_value(&mut self, value: &Value, resources: &mut ResourceContext<'_>) -> Result<(), CodecError> {
        let bytes = owned_value::serialize_owned_value(value, resources)?;
        self.push(&bytes);
        Ok(())
    }

    fn report(&self) -> PreservationReport {
        match self.profile {
            XmlProfile::Source => PreservationReport::new(
                PreservationOutcome::Exact,
                PreservationOutcome::Exact,
                PreservationOutcome::Exact,
                PreservationOutcome::Exact,
            ),
            XmlProfile::Deterministic => PreservationReport::new(
                PreservationOutcome::Exact,
                PreservationOutcome::Exact,
                PreservationOutcome::Normalized,
                // Prefix reallocation and entity/CDATA-boundary loss are
                // preservation entries, not errors.
                PreservationOutcome::Normalized,
            ),
        }
    }
}

impl EncoderSession for XmlEncoder {
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
                if self.bytes.is_empty() {
                    return Ok(self.report());
                }
                sink.write_all(&self.bytes, context.resources())?;
                self.bytes.clear();
                continue;
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

/// Whether the document carries the decoder's doctype fact. The
/// deterministic preflight rejects doctype-bearing documents.
fn has_doctype(document: &Document<'_>, resources: &mut ResourceContext<'_>) -> Result<bool, CodecError> {
    match document.fact_reader(resources) {
        Ok(_) => {}
        Err(jqf_data::DataError::CapabilityUnavailable {
            capability: jqf_data::DocumentCapability::AttachedFacts,
        }) => return Ok(false),
        Err(_) => {
            return Err(jqf_codec_core::data_contract(
                "XML doctype fact read over a valid document",
            ));
        }
    }
    if document.fact_owner_indexed() {
        return root_owns_doctype(document);
    }
    let owner = LocalOwnerRef::Node(document.root());
    let limit = unbounded_batch_limit();
    let Ok(mut reader) = document.fact_reader(resources) else {
        return Err(jqf_codec_core::data_contract(
            "XML doctype fact read over a valid document",
        ));
    };
    loop {
        let poll = reader
            .poll_batch(limit, resources)
            .map_err(|_| jqf_codec_core::data_contract("XML doctype fact read over a valid document"))?;
        match poll {
            ReaderPoll::Batch(batch) => {
                for fact in batch.iter() {
                    if fact.owner() == owner && fact.role().as_str() == DOCTYPE_FACT {
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

fn root_owns_doctype(document: &Document<'_>) -> Result<bool, CodecError> {
    for fact_id in document.owner_fact_ids(document.root()) {
        let fact = document
            .fact(*fact_id)
            .map_err(|_| jqf_codec_core::data_contract("XML doctype fact read over a valid document"))?;
        if fact.role().as_str() == DOCTYPE_FACT {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Compact node-kind tag stored densely by node id. Anything that is not
/// text/comment/PI is rendered as an element.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum XmlKind {
    Element,
    Text,
    Comment,
    Pi,
}

/// The deterministic serializer: gathers namespace URIs, re-binds them as
/// `n0`, `n1`, ... on the document element, and renders the §4.9 byte law.
pub(super) struct DeterministicSerializer {
    /// Fallback name map used only when the document has no fact-owner index
    /// (`finish`-built owned values). Indexed documents look names up live.
    names: BTreeMap<u64, String>,
    /// Fallback attribute map; same split as [`Self::names`].
    attrs: BTreeMap<u64, Vec<(String, String)>>,
    /// Whether [`Document::fact_owner_indexed`] is live for this document.
    indexed: bool,
    /// Schema kind per node id; missing slots render as elements.
    kinds: Vec<XmlKind>,
    /// sorted nonempty namespace URIs in use.
    uris: Vec<String>,
    /// uri -> assigned prefix (`n0`, ...); the XML namespace uses `xml` and
    /// needs no declaration.
    prefixes: BTreeMap<String, String>,
}

impl DeterministicSerializer {
    fn new() -> Self {
        Self {
            names: BTreeMap::new(),
            attrs: BTreeMap::new(),
            indexed: false,
            kinds: Vec::new(),
            uris: Vec::new(),
            prefixes: BTreeMap::new(),
        }
    }

    fn prepare(&mut self, document: &Document<'_>, resources: &mut ResourceContext<'_>) -> Result<(), CodecError> {
        self.read_kinds(document, resources)?;
        self.indexed = document.fact_owner_indexed();
        if self.indexed {
            if !self.any_name_fact(document)? {
                return Err(unsupported(
                    "the document has no XML element names or attributes (they are attached facts \
                     of an XML document); a value decoded from another format cannot be represented \
                     as XML",
                ));
            }
            Ok(())
        } else {
            self.read_facts(document, resources)
        }
    }

    fn any_name_fact(&self, document: &Document<'_>) -> Result<bool, CodecError> {
        for index in 0..document.node_count() {
            let Some(node) = NodeId::try_from_index(index) else {
                break;
            };
            if self.name_of(document, node)?.is_some() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn name_of(&self, document: &Document<'_>, node: NodeId) -> Result<Option<String>, CodecError> {
        if self.indexed {
            for fact_id in document.owner_fact_ids(node) {
                let fact = document
                    .fact(*fact_id)
                    .map_err(|_| jqf_codec_core::data_contract("XML serializer fact read over a valid document"))?;
                if fact.role().as_str() == NAME_FACT
                    && let FactPayloadView::Text(text) = fact.payload()
                {
                    return Ok(Some(text.to_owned()));
                }
            }
            Ok(None)
        } else {
            Ok(self.names.get(&node.get()).cloned())
        }
    }

    fn attr_pairs(&self, document: &Document<'_>, node: NodeId) -> Result<Vec<(String, String)>, CodecError> {
        let mut entries = Vec::new();
        if self.indexed {
            for fact_id in document.owner_fact_ids(node) {
                let fact = document
                    .fact(*fact_id)
                    .map_err(|_| jqf_codec_core::data_contract("XML serializer fact read over a valid document"))?;
                if let Some(pair) = attribute_text_pair(&fact) {
                    entries.push(pair);
                }
            }
        } else if let Some(stored) = self.attrs.get(&node.get()) {
            entries.extend(stored.iter().cloned());
        }
        Ok(entries)
    }

    /// One attached-fact pass: element names and attribute maps.
    /// Used only when the document has no fact-owner index.
    fn read_facts(&mut self, document: &Document<'_>, resources: &mut ResourceContext<'_>) -> Result<(), CodecError> {
        let limit = unbounded_batch_limit();
        let mut reader = match document.fact_reader(resources) {
            Ok(reader) => reader,
            // A document from a non-XML codec (or from owned values) carries
            // no attached facts: its values have no element names or
            // attributes, so XML cannot represent them. That is an
            // UNSUPPORTED conversion, not an internal-contract break — the
            // same classification the `Owned` item arm and the doctype
            // preflight already use.
            Err(jqf_data::DataError::CapabilityUnavailable {
                capability: jqf_data::DocumentCapability::AttachedFacts,
            }) => {
                return Err(unsupported(
                    "the document has no XML element names or attributes (they are attached facts \
                     of an XML document); a value decoded from another format cannot be represented \
                     as XML",
                ));
            }
            Err(_) => {
                return Err(jqf_codec_core::data_contract(
                    "XML serializer fact read over a valid document",
                ));
            }
        };
        let mut saw_name = false;
        loop {
            let poll = reader
                .poll_batch(limit, resources)
                .map_err(|_| jqf_codec_core::data_contract("XML serializer fact read over a valid document"))?;
            match poll {
                ReaderPoll::Batch(batch) => {
                    for fact in batch.iter() {
                        let LocalOwnerRef::Node(node) = fact.owner() else {
                            continue;
                        };
                        let key = node.get();
                        match fact.role().as_str() {
                            NAME_FACT => {
                                saw_name = true;
                                if let FactPayloadView::Text(text) = fact.payload() {
                                    self.names.insert(key, text.to_owned());
                                }
                            }
                            role if role == jqf_codec_core::markup::ATTRIBUTE_FACT => {
                                if let Some(pair) = attribute_text_pair(&fact) {
                                    self.attrs.entry(key).or_default().push(pair);
                                }
                            }
                            _ => {}
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
        if !saw_name {
            // A document decoded by ANOTHER codec carries its own fact
            // roles (e.g. html.name@1), never xml.name@1 — no element has
            // a name and the output would be empty-tag soup. The "no XML
            // element names" law covers them (a value decoded from another
            // format cannot be represented as XML).
            return Err(unsupported(
                "the document has no XML element names or attributes (they are attached facts \
                 of an XML document); a value decoded from another format cannot be represented \
                 as XML",
            ));
        }
        Ok(())
    }

    /// One topology pass: every node's schema kind, so the renderer can tell
    /// text, comment, and processing-instruction leaves apart.
    fn read_kinds(&mut self, document: &Document<'_>, resources: &mut ResourceContext<'_>) -> Result<(), CodecError> {
        let limit = unbounded_batch_limit();
        let Ok(mut reader) = document.topology_reader(resources) else {
            return Err(jqf_codec_core::data_contract(
                "XML serializer topology read over a valid document",
            ));
        };
        self.kinds.clear();
        self.kinds.resize(document.node_count(), XmlKind::Element);
        loop {
            let poll = reader
                .poll_batch(limit, resources)
                .map_err(|_| jqf_codec_core::data_contract("XML serializer topology read over a valid document"))?;
            match poll {
                ReaderPoll::Batch(batch) => match batch {
                    TopologyBatch::Nodes(nodes) => {
                        for node in &nodes {
                            let node =
                                node.map_err(|_| jqf_codec_core::data_contract("XML serializer topology node"))?;
                            let kind = match node.kind().as_str() {
                                TEXT_KIND => XmlKind::Text,
                                COMMENT_KIND => XmlKind::Comment,
                                PI_KIND => XmlKind::Pi,
                                _ => XmlKind::Element,
                            };
                            let index = node.id().get() as usize;
                            if index >= self.kinds.len() {
                                self.kinds.resize(index + 1, XmlKind::Element);
                            }
                            self.kinds[index] = kind;
                        }
                    }
                    TopologyBatch::Occurrences(_) => {}
                },
                ReaderPoll::Pending => {
                    resources
                        .try_begin_next_cooperative_entry(4_096)
                        .map_err(CodecError::from)?;
                }
                ReaderPoll::End(_) => break,
            }
        }
        Ok(())
    }

    /// Walks the subtree and gathers every nonempty namespace URI used by an
    /// element or attribute name.
    fn gather_node(
        &mut self,
        document: &Document<'_>,
        node: NodeHandle,
        resources: &ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        // The gather walk recurses per element level; the guard bounds it so
        // a document the iterative decoder accepts cannot abort the process
        // here (the nesting ceiling raises cleanly instead; released on
        // return).
        let _depth = resources.enter_nesting().map_err(CodecError::from)?;
        let id = document
            .resolve_node_handle(node)
            .map_err(|_| jqf_codec_core::data_contract("XML serializer node resolution"))?;
        let Some(name) = self.name_of(document, id)? else {
            return Ok(());
        };
        let (uri, _) = split_clark(&name);
        if !uri.is_empty() {
            self.note_uri(uri);
        }
        for (clark, _) in self.attr_pairs(document, id)? {
            let (uri, _) = split_clark(&clark);
            if !uri.is_empty() {
                self.note_uri(uri);
            }
        }
        let view = document
            .value_view(node)
            .map_err(|_| jqf_codec_core::data_contract("XML serializer element view"))?;
        let array = view
            .array()
            .map_err(|_| jqf_codec_core::data_contract("XML serializer element view"))?;
        let Some(array) = array else {
            return Ok(());
        };
        for child in array.iter() {
            let handle = document
                .node_handle(child.node())
                .map_err(|_| jqf_codec_core::data_contract("XML serializer child handle"))?;
            self.gather_node(document, handle, resources)?;
        }
        Ok(())
    }

    fn note_uri(&mut self, uri: &str) {
        if uri != crate::XML_NAMESPACE && !self.uris.iter().any(|existing| existing == uri) {
            self.uris.push(uri.to_owned());
        }
    }

    /// Assigns `n0`, `n1`, ... to the gathered URIs sorted by UTF-8 bytes.
    /// The XML namespace keeps the predeclared `xml` prefix.
    fn assign_prefixes(&mut self) -> Result<(), CodecError> {
        self.uris.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
        for (index, uri) in self.uris.iter().enumerate() {
            if uri == crate::XML_NAMESPACE {
                continue;
            }
            self.prefixes.insert(uri.clone(), format!("n{index}"));
        }
        Ok(())
    }

    /// The prefix spelling for a namespace URI: empty for the empty
    /// namespace, `xml` for the XML namespace, `nK` for a gathered URI. A URI
    /// that was never gathered is an unrepresentable document shape, so the
    /// lookup fails with a named representation error rather than aborting
    /// the process.
    fn prefix_for(&self, uri: &str) -> Result<&str, CodecError> {
        if uri.is_empty() {
            Ok("")
        } else if uri == crate::XML_NAMESPACE {
            Ok("xml")
        } else {
            self.prefixes
                .get(uri)
                .map(String::as_str)
                .ok_or_else(|| unsupported("a node references a namespace URI the encoder has no prefix for"))
        }
    }

    /// Renders the whole document frame into the encoder's byte buffer.
    fn render(
        &self,
        document: &Document<'_>,
        node: NodeHandle,
        resources: &ResourceContext<'_>,
        out: &mut Vec<u8>,
    ) -> Result<(), CodecError> {
        self.render_node(document, node, true, resources, out)?;
        out.push(b'\n');
        Ok(())
    }

    fn render_node(
        &self,
        document: &Document<'_>,
        node: NodeHandle,
        is_host: bool,
        resources: &ResourceContext<'_>,
        out: &mut Vec<u8>,
    ) -> Result<(), CodecError> {
        // The render walk recurses per element level (through
        // `render_element`); the guard bounds it so a document the iterative
        // decoder accepts cannot abort the process here (the nesting ceiling
        // raises cleanly instead; released on return).
        let _depth = resources.enter_nesting().map_err(CodecError::from)?;
        let id = document
            .resolve_node_handle(node)
            .map_err(|_| jqf_codec_core::data_contract("XML serializer node resolution"))?;
        let kind = self.kinds.get(id.get() as usize).copied().unwrap_or(XmlKind::Element);
        if kind == XmlKind::Element {
            return self.render_element(document, node, is_host, resources, out);
        }
        let view = document
            .value_view(node)
            .map_err(|_| jqf_codec_core::data_contract("XML serializer leaf view"))?;
        let scalar = view
            .scalar()
            .map_err(|_| jqf_codec_core::data_contract("XML serializer leaf scalar"))?;
        let Some(scalar) = scalar else {
            return Err(jqf_codec_core::data_contract("XML serializer non-scalar leaf"));
        };
        let jqf_data::ScalarView::String(text) = scalar else {
            return Err(jqf_codec_core::data_contract("XML serializer non-string leaf"));
        };
        match kind {
            XmlKind::Text => escape_text(text, out),
            XmlKind::Comment => {
                if text.contains("--") || text.ends_with('-') {
                    return Err(unsupported(
                        "a comment containing '--' or ending with '-' cannot be represented in \
                         XML output",
                    ));
                }
                out.extend_from_slice(b"<!--");
                out.extend_from_slice(text.as_bytes());
                out.extend_from_slice(b"-->");
            }
            XmlKind::Pi => {
                // The decode stored the canonical `<?target data?>` spelling;
                // a validated PI never embeds `?>` or CR.
                out.extend_from_slice(text.as_bytes());
            }
            XmlKind::Element => {
                return Err(jqf_codec_core::data_contract("XML serializer leaf kind"));
            }
        }
        Ok(())
    }

    fn render_element(
        &self,
        document: &Document<'_>,
        node: NodeHandle,
        is_host: bool,
        resources: &ResourceContext<'_>,
        out: &mut Vec<u8>,
    ) -> Result<(), CodecError> {
        let id = document
            .resolve_node_handle(node)
            .map_err(|_| jqf_codec_core::data_contract("XML serializer node resolution"))?;
        let name_clark = self.name_of(document, id)?.unwrap_or_default();
        let (uri, local) = split_clark(&name_clark);
        if local.is_empty() {
            return Err(unsupported("the value cannot be represented in the output format"));
        }
        let prefix = self.prefix_for(uri)?;
        out.push(b'<');
        write_qname(out, prefix, local);
        if is_host {
            for (index, uri) in self.uris.iter().enumerate() {
                if uri == crate::XML_NAMESPACE {
                    continue;
                }
                out.extend_from_slice(b" xmlns:n");
                out.extend_from_slice(index.to_string().as_bytes());
                out.extend_from_slice(b"=\"");
                escape_attr(uri, out);
                out.push(b'"');
            }
        }
        let mut sorted_attrs = self.attr_pairs(document, id)?;
        sorted_attrs.sort_by(|a, b| {
            let (a_uri, a_local) = split_clark(&a.0);
            let (b_uri, b_local) = split_clark(&b.0);
            (a_uri.as_bytes(), a_local.as_bytes()).cmp(&(b_uri.as_bytes(), b_local.as_bytes()))
        });
        for (clark, value) in sorted_attrs {
            let (auri, alocal) = split_clark(&clark);
            out.push(b' ');
            write_qname(out, self.prefix_for(auri)?, alocal);
            out.extend_from_slice(b"=\"");
            escape_attr(&value, out);
            out.push(b'"');
        }
        out.push(b'>');
        let view = document
            .value_view(node)
            .map_err(|_| jqf_codec_core::data_contract("XML serializer element view"))?;
        let array = view
            .array()
            .map_err(|_| jqf_codec_core::data_contract("XML serializer element view"))?;
        if let Some(array) = array {
            for child in array.iter() {
                let handle = document
                    .node_handle(child.node())
                    .map_err(|_| jqf_codec_core::data_contract("XML serializer child handle"))?;
                self.render_node(document, handle, false, resources, out)?;
            }
        }
        out.extend_from_slice(b"</");
        write_qname(out, prefix, local);
        out.push(b'>');
        Ok(())
    }
}

/// Writes `[prefix:]local`; the empty prefix writes just `local`.
fn write_qname(out: &mut Vec<u8>, prefix: &str, local: &str) {
    if prefix.is_empty() {
        out.extend_from_slice(local.as_bytes());
    } else {
        out.extend_from_slice(prefix.as_bytes());
        out.push(b':');
        out.extend_from_slice(local.as_bytes());
    }
}

/// Splits a Clark-notation name into `(uri, local)`.
fn split_clark(clark: &str) -> (&str, &str) {
    if let Some(rest) = clark.strip_prefix('{')
        && let Some(end) = rest.find('}')
    {
        return (&rest[..end], &rest[end + 1..]);
    }
    ("", clark)
}

/// Attribute-value escaping: the named references plus the whitespace
/// characters' hex references.
pub(super) fn escape_attr(value: &str, out: &mut Vec<u8>) {
    escape_with::<XmlEscapeAttr>(value, out, escape_attr_byte);
}

/// Character-data escaping: the named references and CR; TAB and LF stay
/// literal.
pub(super) fn escape_text(value: &str, out: &mut Vec<u8>) {
    escape_with::<XmlEscapeText>(value, out, escape_text_byte);
}

fn escape_with<S: StopSet>(value: &str, out: &mut Vec<u8>, escape: fn(u8, &mut Vec<u8>)) {
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let skip = prefix_len::<S>(&bytes[i..]);
        if skip > 0 {
            out.extend_from_slice(&bytes[i..i + skip]);
            i += skip;
            if i >= bytes.len() {
                break;
            }
        }
        escape(bytes[i], out);
        i += 1;
    }
}

fn escape_text_byte(byte: u8, out: &mut Vec<u8>) {
    match byte {
        b'&' => out.extend_from_slice(b"&amp;"),
        b'<' => out.extend_from_slice(b"&lt;"),
        b'>' => out.extend_from_slice(b"&gt;"),
        b'\r' => out.extend_from_slice(b"&#xD;"),
        _ => out.push(byte),
    }
}

fn escape_attr_byte(byte: u8, out: &mut Vec<u8>) {
    match byte {
        b'&' => out.extend_from_slice(b"&amp;"),
        b'<' => out.extend_from_slice(b"&lt;"),
        b'>' => out.extend_from_slice(b"&gt;"),
        b'"' => out.extend_from_slice(b"&quot;"),
        b'\t' => out.extend_from_slice(b"&#x9;"),
        b'\n' => out.extend_from_slice(b"&#xA;"),
        b'\r' => out.extend_from_slice(b"&#xD;"),
        _ => out.push(byte),
    }
}

/// Text-escape stop set: `&`, `<`, `>`, CR.
#[derive(Clone, Copy)]
struct XmlEscapeText;
impl StopSet for XmlEscapeText {
    const EQ: [u8; 8] = [b'&', b'<', b'>', b'\r', 0, 0, 0, 0];
    const EQ_LEN: u8 = 4;
    const LT: Option<u8> = None;
    const GE: Option<u8> = None;
    const ALL: bool = false;
}

/// Attribute-escape stop set: `&`, `<`, `>`, `"`, TAB, LF, CR.
#[derive(Clone, Copy)]
struct XmlEscapeAttr;
impl StopSet for XmlEscapeAttr {
    const EQ: [u8; 8] = [b'&', b'<', b'>', b'"', b'\t', b'\n', b'\r', 0];
    const EQ_LEN: u8 = 7;
    const LT: Option<u8> = None;
    const GE: Option<u8> = None;
    const ALL: bool = false;
}

#[cfg(test)]
mod tests {
    use super::*;
    use jqf_codec_core::{CodecRunContext, DiagnosticPolicy, EncodeRequest};
    use jqf_data::{Array, DialectId, FormatId, ObjectBuilder, ObjectKey, Value};
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

    /// Encodes one owned value through the deterministic profile, returning
    /// the published bytes.
    fn encode_owned(value: &Value) -> Result<Vec<u8>, CodecError> {
        let registration = crate::registration().expect("registration");
        let mut resources = resources();
        let format = FormatId::try_new(crate::FORMAT_ID).expect("format");
        let dialect = DialectId::try_new(crate::XML_DETERMINISTIC_DIALECT_ID).expect("dialect");
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

    fn text(value: &str) -> Value {
        Value::String(jqf_data::Shared::try_from_str(value).expect("string"))
    }

    /// The §1 value mapping: object members become child elements named
    /// by their keys, array items the fixed `item` element, scalars text
    /// runs, `null` an empty element, and the document element is `root`.
    #[test]
    fn the_deterministic_profile_encodes_owned_values() {
        let _resources = resources();
        let mut builder = ObjectBuilder::try_with_capacity(3).expect("builder");
        builder
            .try_insert_last(
                ObjectKey::try_from_str("a").expect("key"),
                Value::Number(
                    jqf_data::Number::try_integer_unaccounted(jqf_data::Integer::parse("1").expect("int"))
                        .expect("number"),
                ),
            )
            .expect("insert");
        builder
            .try_insert_last(ObjectKey::try_from_str("b").expect("key"), Value::Bool(true))
            .expect("insert");
        builder
            .try_insert_last(ObjectKey::try_from_str("c").expect("key"), text("x&y"))
            .expect("insert");
        let object = Value::Object(builder.try_finish().expect("object"));
        let bytes = encode_owned(&object).expect("encode");
        let out = core::str::from_utf8(&bytes).expect("utf8");
        assert_eq!(
            out, "<root><a>1</a><b>true</b><c>x&amp;y</c></root>\n",
            "object lowering"
        );

        let array = Value::Array(
            Array::try_from_vec(vec![
                Value::Number(
                    jqf_data::Number::try_integer_unaccounted(jqf_data::Integer::parse("2").expect("int"))
                        .expect("number"),
                ),
                text("z"),
            ])
            .expect("array"),
        );
        let bytes = encode_owned(&array).expect("encode");
        let out = core::str::from_utf8(&bytes).expect("utf8");
        assert_eq!(out, "<root><item>2</item><item>z</item></root>\n", "array lowering");

        let bytes = encode_owned(&text("hello")).expect("encode");
        let out = core::str::from_utf8(&bytes).expect("utf8");
        assert_eq!(out, "<root>hello</root>\n", "string lowering");
    }

    /// A key that is not a valid XML `Name` is refused, never renamed.
    #[test]
    fn an_invalid_element_key_is_unrepresentable() {
        let _resources = resources();
        let mut builder = ObjectBuilder::try_with_capacity(1).expect("builder");
        builder
            .try_insert_last(ObjectKey::try_from_str("a b").expect("key"), text("v"))
            .expect("insert");
        let object = Value::Object(builder.try_finish().expect("object"));
        let error = encode_owned(&object).expect_err("unrepresentable key");
        assert_eq!(error.kind(), CodecFailureKind::UnsupportedRepresentation);
    }

    #[test]
    fn an_ungathered_namespace_uri_is_a_clean_representation_error() {
        // The gather and render walks must stay in lockstep; if a URI ever
        // reaches the renderer without a gathered prefix, the encode fails
        // with a named representation error instead of aborting the process.
        let serializer = DeterministicSerializer::new();
        let error = serializer
            .prefix_for("http://example.com/ns")
            .expect_err("unbound namespace URI");
        assert_eq!(error.kind(), CodecFailureKind::UnsupportedRepresentation);
        // A gathered URI still resolves to its assigned prefix.
        let mut serializer = DeterministicSerializer::new();
        serializer
            .prefixes
            .insert("http://example.com/ns".to_owned(), "n0".to_owned());
        assert_eq!(
            serializer.prefix_for("http://example.com/ns").expect("gathered URI"),
            "n0"
        );
    }

    #[test]
    fn a_plural_member_range_declines_located_publication() {
        use crate::document::build_from_hit;
        use crate::locate::LocatedHit;

        let bytes = b"<doc><child>a</child><child>b</child></doc>";
        let range = LocatedHit::Range {
            children: vec![
                LocatedHit::Leaf {
                    kind: "text",
                    value: "a".to_owned(),
                },
                LocatedHit::Leaf {
                    kind: "text",
                    value: "b".to_owned(),
                },
            ],
        };
        let mut resources = resources();
        let Err(error) = build_from_hit(bytes, &range, &mut resources) else {
            panic!("a plural member must decline located publication");
        };
        assert_eq!(error.kind(), CodecFailureKind::RequirementMismatch);
    }

    #[test]
    fn start_tag_terminator_names_the_close_outside_quotes() {
        let open = b"<r a=\"1\">t</r>";
        assert_eq!(edit_splice::start_tag_terminator(open, 0, open.len()), Some(8));
        let empty = b"<r/>";
        assert_eq!(edit_splice::start_tag_terminator(empty, 0, empty.len()), Some(2));
        let quoted_gt = br#"<r a=">">t</r>"#;
        assert_eq!(
            edit_splice::start_tag_terminator(quoted_gt, 0, quoted_gt.len()),
            Some(8)
        );
        let single = b"<r a='1'>t</r>";
        assert_eq!(edit_splice::start_tag_terminator(single, 0, single.len()), Some(8));
        let spaced = b"<r />";
        assert_eq!(edit_splice::start_tag_terminator(spaced, 0, spaced.len()), Some(3));
    }

    #[test]
    fn insertable_attribute_name_refuses_clark_prefix_and_xmlns() {
        edit_splice::insertable_attribute_name("id").expect("unprefixed Name");
        edit_splice::insertable_attribute_name("{http://example.com}id").expect_err("clark");
        edit_splice::insertable_attribute_name("foo:bar").expect_err("prefixed");
        edit_splice::insertable_attribute_name("xmlns").expect_err("xmlns");
        edit_splice::insertable_attribute_name("1id").expect_err("not a Name");
        edit_splice::insertable_attribute_name("").expect_err("empty");
    }

    /// A value deeper than the account's nesting ceiling raises at the
    /// serializer's guard (gather/render) instead of aborting the process:
    /// the iterative decoder accepts documents far deeper than one stack
    /// frame per level survives on re-encode.
    #[test]
    fn deep_owned_value_raises_at_the_serializer_guard() {
        let mut deep = text("leaf");
        for _ in 0..2_000 {
            deep = Value::Array(Array::try_from_vec(vec![deep]).expect("array"));
        }
        let mut deep_resources = ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, 512)).expect("account"),
            &CONTROL,
            WorkMeter::try_new_v1(4_096).expect("work"),
        )
        .expect("resources");
        assert!(owned_value::serialize_owned_value(&deep, &mut deep_resources).is_err());
        // Below the ceiling, the same shape encodes.
        let mut shallow = text("leaf");
        for _ in 0..8 {
            shallow = Value::Array(Array::try_from_vec(vec![shallow]).expect("array"));
        }
        let mut shallow_resources = resources();
        assert!(owned_value::serialize_owned_value(&shallow, &mut shallow_resources).is_ok());
    }
}
