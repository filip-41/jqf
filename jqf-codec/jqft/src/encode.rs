//! Canonical jqft text encoder and canonical jqfjson encoder.
//!
//! The jqft encoder emits the canonical form: the `%jqft 1` header on the first stream item, `---` between items,
//! two-space indentation, one comma style, bare keys where legal, JSON-escaped strings, exact numbers canonically,
//! binary64 with the `f` suffix, bytes as `0x"…"`, temporal literals TOML-shaped, and `@tag("name") ` prefixes for
//! retained tag layers. The jqfjson encoder emits strict canonical compact JSON and refuses the values plain JSON
//! cannot spell (bytes, temporals, tags) with a typed error.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::Cell;

use jqf_codec_core::{
    ByteSink, CodecError, CodecFailureKind, CodecRunContext, EncodeItem, EncodeRequest, EncoderFactoryImpl,
    EncoderSession, ErasedEncoderFactory, ErasedEncoderSession, PreservationOutcome, PreservationReport,
    RecycledSessionState,
};
use jqf_data::{
    ArrayView, BatchLimit, Document, FactPayloadView, IntrinsicTagSemantics, NodeId, NumberView, ObjectView,
    ReaderPoll, ScalarView, Value, ValueView, format_binary64,
};
use jqf_resource::{ResourceContext, WorkAdmission};

use crate::json_escape::push_json_escaped;
use crate::provider;

const OFFER_BYTES: usize = 16 * 1024;

/// The attached-fact snapshot the canonical encoder re-spells (the fact-schema freeze): markup node names (`.@name`),
/// the `.&` per-attribute facts, and the `.@comment` payloads. A node carrying a `jqft.name@1` fact IS a markup node —
/// an array of its ordered children (the array-of-children model) — and renders as `<name &attr="v" children…>`; every
/// other array renders as `[…]`. The `jqft.attrs@1` map and the `jqft.content@1` concatenation are PROJECTIONS the
/// grammar re-derives from the attribute facts and the children, so they need no index.
#[derive(Default)]
struct JqftFactIndex {
    /// node -> markup node name (`jqft.name@1`); presence marks a markup node.
    names: BTreeMap<NodeId, String>,
    /// node -> `.&` attributes in source order (`attribute` kind facts; the fact's role is the expanded attribute
    /// name).
    attrs: BTreeMap<NodeId, Vec<(String, String)>>,
    /// node -> the `jqft.comment@1` payload split by role.
    comments: BTreeMap<NodeId, NodeComments>,
}

/// The §3.15 comment roles one node can carry. The `#` line-comment grammar produces `leading` / `inline` / `detached`
/// only; `trailing` and `inner` are unreachable from it and refused at emit (no spelling), never silently dropped.
#[derive(Default)]
struct NodeComments {
    leading: Vec<String>,
    inline: Vec<String>,
    trailing: Vec<String>,
    inner: Vec<String>,
    detached: Vec<String>,
}

/// Splits one `jqft.comment@1` payload into the per-role text lists. The canonical payload shape is `{leading: [{text,
/// style}], …}` — each role a list of `{text, style}` objects; absent roles read `[]`.
fn parse_comment_payload(payload: &FactPayloadView<'_>) -> Option<NodeComments> {
    let FactPayloadView::Map(roles) = payload else {
        return None;
    };
    let mut out = NodeComments::default();
    for (role, entries) in roles.iter() {
        let texts: Vec<String> = match entries {
            FactPayloadView::List(list) => list
                .iter()
                .filter_map(|entry| {
                    let FactPayloadView::Map(fields) = entry else {
                        return None;
                    };
                    fields.iter().find_map(|(field, value)| {
                        if field == "text" {
                            if let FactPayloadView::Text(text) = value {
                                Some(String::from(text))
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    })
                })
                .collect(),
            _ => return None,
        };
        match role {
            "leading" => out.leading = texts,
            "inline" => out.inline = texts,
            "trailing" => out.trailing = texts,
            "inner" => out.inner = texts,
            "detached" => out.detached = texts,
            _ => return None,
        }
    }
    Some(out)
}

/// Renders `coefficient * 10^-scale` as a plain decimal the jqft/jqfjson number grammar re-parses: no exponent, stored
/// digits preserved, and a trailing `.0` on an integer-shaped spelling so a decimal stays a decimal on the way back
/// (`100.0` encodes `100.0`, not `1E+2`).
fn render_decimal_plain(out: &mut Vec<u8>, coefficient: &str, scale: i64) -> Result<(), CodecError> {
    let (negative, digits) = match coefficient.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, coefficient),
    };
    if digits.is_empty() {
        return Err(unrepresentable());
    }
    if negative {
        out.push(b'-');
    }
    let start = out.len();
    if scale <= 0 {
        out.extend_from_slice(digits.as_bytes());
        let zeros = usize::try_from(scale.unsigned_abs()).map_err(|_| unrepresentable())?;
        out.resize(out.len() + zeros, b'0');
    } else {
        let point = usize::try_from(scale).map_err(|_| unrepresentable())?;
        if digits.len() > point {
            let split = digits.len() - point;
            out.extend_from_slice(&digits.as_bytes()[..split]);
            out.push(b'.');
            out.extend_from_slice(&digits.as_bytes()[split..]);
        } else {
            out.extend_from_slice(b"0.");
            out.resize(out.len() + (point - digits.len()), b'0');
            out.extend_from_slice(digits.as_bytes());
        }
    }
    if !out[start..].contains(&b'.') {
        out.extend_from_slice(b".0");
    }
    Ok(())
}

fn unrepresentable() -> CodecError {
    CodecError::new(CodecFailureKind::UnsupportedRepresentation)
}

/// A message-only `UnsupportedRepresentation` naming the missing retention: encode-side failures describe the value,
/// not a source span. If diagnostic construction is refused on resource grounds the bare failure survives, so the error
/// path never makes an unrepresentable document worse.
fn unsupported(message: &str) -> CodecError {
    // The plain carrier builds fallibly; on refusal the bare failure survives, so the error path never makes an
    // unrepresentable document worse.
    let base = CodecError::new(CodecFailureKind::UnsupportedRepresentation);
    let Some(diagnostic) = jqf_source::Diagnostic::try_new(
        jqf_source::Namespace::new("jqft").code("representation"),
        jqf_source::Severity::Error,
        message,
    ) else {
        return base;
    };
    base.with_diagnostic(diagnostic)
}

fn map_data(error: jqf_data::DataError) -> CodecError {
    jqf_codec_core::map_data(error, "jqft encoder document read")
}

/// The `#` grammar spells `leading`/`inline`/`detached` only; a `trailing`/`inner` comment (or a detached comment on a
/// non-root node) has no spelling and is a clean typed error, never a silently thinner file. A free function: the
/// callers hold a borrow of the encoder's fact index.
fn check_unspellable_roles(document: &Document<'_>, node: NodeId, comments: &NodeComments) -> Result<(), CodecError> {
    if !comments.trailing.is_empty() || !comments.inner.is_empty() {
        return Err(unsupported(
            "a trailing or inner comment has no spelling in the jqft # grammar; \
             encode with a codec that carries the role, or strip the fact",
        ));
    }
    if !comments.detached.is_empty() && node != document.root() {
        return Err(unsupported(
            "a detached comment on a non-root node has no spelling in the jqft grammar; \
             only the document trailer can carry one",
        ));
    }
    Ok(())
}

// --------------------------------------------------------------------------- jqft
// ---------------------------------------------------------------------------

pub(crate) fn create_jqft_factory(
    request: EncodeRequest<'_, '_>,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedEncoderFactory, CodecError> {
    request.expect_target(crate::FORMAT_ID, &[crate::JQFT_CANONICAL_DIALECT_ID])?;
    let options = match request.options {
        None => crate::JqftEncodeOptions::default(),
        Some(options) => options
            .downcast_ref::<crate::JqftEncodeOptions>()
            .copied()
            .ok_or_else(|| CodecError::new(CodecFailureKind::RequirementMismatch))?,
    };
    ErasedEncoderFactory::try_new_factory(request.preservation, resources, || {
        Ok(JqftEncoderFactory {
            options,
            first: Cell::new(true),
        })
    })
}

struct JqftEncoderFactory {
    /// The level-composition request (`with_source`).
    options: crate::JqftEncodeOptions,
    /// Whether no item has been emitted yet: the first item opens the `%jqft 1` header, subsequent items open with the
    /// `---` stream separator.
    first: Cell<bool>,
}

impl EncoderFactoryImpl for JqftEncoderFactory {
    fn physical_encoder(&self) -> jqf_codec_core::PhysicalRouteId {
        crate::JQFT_ENCODE_PHYSICAL_ROUTE_ID
    }

    fn start<'item, 'source>(
        &self,
        item: EncodeItem<'item, 'source>,
        _preservation: jqf_codec_core::PreservationRequest,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedEncoderSession<'item, 'source>, CodecError> {
        let leading_header = self.first.replace(false);
        ErasedEncoderSession::try_new(item, jqf_codec_core::PreservationRequest::None, || {
            Ok(JqftEncoder {
                bytes: Vec::new(),
                leading_header,
                root_done: false,
                options: self.options,
                facts: JqftFactIndex::default(),
            })
        })
    }

    fn try_restart(
        &self,
        state: &mut RecycledSessionState<'_>,
        _item: EncodeItem<'_, '_>,
        _preservation: jqf_codec_core::PreservationRequest,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        let Some(encoder) = state.downcast_mut::<JqftEncoder>() else {
            return Ok(false);
        };
        // A recycled encoder must carry the stream-position fact a fresh `start` would have computed: the first item
        // opens the `%jqft 1` header, subsequent items open with the `---` separator, so the recycled item after a
        // completed one never re-emits the header. The factory's `first` cell is the same mutation a fresh `start`
        // performs.
        encoder.reset(self.first.replace(false), self.options);
        Ok(true)
    }
}

struct JqftEncoder {
    bytes: Vec<u8>,
    leading_header: bool,
    root_done: bool,
    options: crate::JqftEncodeOptions,
    /// The attached-fact index of the located item being rendered (empty for owned values, which carry no facts).
    facts: JqftFactIndex,
}

impl JqftEncoder {
    /// Reinitializes one recycled encoder for one more ordered item: drops every byte, flag, and fact index a previous
    /// item may have left behind — including one that aborted mid-offer, whose partial staging must never reach the
    /// next item — leaving exactly the state a fresh [`EncoderFactoryImpl::start`] would have produced.
    fn reset(&mut self, leading_header: bool, options: crate::JqftEncodeOptions) {
        self.bytes.clear();
        self.leading_header = leading_header;
        self.root_done = false;
        self.options = options;
        self.facts = JqftFactIndex::default();
    }

    fn push(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    /// Pushes a JSON-escaped byte string directly into the staging buffer.
    fn push_escaped(&mut self, bytes: &[u8]) {
        push_json_escaped(&mut self.bytes, bytes);
    }

    fn encode_item(&mut self, item: EncodeItem<'_, '_>, resources: &mut ResourceContext<'_>) -> Result<(), CodecError> {
        if self.options.with_source {
            // Level-composition law (§7): `--with-source` requests conformance level 1 — the retained source,
            // byte-identical re-emission of the origin format. The echo is the WHOLE origin document, header included,
            // so no canonical header or stream separator is prefixed. v1's presentation level (2) is the same echo: the
            // retained source IS the spelling-authority presentation of the jqft family (its canonical form is the
            // default below every level). A run without the retention fails cleanly, never publishing a thinner file.
            return match item {
                EncodeItem::Owned(_) => Err(unsupported(
                    "this run cannot supply the source level (with_source): \
                     the item is a computed value with no retained source — encode a document \
                     produced by a source-backed decode",
                )),
                EncodeItem::Located { product, node } => {
                    let document = product.document();
                    if node == document.root_handle()
                        && let Some(segment) = document.source_segment()
                    {
                        self.push(segment);
                        Ok(())
                    } else {
                        Err(unsupported(
                            "this run cannot supply the source level (with_source): \
                             the value is an edited or computed document with no retained source",
                        ))
                    }
                }
            };
        }
        if self.leading_header {
            self.push(b"%jqft 1\n");
        } else {
            self.push(b"---\n");
        }
        match item {
            EncodeItem::Owned(value) => self.render_owned(value, 0, resources),
            EncodeItem::Located { product, node } => {
                let document = product.document();
                self.facts = Self::build_fact_index(document, resources)?;
                let view = document.value_view(node).map_err(map_data)?;
                self.render_root_value(document, view, resources)
            }
        }
    }

    /// Renders a located root value with its attached comments: the leading comments on own lines before the value, the
    /// inline comment on the value's own line, and the document-trailer (detached) comments after it. The §3.15 role
    /// law is preserved — a role the `#` grammar cannot spell is refused, never silently re-rolled as another role.
    fn render_root_value(
        &mut self,
        document: &Document<'_>,
        view: ValueView<'_, '_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        let root_node = view.node();
        self.emit_leading_comments(document, root_node, 0)?;
        self.render_view(document, view, 0, resources)?;
        let inline = self
            .facts
            .comments
            .get(&root_node)
            .map(|comments| comments.inline.clone())
            .unwrap_or_default();
        for text in inline {
            self.push(b" # ");
            self.push(text.as_bytes());
        }
        self.emit_detached_comments(document, root_node)?;
        Ok(())
    }

    /// Emits a node's leading comments (`# text` own-line, at `depth`), and refuses the roles the `#` grammar cannot
    /// spell anywhere.
    fn emit_leading_comments(&mut self, document: &Document<'_>, node: NodeId, depth: usize) -> Result<(), CodecError> {
        let Some(comments) = self.facts.comments.get(&node) else {
            return Ok(());
        };
        check_unspellable_roles(document, node, comments)?;
        let leading: Vec<String> = comments.leading.clone();
        for text in leading {
            self.indent(depth);
            self.push(b"# ");
            self.push(text.as_bytes());
            self.push(b"\n");
        }
        Ok(())
    }

    /// The `#` grammar spells `leading`/`inline`/`detached` only; a `trailing`/`inner` comment (or a detached comment
    /// on a non-root node) has no spelling and is a clean typed error, never a silently thinner file.
    ///
    /// A free function: the callers hold a borrow of `self.facts`.
    fn emit_detached_comments(&mut self, document: &Document<'_>, node: NodeId) -> Result<(), CodecError> {
        let Some(comments) = self.facts.comments.get(&node) else {
            return Ok(());
        };
        check_unspellable_roles(document, node, comments)?;
        if comments.detached.is_empty() {
            return Ok(());
        }
        let detached: Vec<String> = comments.detached.clone();
        // The detached comments begin on their own line (the root value render never ends with a newline).
        self.push(b"\n");
        for text in detached {
            self.push(b"# ");
            self.push(text.as_bytes());
            self.push(b"\n");
        }
        Ok(())
    }

    /// Builds the attached-fact index for a located document: markup names, `.&` attributes, and comment payloads.
    fn build_fact_index(
        document: &Document<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<JqftFactIndex, CodecError> {
        let mut index = JqftFactIndex::default();
        let mut reader = match document.fact_reader(resources) {
            Ok(reader) => reader,
            // A document from owned values or a fact-less codec carries no attached facts; the index stays empty and
            // the render is plain.
            Err(jqf_data::DataError::CapabilityUnavailable {
                capability: jqf_data::DocumentCapability::AttachedFacts,
            }) => return Ok(index),
            Err(_) => {
                return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                    contract: "jqft fact index over a valid document",
                }));
            }
        };
        let limit = BatchLimit::new(usize::MAX).ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?;
        loop {
            let poll = reader.poll_batch(limit, resources).map_err(|_| {
                CodecError::new(CodecFailureKind::InternalContractViolation {
                    contract: "jqft fact index read",
                })
            })?;
            match poll {
                ReaderPoll::Batch(batch) => {
                    for fact in batch.iter() {
                        let jqf_data::LocalOwnerRef::Node(node) = fact.owner() else {
                            continue;
                        };
                        match fact.role().as_str() {
                            // One fact per `.&` attribute: the role is the fixed `attribute`, the KIND is the expanded
                            // attribute name (the parser's `add_fact` order).
                            provider::ATTRIBUTE_FACT => {
                                if let FactPayloadView::Text(text) = fact.payload() {
                                    index
                                        .attrs
                                        .entry(node)
                                        .or_default()
                                        .push((String::from(fact.kind().as_str()), String::from(text)));
                                }
                            }
                            provider::JQFT_NAME_FACT => {
                                if let FactPayloadView::Text(text) = fact.payload() {
                                    index.names.insert(node, String::from(text));
                                }
                            }
                            // One fact per `.@`-addressable comment set is the role-keyed MAP: the flat siblings
                            // (`jqft.comment@1` etc.) are plain text lists the encoder cannot re-spell `{text, style}`
                            // from.
                            provider::JQFT_COMMENT_MAP_FACT => {
                                let comments = parse_comment_payload(&fact.payload())
                                    .ok_or_else(|| unsupported("a comment fact is not the role-keyed map"))?;
                                index.comments.insert(node, comments);
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
        Ok(index)
    }

    /// Renders one markup node: `<name &attr="v" children…>` (§3.6, the angle form). Children are nodes and strings
    /// ONLY (the membrane); a tagged child, a container without a name, or a comment role with no position inside the
    /// single-line form is refused.
    fn render_markup(
        &mut self,
        document: &Document<'_>,
        node: NodeId,
        array: ArrayView<'_, '_>,
        name: &str,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        // Markup nests within markup recursively; the guard bounds the recursion so a document the iterative parser
        // accepts cannot blow the stack on re-encode (the depth ceiling raises cleanly instead).
        let _depth = resources.enter_nesting_owned().map_err(CodecError::from)?;
        self.push(b"<");
        self.push(name.as_bytes());
        let attrs: Vec<(String, String)> = self.facts.attrs.get(&node).cloned().unwrap_or_default();
        for (attr, value) in &attrs {
            self.push(b" &");
            self.push_attr_name(attr);
            self.push(b"=\"");
            self.push_escaped(value.as_bytes());
            self.push(b"\"");
        }
        let items: Vec<ValueView<'_, '_>> = array.iter().collect();
        for item in items {
            let child = item.node();
            if let Some(comments) = self.facts.comments.get(&child) {
                check_unspellable_roles(document, child, comments)?;
                // No comment role has a spelling position inside the single-line markup form: an own-line `#` would
                // need a line break, and an inline ` # c` would swallow the rest of the line (the `#` rule runs to end
                // of line) — so the child's next sibling would be eaten. Refuse rather than emit a line that re-decodes
                // to different facts.
                if !comments.leading.is_empty() || !comments.inline.is_empty() {
                    return Err(unsupported(
                        "a comment on a markup child has no spelling position inside the \
                         single-line markup form; move it before the markup node",
                    ));
                }
            }
            if item.tag_semantics().map_err(map_data)? == Some(IntrinsicTagSemantics::Tagged) {
                return Err(unsupported(
                    "the markup membrane: a tagged value cannot be represented inside markup",
                ));
            }
            self.push(b" ");
            match item.kind().map_err(map_data)? {
                jqf_data::ValueKind::String => {
                    let ScalarView::String(text) = item.scalar().map_err(map_data)?.ok_or_else(unrepresentable)? else {
                        return Err(unrepresentable());
                    };
                    self.push_quoted(text.as_bytes());
                }
                jqf_data::ValueKind::Array => {
                    let Some(name) = self.facts.names.get(&child).cloned() else {
                        return Err(unsupported(
                            "the markup membrane: a container child with no node name cannot \
                             be represented inside markup",
                        ));
                    };
                    let array = item.array().map_err(map_data)?.ok_or_else(unrepresentable)?;
                    self.render_markup(document, child, array, &name, resources)?;
                }
                _ => {
                    return Err(unsupported(
                        "the markup membrane: markup children are nodes and strings only; a \
                         bare scalar or container cannot be represented in a markup target",
                    ));
                }
            }
        }
        self.push(b">");
        Ok(())
    }

    /// The attribute-name spelling: a bare `&name` where the identifier grammar permits it, the quoted bracket form
    /// `&["name"]` otherwise (the accessor twin of `&["aria-label"]`).
    fn push_attr_name(&mut self, name: &str) {
        let mut bare = !name.is_empty();
        let mut bytes = name.bytes();
        match bytes.next() {
            Some(first) if first.is_ascii_alphabetic() || first == b'_' => {}
            _ => bare = false,
        }
        if bare {
            for byte in bytes {
                if !(byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-') {
                    bare = false;
                    break;
                }
            }
        }
        if bare {
            self.push(name.as_bytes());
        } else {
            self.push(b"[\"");
            self.push_escaped(name.as_bytes());
            self.push(b"\"]");
        }
    }

    fn indent(&mut self, depth: usize) {
        for _ in 0..depth {
            self.push(b"  ");
        }
    }

    fn render_owned(
        &mut self,
        value: &Value,
        depth: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        // The text encoder recurses per container level; the guard bounds the recursion so a document the iterative
        // parser accepts cannot blow the stack on re-encode (the depth ceiling raises cleanly instead, exactly as the
        // jqfb encoder's walk guards do).
        let _depth = resources.enter_nesting_owned().map_err(CodecError::from)?;
        match value {
            Value::Null => {
                self.push(b"null");
                Ok(())
            }
            Value::Bool(true) => {
                self.push(b"true");
                Ok(())
            }
            Value::Bool(false) => {
                self.push(b"false");
                Ok(())
            }
            Value::Number(number) => self.render_number_owned(number, resources),
            Value::String(text) => {
                self.push_quoted(text.as_bytes());
                Ok(())
            }
            Value::Bytes(bytes) => {
                self.push_hex(bytes.as_ref());
                Ok(())
            }
            Value::LocalDate(date) => self.push_temporal(|out| date.write_text(out)),
            Value::LocalTime(time) => self.push_temporal(|out| time.write_text(out)),
            Value::LocalDateTime(datetime) => self.push_temporal(|out| datetime.write_text(out)),
            Value::OffsetDateTime(datetime) => self.push_temporal(|out| datetime.write_text(out)),
            Value::Tagged { tag, payload } => {
                self.push(b"@tag(\"");
                self.push_escaped(tag.as_str().as_bytes());
                self.push(b"\") ");
                self.render_owned(payload, depth, resources)
            }
            Value::Array(array) => {
                if array.is_empty() {
                    self.push(b"[]");
                    return Ok(());
                }
                self.push(b"[\n");
                for (index, item) in array.iter().enumerate() {
                    self.indent(depth + 1);
                    self.render_owned(item, depth + 1, resources)?;
                    self.push(if index + 1 < array.len() { b",\n" } else { b"\n" });
                }
                self.indent(depth);
                self.push(b"]");
                Ok(())
            }
            Value::Object(object) => {
                if object.is_empty() {
                    self.push(b"{}");
                    return Ok(());
                }
                self.push(b"{\n");
                for (index, entry) in object.iter().enumerate() {
                    self.indent(depth + 1);
                    self.push_key(entry.key());
                    self.push(b": ");
                    self.render_owned(entry.value(), depth + 1, resources)?;
                    self.push(if index + 1 < object.len() { b",\n" } else { b"\n" });
                }
                self.indent(depth);
                self.push(b"}");
                Ok(())
            }
        }
    }

    fn render_number_owned(
        &mut self,
        number: &jqf_data::Number,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        // The inline machine arm renders its canonical spelling on demand demand; the boxed arm borrows its retained
        // one.
        if let Some(machine) = number.as_machine() {
            let integer = jqf_data::Integer::from_i64(machine);
            self.push(integer.as_str().as_bytes());
            return Ok(());
        }
        if let Some(integer) = number.as_integer() {
            self.push(integer.as_str().as_bytes());
            return Ok(());
        }
        if let Some(decimal) = number.as_decimal() {
            return render_decimal_plain(&mut self.bytes, decimal.coefficient().as_str(), decimal.scale());
        }
        if let Some(float) = number.as_float() {
            return self.push_float(float.get());
        }
        Err(unrepresentable())
    }

    fn push_float(&mut self, value: f64) -> Result<(), CodecError> {
        if value.is_nan() {
            self.push(b"nan");
            return Ok(());
        }
        if value == f64::INFINITY {
            self.push(b"inf");
            return Ok(());
        }
        if value == f64::NEG_INFINITY {
            self.push(b"-inf");
            return Ok(());
        }
        let text = format_binary64(value).ok_or_else(unrepresentable)?;
        self.push(text.as_str().as_bytes());
        self.push(b"f");
        Ok(())
    }

    fn push_hex(&mut self, bytes: &[u8]) {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        self.push(b"0x\"");
        for &byte in bytes {
            let pair = [HEX[((byte >> 4) & 15) as usize], HEX[(byte & 15) as usize]];
            self.push(&pair);
        }
        self.push(b"\"");
    }

    fn push_temporal(
        &mut self,
        write: impl FnOnce(&mut String) -> Result<(), jqf_data::TemporalError>,
    ) -> Result<(), CodecError> {
        let mut text = String::new();
        write(&mut text).map_err(|_| unrepresentable())?;
        self.push(text.as_bytes());
        Ok(())
    }

    fn push_quoted(&mut self, bytes: &[u8]) {
        self.push(b"\"");
        self.push_escaped(bytes);
        self.push(b"\"");
    }

    fn push_key(&mut self, key: &str) {
        let mut bare = !key.is_empty();
        let mut bytes = key.bytes();
        match bytes.next() {
            Some(first) if first.is_ascii_alphabetic() || first == b'_' => {}
            _ => bare = false,
        }
        if bare {
            for byte in bytes {
                if !(byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-') {
                    bare = false;
                    break;
                }
            }
        }
        if bare {
            self.push(key.as_bytes());
        } else {
            self.push_quoted(key.as_bytes());
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one value dispatch table per renderer: every core kind's spelling sits beside the others"
    )]
    fn render_view(
        &mut self,
        document: &Document<'_>,
        view: ValueView<'_, '_>,
        depth: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        // The located renderer recurses per container level (via the array/object arms and the tagged-payload arm);
        // the guard bounds the recursion against the same depth ceiling as the owned renderer.
        let _depth = resources.enter_nesting_owned().map_err(CodecError::from)?;
        if view.tag_semantics().map_err(map_data)? == Some(jqf_data::IntrinsicTagSemantics::Tagged) {
            let tag = view.tag().map_err(map_data)?.ok_or_else(unrepresentable)?;
            self.push(b"@tag(\"");
            self.push_escaped(tag.as_str().as_bytes());
            self.push(b"\") ");
            let payload = document
                .tag_payload(view.node())
                .map_err(map_data)?
                .ok_or_else(unrepresentable)?;
            let handle = document.node_handle(payload).map_err(map_data)?;
            let payload = document.value_view(handle).map_err(map_data)?;
            return self.render_view(document, payload, depth, resources);
        }
        match view.kind().map_err(map_data)? {
            jqf_data::ValueKind::Null => {
                self.push(b"null");
                Ok(())
            }
            jqf_data::ValueKind::Bool => {
                let ScalarView::Bool(value) = view.scalar().map_err(map_data)?.ok_or_else(unrepresentable)? else {
                    return Err(unrepresentable());
                };
                self.push(if value { b"true" } else { b"false" });
                Ok(())
            }
            jqf_data::ValueKind::Number => {
                let ScalarView::Number(number) = view.scalar().map_err(map_data)?.ok_or_else(unrepresentable)? else {
                    return Err(unrepresentable());
                };
                self.render_number_view(number, resources)
            }
            jqf_data::ValueKind::String => {
                let ScalarView::String(text) = view.scalar().map_err(map_data)?.ok_or_else(unrepresentable)? else {
                    return Err(unrepresentable());
                };
                self.push_quoted(text.as_bytes());
                Ok(())
            }
            jqf_data::ValueKind::Bytes => {
                let ScalarView::Bytes(bytes) = view.scalar().map_err(map_data)?.ok_or_else(unrepresentable)? else {
                    return Err(unrepresentable());
                };
                self.push_hex(bytes);
                Ok(())
            }
            jqf_data::ValueKind::LocalDate => {
                let ScalarView::LocalDate(date) = view.scalar().map_err(map_data)?.ok_or_else(unrepresentable)? else {
                    return Err(unrepresentable());
                };
                self.push_temporal(|out| date.write_text(out))
            }
            jqf_data::ValueKind::LocalTime => {
                let ScalarView::LocalTime(time) = view.scalar().map_err(map_data)?.ok_or_else(unrepresentable)? else {
                    return Err(unrepresentable());
                };
                self.push_temporal(|out| time.write_text(out))
            }
            jqf_data::ValueKind::LocalDateTime => {
                let ScalarView::LocalDateTime(datetime) =
                    view.scalar().map_err(map_data)?.ok_or_else(unrepresentable)?
                else {
                    return Err(unrepresentable());
                };
                self.push_temporal(|out| datetime.write_text(out))
            }
            jqf_data::ValueKind::OffsetDateTime => {
                let ScalarView::OffsetDateTime(datetime) =
                    view.scalar().map_err(map_data)?.ok_or_else(unrepresentable)?
                else {
                    return Err(unrepresentable());
                };
                self.push_temporal(|out| datetime.write_text(out))
            }
            jqf_data::ValueKind::Array => {
                let array = view.array().map_err(map_data)?.ok_or_else(unrepresentable)?;
                if let Some(name) = self.facts.names.get(&view.node()).cloned() {
                    // A node with a `jqft.name@1` fact IS a markup node (the array model: children are its array
                    // items).
                    return self.render_markup(document, view.node(), array, &name, resources);
                }
                self.render_view_array(document, array, depth, resources)
            }
            jqf_data::ValueKind::Object => {
                let object = view.object().map_err(map_data)?.ok_or_else(unrepresentable)?;
                self.render_view_object(document, object, depth, resources)
            }
        }
    }

    fn render_view_array(
        &mut self,
        document: &Document<'_>,
        array: ArrayView<'_, '_>,
        depth: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        if array.is_empty() {
            self.push(b"[]");
            return Ok(());
        }
        self.push(b"[\n");
        let len = array.len();
        for index in 0..len {
            let item = array.get(index).ok_or_else(unrepresentable)?;
            let node = item.node();
            self.emit_leading_comments(document, node, depth + 1)?;
            self.indent(depth + 1);
            self.render_view(document, item, depth + 1, resources)?;
            let inline_len = self
                .facts
                .comments
                .get(&node)
                .map_or(0, |comments| comments.inline.len());
            if inline_len > 0 {
                if index + 1 < len {
                    self.push(b",");
                }
                let mut inline = Vec::new();
                if let Some(comments) = self.facts.comments.get(&node) {
                    for text in &comments.inline {
                        inline.extend_from_slice(b" # ");
                        inline.extend_from_slice(text.as_bytes());
                    }
                }
                self.push(&inline);
                self.push(b"\n");
            } else if index + 1 < len {
                self.push(b",\n");
            } else {
                self.push(b"\n");
            }
        }
        self.indent(depth);
        self.push(b"]");
        Ok(())
    }

    fn render_view_object(
        &mut self,
        document: &Document<'_>,
        object: ObjectView<'_, '_>,
        depth: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        if object.is_empty() {
            self.push(b"{}");
            return Ok(());
        }
        self.push(b"{\n");
        let len = object.len();
        for index in 0..len {
            let entry = object.get_index(index).map_err(map_data)?.ok_or_else(unrepresentable)?;
            let value = entry.value();
            // An own-line comment before an entry is the VALUE's leading comment (§3.15 attaches it to the next
            // completed node); it sits on its own lines above the entry.
            self.emit_leading_comments(document, value.node(), depth + 1)?;
            self.indent(depth + 1);
            self.push_key(entry.key());
            self.push(b": ");
            self.render_view(document, value, depth + 1, resources)?;
            let node = value.node();
            let inline_len = self
                .facts
                .comments
                .get(&node)
                .map_or(0, |comments| comments.inline.len());
            if inline_len > 0 {
                if index + 1 < len {
                    self.push(b",");
                }
                let mut inline = Vec::new();
                if let Some(comments) = self.facts.comments.get(&node) {
                    for text in &comments.inline {
                        inline.extend_from_slice(b" # ");
                        inline.extend_from_slice(text.as_bytes());
                    }
                }
                self.push(&inline);
                self.push(b"\n");
            } else if index + 1 < len {
                self.push(b",\n");
            } else {
                self.push(b"\n");
            }
        }
        self.indent(depth);
        self.push(b"}");
        Ok(())
    }

    fn render_number_view(
        &mut self,
        number: NumberView<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        match number {
            NumberView::Number(number) => self.render_number_owned(number, resources),
            NumberView::Integer(text) => {
                self.push(text.as_bytes());
                Ok(())
            }
            NumberView::Decimal { coefficient, scale } => render_decimal_plain(&mut self.bytes, coefficient, scale),
            NumberView::Float(value) => self.push_float(value.get()),
        }
    }
}

impl EncoderSession for JqftEncoder {
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
                return Ok(Self::report());
            }
            if self.bytes.len() >= OFFER_BYTES {
                sink.write_all(&self.bytes, context.resources())?;
                self.bytes.clear();
                continue;
            }
            let remaining = context.resources().remaining_work() as usize;
            match context.resources().admit_work_transitions(remaining.max(1))? {
                WorkAdmission::Pending => context.replenish_work()?,
                WorkAdmission::Granted(_granted) => {
                    self.encode_item(item, context.resources())?;
                    self.root_done = true;
                }
            }
        }
    }
}

impl JqftEncoder {
    fn report() -> jqf_codec_core::PreservationReport {
        jqf_codec_core::PreservationReport::new(
            PreservationOutcome::Exact,
            PreservationOutcome::Omitted,
            PreservationOutcome::Exact,
            PreservationOutcome::Normalized,
        )
    }
}

// --------------------------------------------------------------------------- jqfjson
// ---------------------------------------------------------------------------

pub(crate) fn create_jqfjson_factory(
    request: EncodeRequest<'_, '_>,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedEncoderFactory, CodecError> {
    if request.format.as_str() != crate::JQFJSON_FORMAT_ID
        || request.dialect.as_str() != crate::JQFJSON_CANONICAL_DIALECT_ID
    {
        return Err(CodecError::new(CodecFailureKind::RequirementMismatch));
    }
    ErasedEncoderFactory::try_new_factory(request.preservation, resources, || Ok(JqfjsonEncoderFactory))
}

struct JqfjsonEncoderFactory;

impl EncoderFactoryImpl for JqfjsonEncoderFactory {
    fn physical_encoder(&self) -> jqf_codec_core::PhysicalRouteId {
        crate::JQFJSON_ENCODE_PHYSICAL_ROUTE_ID
    }

    fn start<'item, 'source>(
        &self,
        item: EncodeItem<'item, 'source>,
        _preservation: jqf_codec_core::PreservationRequest,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedEncoderSession<'item, 'source>, CodecError> {
        ErasedEncoderSession::try_new(item, jqf_codec_core::PreservationRequest::None, || {
            Ok(JqfjsonEncoder {
                bytes: Vec::new(),
                root_done: false,
            })
        })
    }

    fn try_restart(
        &self,
        state: &mut RecycledSessionState<'_>,
        _item: EncodeItem<'_, '_>,
        _preservation: jqf_codec_core::PreservationRequest,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        let Some(encoder) = state.downcast_mut::<JqfjsonEncoder>() else {
            return Ok(false);
        };
        encoder.reset();
        Ok(true)
    }
}

struct JqfjsonEncoder {
    bytes: Vec<u8>,
    root_done: bool,
}

impl JqfjsonEncoder {
    /// Reinitializes one recycled encoder for one more ordered item: drops every byte and flag a previous item may have
    /// left behind — including one that aborted mid-offer, whose partial staging must never reach the next item —
    /// leaving exactly the state a fresh [`EncoderFactoryImpl::start`] would have produced.
    fn reset(&mut self) {
        self.bytes.clear();
        self.root_done = false;
    }

    fn push(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    fn push_quoted(&mut self, bytes: &[u8]) {
        self.push(b"\"");
        push_json_escaped(&mut self.bytes, bytes);
        self.push(b"\"");
    }

    fn encode_item(&mut self, item: EncodeItem<'_, '_>, resources: &mut ResourceContext<'_>) -> Result<(), CodecError> {
        match item {
            EncodeItem::Owned(value) => self.render_owned(value, resources),
            EncodeItem::Located { product, node } => {
                let document = product.document();
                let view = document.value_view(node).map_err(map_data)?;
                self.render_view(document, view, resources)
            }
        }
    }

    fn render_owned(&mut self, value: &Value, resources: &mut ResourceContext<'_>) -> Result<(), CodecError> {
        // The strict-JSON envelope is parsed by the same ITERATIVE parser as jqft, so a deeply nested document can
        // reach this recursive renderer; the guard raises the depth ceiling cleanly instead of overflowing the stack.
        let _depth = resources.enter_nesting_owned().map_err(CodecError::from)?;
        match value {
            Value::Null => {
                self.push(b"null");
                Ok(())
            }
            Value::Bool(true) => {
                self.push(b"true");
                Ok(())
            }
            Value::Bool(false) => {
                self.push(b"false");
                Ok(())
            }
            Value::Number(number) => self.render_number(number, resources),
            Value::String(text) => {
                self.push_quoted(text.as_bytes());
                Ok(())
            }
            Value::Array(array) => {
                self.push(b"[");
                for (index, item) in array.iter().enumerate() {
                    if index > 0 {
                        self.push(b",");
                    }
                    self.render_owned(item, resources)?;
                }
                self.push(b"]");
                Ok(())
            }
            Value::Object(object) => {
                self.push(b"{");
                for (index, entry) in object.iter().enumerate() {
                    if index > 0 {
                        self.push(b",");
                    }
                    self.push_quoted(entry.key().as_bytes());
                    self.push(b":");
                    self.render_owned(entry.value(), resources)?;
                }
                self.push(b"}");
                Ok(())
            }
            Value::Bytes(_)
            | Value::LocalDate(_)
            | Value::LocalTime(_)
            | Value::LocalDateTime(_)
            | Value::OffsetDateTime(_)
            | Value::Tagged { .. } => Err(unrepresentable()),
        }
    }

    fn render_view(
        &mut self,
        document: &Document<'_>,
        view: ValueView<'_, '_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        let _ = document;
        let _depth = resources.enter_nesting_owned().map_err(CodecError::from)?;
        if view.tag_semantics().map_err(map_data)? == Some(jqf_data::IntrinsicTagSemantics::Tagged) {
            return Err(unrepresentable());
        }
        match view.kind().map_err(map_data)? {
            jqf_data::ValueKind::Null => {
                self.push(b"null");
                Ok(())
            }
            jqf_data::ValueKind::Bool => {
                let ScalarView::Bool(value) = view.scalar().map_err(map_data)?.ok_or_else(unrepresentable)? else {
                    return Err(unrepresentable());
                };
                self.push(if value { b"true" } else { b"false" });
                Ok(())
            }
            jqf_data::ValueKind::Number => {
                let ScalarView::Number(number) = view.scalar().map_err(map_data)?.ok_or_else(unrepresentable)? else {
                    return Err(unrepresentable());
                };
                self.render_number_view(number, resources)
            }
            jqf_data::ValueKind::String => {
                let ScalarView::String(text) = view.scalar().map_err(map_data)?.ok_or_else(unrepresentable)? else {
                    return Err(unrepresentable());
                };
                self.push_quoted(text.as_bytes());
                Ok(())
            }
            jqf_data::ValueKind::Array => {
                let array = view.array().map_err(map_data)?.ok_or_else(unrepresentable)?;
                self.push(b"[");
                for index in 0..array.len() {
                    if index > 0 {
                        self.push(b",");
                    }
                    let item = array.get(index).ok_or_else(unrepresentable)?;
                    self.render_view(document, item, resources)?;
                }
                self.push(b"]");
                Ok(())
            }
            jqf_data::ValueKind::Object => {
                let object = view.object().map_err(map_data)?.ok_or_else(unrepresentable)?;
                self.push(b"{");
                for index in 0..object.len() {
                    if index > 0 {
                        self.push(b",");
                    }
                    let entry = object.get_index(index).map_err(map_data)?.ok_or_else(unrepresentable)?;
                    self.push_quoted(entry.key().as_bytes());
                    self.push(b":");
                    self.render_view(document, entry.value(), resources)?;
                }
                self.push(b"}");
                Ok(())
            }
            jqf_data::ValueKind::Bytes
            | jqf_data::ValueKind::LocalDate
            | jqf_data::ValueKind::LocalTime
            | jqf_data::ValueKind::LocalDateTime
            | jqf_data::ValueKind::OffsetDateTime => Err(unrepresentable()),
        }
    }

    fn render_number(
        &mut self,
        number: &jqf_data::Number,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        // The inline machine arm renders its canonical spelling on demand demand; the boxed arm borrows its retained
        // one.
        if let Some(machine) = number.as_machine() {
            let integer = jqf_data::Integer::from_i64(machine);
            self.push(integer.as_str().as_bytes());
            return Ok(());
        }
        if let Some(integer) = number.as_integer() {
            self.push(integer.as_str().as_bytes());
            return Ok(());
        }
        if let Some(decimal) = number.as_decimal() {
            return render_decimal_plain(&mut self.bytes, decimal.coefficient().as_str(), decimal.scale());
        }
        // A binary64 cannot be spelled in plain JSON: the envelope's float spelling is a later pass.
        Err(unrepresentable())
    }

    fn render_number_view(
        &mut self,
        number: NumberView<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        match number {
            NumberView::Number(number) => self.render_number(number, resources),
            NumberView::Integer(text) => {
                self.push(text.as_bytes());
                Ok(())
            }
            NumberView::Decimal { coefficient, scale } => render_decimal_plain(&mut self.bytes, coefficient, scale),
            NumberView::Float(_) => Err(unrepresentable()),
        }
    }
}

impl EncoderSession for JqfjsonEncoder {
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
                return Ok(Self::report());
            }
            if self.bytes.len() >= OFFER_BYTES {
                sink.write_all(&self.bytes, context.resources())?;
                self.bytes.clear();
                continue;
            }
            let remaining = context.resources().remaining_work() as usize;
            match context.resources().admit_work_transitions(remaining.max(1))? {
                WorkAdmission::Pending => context.replenish_work()?,
                WorkAdmission::Granted(_granted) => {
                    self.encode_item(item, context.resources())?;
                    self.root_done = true;
                }
            }
        }
    }
}

impl JqfjsonEncoder {
    fn report() -> jqf_codec_core::PreservationReport {
        jqf_codec_core::PreservationReport::new(
            PreservationOutcome::Exact,
            PreservationOutcome::Omitted,
            PreservationOutcome::Exact,
            PreservationOutcome::Normalized,
        )
    }
}
