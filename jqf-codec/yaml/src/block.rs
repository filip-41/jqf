//! `yaml.block@1`: the human-readable block dialect, and the default for `--output-format yaml`.
//!
//! The two canonical dialects answer a machine: every node carries an explicit standard tag, every scalar is
//! double-quoted, and containers are flow-style with one trailing comma per item. That is byte-determinism, and it is
//! the right answer for a diff or a hash. It is the wrong answer for a person, who reads YAML precisely because it does
//! not look like that.
//!
//! This dialect emits block maps (`name: ann`), sequence items under their key at a two-space indent with `- `, plain
//! scalars wherever the YAML 1.2 core schema round-trips them, and quotes ONLY where they are load-bearing. The
//! canonical dialects are untouched and stay reachable by name — determinism did not get worse, it got a name.
//!
//! # The quoting rule
//!
//! A single-line string is emitted PLAIN when reading it back yields the same string, and double-quoted otherwise. It
//! is quoted when it is empty, when it has leading or trailing whitespace, when it holds a control character, when its
//! first character is a YAML indicator, when it contains `: ` or ` #` or ends with `:` (each of which would end the
//! scalar early), or when a reader would resolve it as something other than a string: `null`/`~`, a boolean in any of
//! its spellings, a core-schema number, or a date/time.
//!
//! A scalar directly under a non-core TAG skips that last test. An explicit tag overrides resolution, so `!money 12.5`
//! reads back as the `!money` tag over the text `12.5` — the quotes would be noise, and this codec's own decoder proves
//! the round trip.
//!
//! # The multiline rule
//!
//! A string containing a newline is emitted as a LITERAL block scalar when its content survives that form byte for
//! byte, and double-quoted otherwise. Literal form requires: no control character other than the newlines themselves (a
//! `\r` or a tab would be lost or re-indented), no line with leading or trailing space (indentation is the block
//! scalar's own syntax, and trailing space is not preserved), and at most ONE trailing newline — the only two chomping
//! states this dialect spells. The chomping indicator says which: `|` keeps the single trailing newline, `|-` strips it
//! when the content has none. Anything else — `\r\n` line endings, a blank final line, an indented first line — is
//! quoted, where every byte is explicit.
//!
//! # Composition with the encode-projection policy
//!
//! Tags emit NATIVELY here (`v: !money 12.5`): YAML is a format with tags, so nothing is projected and nothing is
//! warned about. Temporals still project to RFC 3339 text with their warning, because the YAML core schema has no
//! timestamp type — and the projected text is QUOTED, since an unquoted `2026-06-15` is exactly the text a reader that
//! carries the type would resolve back into something else. Byte strings project to base64url for the same reason the
//! canonical dialects do: `!!binary` is a spelling this codec decodes to a non-core TAGGED value rather than to
//! `Bytes`, so emitting it would not round-trip to what was read.
//!
//! Object key order is the value's own order. It is never sorted: a map's order is data here, and this dialect exists
//! to show the data.
//!
//! # The stream shape
//!
//! `---` goes BETWEEN documents, never before the first, and no document is terminated with `...`. A renderer here
//! writes no trailing newline either: the publication facade terminates every item, and a second LF would open a blank
//! line inside the stream.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;

use jqf_codec_core::{CodecError, CodecFailureKind, DocumentProduct, NativeSpellings, ProjectionSink, classify_scalar};
use jqf_data::{
    BatchLimit, Document, FactPayloadView, IntrinsicTagSemantics, NodeId, ReaderPoll, ScalarView, Value, ValueKind,
    ValueView,
};
use jqf_resource::ResourceContext;

use crate::document::{ANCHOR_FACT, COMMENT_FACT, COMMENT_FOOT_FACT, COMMENT_INLINE_FACT};
use crate::encode::{map_data, number_text, number_view_text, unrepresentable, write_escaped};

/// Every node's comment texts by position, indexed once per document.
///
/// The decoder attaches one `yaml.comment@1` fact per node whose leading comments it recorded, a
/// `yaml.comment_inline@1` for a same-line comment, and a `yaml.comment_foot@1` for a comment below a closing block: a
/// leading comment always finds the node it precedes; a same-line comment is the value it trails; a comment on its own
/// line deeper than the next node's column is a foot — the closing block's, or the ROOT's when no node follows.
/// Re-emitting the leading set before each node, the inline set after each value on its own line, and the foot set
/// after each block — with the root's foot as the document trailer — is what makes `jqf --input-format yaml
/// --output-format yaml` a config editor rather than a config destroyer: without it, every comment in the file is
/// deleted by any edit.
#[derive(Default)]
struct CommentIndex {
    leading: BTreeMap<NodeId, Vec<String>>,
    inline: BTreeMap<NodeId, Vec<String>>,
    foot: BTreeMap<NodeId, Vec<String>>,
}

impl CommentIndex {
    fn new() -> Self {
        Self::default()
    }
}

/// Builds the comment index for one located document.
///
/// A document from owned values, or one whose codec attaches no facts, has no fact reader: the index stays empty and
/// the render is exactly what it was before comments were preserved.
fn ingest_comment_fact(index: &mut CommentIndex, fact: jqf_data::DocumentFact<'_>) {
    let jqf_data::LocalOwnerRef::Node(node) = fact.owner() else {
        return;
    };
    let role = fact.role().as_str();
    let map = if role == COMMENT_FACT {
        &mut index.leading
    } else if role == COMMENT_INLINE_FACT {
        &mut index.inline
    } else if role == COMMENT_FOOT_FACT {
        &mut index.foot
    } else {
        return;
    };
    let FactPayloadView::List(texts) = fact.payload() else {
        return;
    };
    let texts: Vec<String> = texts
        .iter()
        .filter_map(|entry| match entry {
            FactPayloadView::Text(text) => Some(String::from(text)),
            _ => None,
        })
        .collect();
    if !texts.is_empty() {
        map.insert(node, texts);
    }
}

/// The shared skeleton of the comment and anchor indexes: walk one document's attached facts through `ingest`, via the
/// owner-indexed fast path when the document has one, else a batched reader loop. A document from owned values, or one
/// whose codec attaches no facts, has no fact reader: the state stays as constructed and the render is exactly what it
/// was before comments were preserved.
fn fold_document_facts<S>(
    document: &Document<'_>,
    resources: &mut ResourceContext<'_>,
    contract: &'static str,
    ingest: &mut dyn FnMut(jqf_data::DocumentFact<'_>, &mut S),
    state: &mut S,
) -> Result<(), CodecError> {
    if document.fact_owner_indexed() {
        for node_index in 0..document.node_count() {
            let Some(node) = NodeId::try_from_index(node_index) else {
                break;
            };
            for fact_id in document.owner_fact_ids(node) {
                // map_data keeps resource-class causes honest; a fact id from owner_fact_ids that missed is genuinely
                // impossible, which is the contract collapse it falls back to — under THIS walk's contract name, never
                // another site's.
                let fact = document
                    .fact(*fact_id)
                    .map_err(|error| jqf_codec_core::map_data(error, contract))?;
                ingest(fact, state);
            }
        }
        return Ok(());
    }
    let mut reader = match document.fact_reader(resources) {
        Ok(reader) => reader,
        Err(jqf_data::DataError::CapabilityUnavailable {
            capability: jqf_data::DocumentCapability::AttachedFacts,
        }) => return Ok(()),
        // Resource-class failures keep their cause (a ceiling refusal must read as the ceiling, never as an internal
        // bug); map_data routes them through their own classes and collapses only genuinely impossible local states to
        // this walk's contract violation.
        Err(error) => return Err(jqf_codec_core::map_data(error, contract)),
    };
    let limit = BatchLimit::new(usize::MAX).ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?;
    loop {
        let poll = reader
            .poll_batch(limit, resources)
            .map_err(|error| jqf_codec_core::map_data(error, contract))?;
        match poll {
            ReaderPoll::Batch(batch) => {
                for fact in batch.iter() {
                    ingest(fact, state);
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
    Ok(())
}

fn build_comment_index(
    document: &Document<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<CommentIndex, CodecError> {
    let mut index = CommentIndex::new();
    fold_document_facts(
        document,
        resources,
        "YAML comment index over a valid document",
        &mut |fact, index| ingest_comment_fact(index, fact),
        &mut index,
    )?;
    jqf_codec_core::comment::apply_encode_overlay(&mut index.leading, &mut index.inline, &mut index.foot, resources);
    Ok(index)
}

/// The decoded anchor name per anchored document node, indexed once per document from the `yaml.anchor@1` facts. One
/// name per node: the decoder attaches exactly one fact per authored `&name`, and the block encoder emits `&name` at
/// the node's first preorder occurrence and `*name` at every later one — the walk shares ONE document node across an
/// anchor and its aliases, so the shared node's second encounter is an alias site. A document without facts (owned
/// values, a codec that attaches none) has no anchor index and renders exactly as before.
type AnchorIndex = BTreeMap<NodeId, String>;

fn ingest_anchor_fact(index: &mut AnchorIndex, fact: jqf_data::DocumentFact<'_>) {
    let jqf_data::LocalOwnerRef::Node(node) = fact.owner() else {
        return;
    };
    if fact.role().as_str() != ANCHOR_FACT {
        return;
    }
    let FactPayloadView::Text(name) = fact.payload() else {
        return;
    };
    index.insert(node, String::from(name));
}

fn build_anchor_index(document: &Document<'_>, resources: &mut ResourceContext<'_>) -> Result<AnchorIndex, CodecError> {
    let mut index = AnchorIndex::new();
    fold_document_facts(
        document,
        resources,
        "YAML anchor index over a valid document",
        &mut |fact, index| ingest_anchor_fact(index, fact),
        &mut index,
    )?;
    Ok(index)
}

/// The scalars this dialect spells natively: none of them.
///
/// YAML 1.2's core schema has no timestamp and no byte string, so every projectable scalar projects here exactly as it
/// does on the way to JSON. The TAG is the thing YAML spells natively, and a tag never reaches the scalar layer.
const BLOCK_NATIVE: NativeSpellings = NativeSpellings::NONE;

/// Whether a scalar's own text is subject to core-schema resolution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Resolution {
    /// An untagged scalar: a reader resolves its text, so text that looks like a number, a boolean, a null or a
    /// timestamp needs quotes to stay a string.
    Resolved,
    /// A scalar directly under an explicit non-core tag: the tag settles the node's type, so only YAML's own syntax
    /// constrains the spelling.
    Tagged,
}

/// How one string is spelled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StringForm {
    /// Bare, no quotes: the text reads back as itself.
    Plain,
    /// A literal block scalar keeping its one trailing newline (`|`).
    LiteralClip,
    /// A literal block scalar with no trailing newline (`|-`).
    LiteralStrip,
    /// Double-quoted, where every byte is explicit.
    Quoted,
}

/// Renders one owned root as a block document, with no trailing newline.
///
/// An owned value has no document behind it and therefore no comments.
pub(crate) fn render_owned_document(
    bytes: &mut Vec<u8>,
    value: &Value,
    resources: &mut ResourceContext<'_>,
) -> Result<(), CodecError> {
    BlockWriter {
        bytes,
        comments: CommentIndex::new(),
        anchors: AnchorIndex::new(),
        anchors_seen: BTreeSet::new(),
        step: 2,
        flow: false,
    }
    .write_owned(value, 0, Resolution::Resolved, resources)
}

/// Renders one new BLOCK mapping member (`key: value`, or `key:` with the value's block content at `indent + step`),
/// with the cursor already at the member's line and no trailing LF. The edit lane's structural splice reuses the block
/// profile's own spellings, so a spliced member reads exactly like the profile's output: the key is plain when the core
/// schema round-trips it, a multiline string takes the literal form. `step` is the file's OWN indent step, so a spliced
/// block matches the file it lands in; the floor's step is 2 by default.
pub(crate) fn render_owned_member(
    bytes: &mut Vec<u8>,
    key: &str,
    value: &Value,
    indent: usize,
    step: usize,
    resources: &mut ResourceContext<'_>,
) -> Result<(), CodecError> {
    let mut writer = BlockWriter {
        bytes,
        comments: CommentIndex::new(),
        anchors: AnchorIndex::new(),
        anchors_seen: BTreeSet::new(),
        step,
        flow: false,
    };
    writer.push_indent(indent);
    writer.write_key(key);
    if owned_is_block(value) {
        writer.push(b":\n");
        writer.push_indent(indent + step);
    } else {
        writer.push(b": ");
    }
    writer.write_owned(value, indent + step, Resolution::Resolved, resources)
}

/// Renders one new BLOCK sequence item (`- ` at `dash_col`, the item's content at `dash_col + step`), no trailing LF.
/// The item rendering is the block profile's own (`write_owned` at the dash column). `step` is the file's own indent
/// step, exactly as for the mapping member above.
pub(crate) fn render_owned_item(
    bytes: &mut Vec<u8>,
    value: &Value,
    dash_col: usize,
    step: usize,
    resources: &mut ResourceContext<'_>,
) -> Result<(), CodecError> {
    let mut writer = BlockWriter {
        bytes,
        comments: CommentIndex::new(),
        anchors: AnchorIndex::new(),
        anchors_seen: BTreeSet::new(),
        step,
        flow: false,
    };
    writer.push_indent(dash_col);
    writer.push(b"- ");
    writer.write_owned(value, dash_col + step, Resolution::Resolved, resources)
}

/// Renders one owned value in FLOW syntax (`{k: v, ...}` / `[item, ...]`, nested containers flow), no trailing LF. A
/// nested collection inside a flow splice MUST be flow — block collections cannot be flow members. Scalars and keys use
/// the block profile's spelling (plain where the core schema round-trips, quoted otherwise); a block scalar stays legal
/// (the reference allows one as a flow member). Pushes one flow member's text (`key: <flow value>`), with no separator
/// — the flow splice composes the `, ` separators itself. Keys use the block profile's spelling; the value renders flow
/// (a nested container must be flow inside a flow collection).
pub(crate) fn push_flow_member(
    bytes: &mut Vec<u8>,
    key: &str,
    value: &Value,
    resources: &mut ResourceContext<'_>,
) -> Result<(), CodecError> {
    let mut writer = BlockWriter {
        bytes,
        comments: CommentIndex::new(),
        anchors: AnchorIndex::new(),
        anchors_seen: BTreeSet::new(),
        step: 2,
        flow: true,
    };
    writer.write_key(key);
    writer.push(b": ");
    writer.write_flow(value, resources)
}

/// Pushes one flow item's text (the value in flow syntax), no separator.
pub(crate) fn push_flow_item(
    bytes: &mut Vec<u8>,
    value: &Value,
    resources: &mut ResourceContext<'_>,
) -> Result<(), CodecError> {
    let mut writer = BlockWriter {
        bytes,
        comments: CommentIndex::new(),
        anchors: AnchorIndex::new(),
        anchors_seen: BTreeSet::new(),
        step: 2,
        flow: true,
    };
    writer.write_flow(value, resources)
}

/// Renders one located root as a block document, with no trailing newline.
pub(crate) fn render_located_document(
    bytes: &mut Vec<u8>,
    product: &DocumentProduct<'_>,
    view: &ValueView<'_, '_>,
    resources: &mut ResourceContext<'_>,
) -> Result<(), CodecError> {
    let comments = build_comment_index(product.document(), resources)?;
    let anchors = build_anchor_index(product.document(), resources)?;
    let mut writer = BlockWriter {
        bytes,
        comments,
        anchors,
        anchors_seen: BTreeSet::new(),
        step: 2,
        flow: false,
    };
    // A comment owned by the ROOT is a document-TRAILER comment (the decoder's ownership law: a leading comment always
    // finds the node it precedes, so only a comment with no following node lands on the root). It re-emits after the
    // document body, where it was written.
    let trailer = writer.comments.foot.remove(&view.node());
    writer.write_located(product, view, 0, ValuePrefix::Root, resources)?;
    if let Some(texts) = trailer {
        for text in texts {
            writer.push(b"\n");
            writer.push_comment_text(&text);
        }
    }
    Ok(())
}

/// The block renderer over one encoder's staging buffer.
struct BlockWriter<'buffer> {
    bytes: &'buffer mut Vec<u8>,
    /// Comments to re-emit, by owning node and position. Empty for owned values.
    comments: CommentIndex,
    /// The decoded `&name` per anchored document node. Empty for owned values, which never carry an anchor.
    anchors: AnchorIndex,
    /// Whether the walk is INSIDE a flow collection (the edit lane's flow splice): a scalar that would spell a flow
    /// delimiter as ordinary text must be quoted there.
    flow: bool,
    /// The anchored nodes already rendered at their ANCHOR position during this walk. The document shares ONE node
    /// across an anchor and every alias referencing it, so a later encounter of an anchored node is an ALIAS site and
    /// renders `*name` instead of the value.
    anchors_seen: BTreeSet<NodeId>,
    /// The indent step between nesting levels. The floor's shape is two-space (the block dialect's declared shape), so
    /// every construction defaults to 2; the edit lane's structural splice sets the FILE's own step, so a spliced block
    /// matches its neighbours without moving a single whole-document byte.
    step: usize,
}

/// What precedes a located value's position on its introducing line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ValuePrefix {
    /// The document root: nothing precedes the value.
    Root,
    /// A mapping member's `key:`.
    Key,
    /// A sequence item's `-`.
    Dash,
}

impl ValuePrefix {
    /// Whether a token already sits on the value's line.
    const fn preceded(self) -> bool {
        !matches!(self, Self::Root)
    }
}

impl BlockWriter<'_> {
    fn push(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    fn push_indent(&mut self, indent: usize) {
        for _ in 0..indent {
            self.push(b" ");
        }
    }

    /// Writes one comment line's `# text` (no indent, no line break).
    ///
    /// A comment's text is everything after the `#`, so a text carrying its own line break would open a second line
    /// that is NOT a comment. The decoder's text never spans a line — the scanner's span ends at the break — but the
    /// guard keeps a hand-built fact from producing invalid YAML.
    fn push_comment_text(&mut self, text: &str) {
        self.push(b"# ");
        let single_line = text.split(['\n', '\r']).next().unwrap_or("");
        self.push(single_line.as_bytes());
    }

    /// Emits the node's leading comments, each on its own line at `indent`, with the cursor already at that indent and
    /// the caller's own line still to come.
    fn write_leading_comments(&mut self, node: NodeId, indent: usize) {
        let Some(texts) = self.comments.leading.remove(&node) else {
            return;
        };
        for text in texts {
            self.push_comment_text(&text);
            self.push(b"\n");
            self.push_indent(indent);
        }
    }

    /// Emits the node's own-line inline comment after a single-line scalar render: ` # text`. The comment sits on the
    /// value's own line, so it is pushed with the cursor still at that line's end.
    fn write_inline_comment(&mut self, node: NodeId) {
        let Some(texts) = self.comments.inline.remove(&node) else {
            return;
        };
        for text in texts {
            self.push(b" # ");
            let single_line = text.split(['\n', '\r']).next().unwrap_or("");
            self.push(single_line.as_bytes());
        }
    }

    /// Emits the node's foot comments after its block, each on its own line at the block's content indent (the column
    /// the foot rule attributed them at). The ROOT's foot is the document trailer, removed by the caller before the
    /// render, so only a real block's foot lands here.
    fn write_foot_comments(&mut self, node: NodeId, indent: usize) {
        let Some(texts) = self.comments.foot.remove(&node) else {
            return;
        };
        for text in texts {
            self.push(b"\n");
            self.push_indent(indent);
            self.push_comment_text(&text);
        }
    }

    /// Writes one owned value, with the cursor already positioned where the value starts (after a `key: `, a `- `, or a
    /// tag) and no trailing LF.
    ///
    /// `indent` is the column this value's own CONTENT lives at: a map's keys, a sequence's dashes, a block scalar's
    /// lines. A one-line scalar ignores it.
    fn write_owned(
        &mut self,
        value: &Value,
        indent: usize,
        resolution: Resolution,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        let _depth = resources.enter_nesting_owned().map_err(CodecError::from)?;
        // A tag emits as ITSELF — YAML has tags, so there is nothing to project and nothing to warn about. A tagged
        // block collection starts on the next line, because a tag property and a block collection cannot share one.
        if let Value::Tagged { tag, payload } = value {
            if matches!(&**payload, Value::Tagged { .. }) {
                return Err(unrepresentable());
            }
            let spelling = crate::tag::emit_spelling(tag.as_str());
            self.push(spelling.as_bytes());
            if owned_is_block(payload) {
                self.push(b"\n");
                self.push_indent(indent);
            } else {
                self.push(b" ");
            }
            return self.write_owned(payload, indent, Resolution::Tagged, resources);
        }
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
            Value::Number(number) => {
                let text = number_text(number).ok_or_else(unrepresentable)?;
                self.push(text.as_bytes());
                Ok(())
            }
            Value::String(text) => {
                self.write_string(text.as_str(), indent, resolution);
                Ok(())
            }
            Value::Array(array) => {
                if array.is_empty() {
                    self.push(b"[]");
                    return Ok(());
                }
                for (index, item) in array.into_iter().enumerate() {
                    if index > 0 {
                        self.push(b"\n");
                        self.push_indent(indent);
                    }
                    self.push(b"- ");
                    self.write_owned(item, indent + self.step, Resolution::Resolved, resources)?;
                }
                Ok(())
            }
            Value::Object(object) => {
                if object.is_empty() {
                    self.push(b"{}");
                    return Ok(());
                }
                for (index, entry) in object.into_iter().enumerate() {
                    if index > 0 {
                        self.push(b"\n");
                        self.push_indent(indent);
                    }
                    self.write_key(entry.key());
                    let child = entry.value();
                    if owned_is_block(child) {
                        self.push(b":\n");
                        self.push_indent(indent + self.step);
                    } else {
                        self.push(b": ");
                    }
                    self.write_owned(child, indent + self.step, Resolution::Resolved, resources)?;
                }
                Ok(())
            }
            Value::Bytes(_)
            | Value::LocalDate(_)
            | Value::LocalTime(_)
            | Value::LocalDateTime(_)
            | Value::OffsetDateTime(_) => {
                let scalar = ScalarView::from_value(value).ok_or_else(unrepresentable)?;
                self.write_projected(&scalar, indent, resolution, resources)
            }
            Value::Tagged { .. } => unreachable!("tag layer emitted above"),
        }
    }

    /// Writes one owned value in FLOW syntax: a tag as its spelling plus the payload, a nested collection as `{k: v,
    /// ...}` / `[item, ...]`, and a scalar through the block profile's spelling rules. The edit lane's flow splice uses
    /// this because a block collection cannot be a flow member — a nested container inside a flow splice must render
    /// flow.
    fn write_flow(&mut self, value: &Value, resources: &mut ResourceContext<'_>) -> Result<(), CodecError> {
        let _depth = resources.enter_nesting_owned().map_err(CodecError::from)?;
        if let Value::Tagged { tag, payload } = value {
            if matches!(&**payload, Value::Tagged { .. }) {
                return Err(unrepresentable());
            }
            let spelling = crate::tag::emit_spelling(tag.as_str());
            self.push(spelling.as_bytes());
            self.push(b" ");
            return self.write_flow(payload, resources);
        }
        match value {
            Value::Object(object) => {
                self.push(b"{");
                for (index, entry) in object.iter().enumerate() {
                    if index > 0 {
                        self.push(b", ");
                    }
                    self.write_key(entry.key());
                    self.push(b": ");
                    self.write_flow(entry.value(), resources)?;
                }
                self.push(b"}");
                Ok(())
            }
            Value::Array(array) => {
                self.push(b"[");
                for (index, item) in array.iter().enumerate() {
                    if index > 0 {
                        self.push(b", ");
                    }
                    self.write_flow(item, resources)?;
                }
                self.push(b"]");
                Ok(())
            }
            _ => self.write_owned(value, 0, Resolution::Resolved, resources),
        }
    }

    /// Renders one located value under [`Self::write_owned`]'s contract, preceded by the node's PROPERTIES: the decoded
    /// `&name`/`*name` anchor and the tag spelling. The caller has pushed the introducing token (`key:` or `-`) with NO
    /// trailing separator, or nothing at the root (`prefix` says which), and the value's own content belongs at
    /// `indent` — the block body's column for a collection, ignored by a one-line scalar.
    ///
    /// The anchor/alias law: a node carrying a decoded `&name` renders `&name` at its FIRST preorder occurrence — its
    /// authored anchor position — and `*name` at every later one, because the document walk shares ONE node across an
    /// anchor and every alias referencing it. An alias site renders NOTHING else: the referenced value is emitted
    /// exactly once, where its anchor is, and the alias names it. A node without an authored anchor — a merge-expanded
    /// member, an unanchored shared node — renders its value at every position, exactly as before.
    ///
    /// The block-vs-inline line decision: a block collection cannot share its introducing line, so after a KEY or after
    /// any PROPERTY it moves to the next line at `indent`; under a bare DASH it stays compact (`- - 1`) and at the bare
    /// ROOT it starts at column 0. A one-line value is separated from the introducing token or the last property by a
    /// space.
    #[allow(
        clippy::only_used_in_recursion,
        reason = "the product is the document the recursive child walk reads"
    )]
    fn write_located(
        &mut self,
        product: &DocumentProduct<'_>,
        view: &ValueView<'_, '_>,
        indent: usize,
        prefix: ValuePrefix,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        let _depth = resources.enter_nesting_owned().map_err(CodecError::from)?;
        // The property pass: each property token (anchor, then tag) is preceded by a space when a token already sits on
        // the line.
        let mut preceded = prefix.preceded();
        let mut property = false;
        if let Some(name) = self.anchors.get(&view.node()).cloned() {
            if !self.anchors_seen.insert(view.node()) {
                // An alias site: `*name` closes the value; the shared node's content was already emitted at its anchor
                // position.
                if preceded {
                    self.push(b" ");
                }
                self.push(b"*");
                self.push(name.as_bytes());
                return Ok(());
            }
            if preceded {
                self.push(b" ");
            }
            self.push(b"&");
            self.push(name.as_bytes());
            preceded = true;
            property = true;
        }
        let non_core = view
            .tag_semantics()
            .map_err(map_data)?
            .is_some_and(|semantics| semantics == IntrinsicTagSemantics::Tagged);
        if non_core {
            // A located tag is a fact on the node; the payload is the same node without its tag, which only the owned
            // form expresses. Materialize, peel one layer, then write under Tagged resolution.
            let tag = view.tag().map_err(map_data)?.ok_or_else(unrepresentable)?;
            let spelling = crate::tag::emit_spelling(tag.as_str());
            if preceded {
                self.push(b" ");
            }
            self.push(spelling.as_bytes());
            let handle = product.document().node_handle(view.node()).map_err(map_data)?;
            let payload = product
                .document()
                .materialize_node(handle, resources)
                .map_err(map_data)?;
            let payload = payload.untagged();
            if owned_is_block(payload) {
                self.push(b"\n");
                self.push_indent(indent);
            } else {
                self.push(b" ");
            }
            return self.write_owned(payload, indent, Resolution::Tagged, resources);
        }
        // The value's own line (see the doc comment for the matrix).
        if located_is_block(view)? {
            if property || prefix == ValuePrefix::Key {
                // The collection VALUE's own inline comment trails its introducing line (`key: # note`) before the body
                // starts, exactly where the decoder attributed it.
                self.write_inline_comment(view.node());
                self.push(b"\n");
                self.push_indent(indent);
            } else if prefix == ValuePrefix::Dash {
                // RECORDED NARROWING: under a bare dash the content shares the item line, so an inline comment owned by
                // this collection has no same-line slot of its own and is dropped here (the foot still re-emits).
                self.push(b" ");
            }
        } else if preceded {
            self.push(b" ");
        }
        match view.kind().map_err(map_data)? {
            ValueKind::Null => {
                self.push(b"null");
                self.write_inline_comment(view.node());
                Ok(())
            }
            ValueKind::Bool => {
                let Some(ScalarView::Bool(boolean)) = view.scalar().map_err(map_data)? else {
                    return Err(unrepresentable());
                };
                self.push(if boolean { b"true" } else { b"false" });
                self.write_inline_comment(view.node());
                Ok(())
            }
            ValueKind::Number => {
                let Some(ScalarView::Number(number)) = view.scalar().map_err(map_data)? else {
                    return Err(unrepresentable());
                };
                let text = number_view_text(&number).ok_or_else(unrepresentable)?;
                self.push(text.as_bytes());
                self.write_inline_comment(view.node());
                Ok(())
            }
            ValueKind::String => {
                let Some(ScalarView::String(text)) = view.scalar().map_err(map_data)? else {
                    return Err(unrepresentable());
                };
                self.write_string(text, indent, Resolution::Resolved);
                // An inline comment has a same-line position only after a single-line render; a multi-line string's
                // block scalar has none (recorded narrowing: the inline on a block-scalar value is dropped on re-emit).
                if !text.contains('\n') {
                    self.write_inline_comment(view.node());
                }
                Ok(())
            }
            ValueKind::Array => {
                let array = view.array().map_err(map_data)?.ok_or_else(unrepresentable)?;
                if array.is_empty() {
                    self.push(b"[]");
                    // An empty collection still owns its same-line comment and its foot: the early return must not drop
                    // them.
                    self.write_inline_comment(view.node());
                    self.write_foot_comments(view.node(), indent);
                    return Ok(());
                }
                for index in 0..array.len() {
                    if index > 0 {
                        self.push(b"\n");
                        self.push_indent(indent);
                    }
                    let item = array.get(index).ok_or_else(unrepresentable)?;
                    self.write_leading_comments(item.node(), indent);
                    self.push(b"-");
                    self.write_located(product, &item, indent + self.step, ValuePrefix::Dash, resources)?;
                }
                // The block's foot sits at the member column (the dashes'), the column the foot rule attributed it at.
                self.write_foot_comments(view.node(), indent);
                Ok(())
            }
            ValueKind::Object => {
                let object = view.object().map_err(map_data)?.ok_or_else(unrepresentable)?;
                if object.is_empty() {
                    self.push(b"{}");
                    // An empty collection still owns its same-line comment and its foot: the early return must not drop
                    // them.
                    self.write_inline_comment(view.node());
                    self.write_foot_comments(view.node(), indent);
                    return Ok(());
                }
                for index in 0..object.len() {
                    if index > 0 {
                        self.push(b"\n");
                        self.push_indent(indent);
                    }
                    let entry = object.get_index(index).map_err(map_data)?.ok_or_else(unrepresentable)?;
                    let child = entry.value();
                    // The decoder redirects a key's comment onto its VALUE node, so the member's comments are read from
                    // the child.
                    self.write_leading_comments(child.node(), indent);
                    self.write_key(entry.key());
                    self.push(b":");
                    self.write_located(product, &child, indent + self.step, ValuePrefix::Key, resources)?;
                }
                // The block's foot sits at the member column (the keys'), the column the foot rule attributed it at.
                self.write_foot_comments(view.node(), indent);
                Ok(())
            }
            ValueKind::Bytes
            | ValueKind::LocalDate
            | ValueKind::LocalTime
            | ValueKind::LocalDateTime
            | ValueKind::OffsetDateTime => {
                let scalar = view.scalar().map_err(map_data)?.ok_or_else(unrepresentable)?;
                self.write_projected(&scalar, indent, Resolution::Resolved, resources)
            }
        }
    }

    /// A mapping key, which is a string in this value model and is spelled by the same rule as any other resolved
    /// string. A key that would want a block scalar is quoted instead: an explicit `? ` key is a machine's spelling,
    /// and this dialect is not writing for one.
    fn write_key(&mut self, key: &str) {
        if string_form(key, Resolution::Resolved, self.flow) == StringForm::Plain {
            self.push(key.as_bytes());
            return;
        }
        self.write_quoted(key);
    }

    fn write_string(&mut self, text: &str, indent: usize, resolution: Resolution) {
        // A root scalar sits at column 0, where `---`/`...` are STREAM markers, not content: `--- x` re-decodes as `x`,
        // a bare `---` as `null`, a bare `...` as a document end. Nested scalars are indented past column 0, so only
        // the root is at risk — quote it here rather than in `plain_admits`, which does not know its column.
        if indent == 0 && is_document_marker(text) {
            self.write_quoted(text);
            return;
        }
        match string_form(text, resolution, self.flow) {
            StringForm::Plain => self.push(text.as_bytes()),
            StringForm::Quoted => self.write_quoted(text),
            StringForm::LiteralClip => self.write_literal(text, b"|", indent),
            StringForm::LiteralStrip => self.write_literal(text, b"|-", indent),
        }
    }

    /// A literal block scalar: the header, then every line at `indent`.
    ///
    /// The content's own newlines ARE the line breaks, so the body is written line by line and the trailing newline the
    /// clip form keeps is the block's own final break rather than a byte of the last line.
    fn write_literal(&mut self, text: &str, header: &[u8], indent: usize) {
        self.push(header);
        let body = text.strip_suffix('\n').unwrap_or(text);
        for line in body.split('\n') {
            self.push(b"\n");
            // A blank line carries no indentation: trailing whitespace is exactly what the literal form cannot
            // preserve.
            if !line.is_empty() {
                self.push_indent(indent);
                self.push(line.as_bytes());
            }
        }
    }

    /// A double-quoted scalar, through the codec's own §4.8 escape set so both dialects escape one way.
    fn write_quoted(&mut self, text: &str) {
        self.push(b"\"");
        write_escaped(self.bytes, text);
        self.push(b"\"");
    }

    /// A projected scalar (a temporal's RFC 3339 text, a byte string's base64url) written through the ONE shared layer,
    /// then spelled by the same quoting rule as any other string.
    ///
    /// The text lands in accounted scratch first because the quoting decision has to READ it: `2026-06-15` must be
    /// quoted and `AQIDBAU` must not, and only the bytes say which.
    fn write_projected(
        &mut self,
        scalar: &ScalarView<'_>,
        indent: usize,
        resolution: Resolution,
        resources: &ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        let projection = classify_scalar(scalar, BLOCK_NATIVE, resources).ok_or_else(unrepresentable)?;
        let mut scratch = String::new();
        projection.write(&mut ScratchSink(&mut scratch), resources)?;
        self.write_string(scratch.as_str(), indent, resolution);
        Ok(())
    }
}

/// A projection sink over accounted scratch text.
struct ScratchSink<'scratch>(&'scratch mut String);

impl ProjectionSink for ScratchSink<'_> {
    fn push_projected(&mut self, text: &str, _resources: &ResourceContext<'_>) -> Result<(), CodecError> {
        self.0.push_str(text);
        Ok(())
    }
}

/// Whether an owned value starts a BLOCK — a non-empty collection, which cannot share a line with the key or tag that
/// introduces it.
fn owned_is_block(value: &Value) -> bool {
    match value {
        Value::Array(array) => !array.is_empty(),
        Value::Object(object) => !object.is_empty(),
        // A tag always takes the introducing line itself; whether ITS payload is a block is settled after the tag is
        // written.
        _ => false,
    }
}

/// [`owned_is_block`] over a located node.
fn located_is_block(view: &ValueView<'_, '_>) -> Result<bool, CodecError> {
    if view.tag_semantics().map_err(map_data)? == Some(IntrinsicTagSemantics::Tagged) {
        return Ok(false);
    }
    Ok(match view.kind().map_err(map_data)? {
        ValueKind::Array => !view.array().map_err(map_data)?.ok_or_else(unrepresentable)?.is_empty(),
        ValueKind::Object => !view.object().map_err(map_data)?.ok_or_else(unrepresentable)?.is_empty(),
        _ => false,
    })
}

/// The spelling one string gets.
fn string_form(text: &str, resolution: Resolution, flow: bool) -> StringForm {
    if text.contains('\n') {
        return literal_form(text).unwrap_or(StringForm::Quoted);
    }
    if plain_admits_in(text, resolution, flow) {
        StringForm::Plain
    } else {
        StringForm::Quoted
    }
}

/// The literal block form a multiline string qualifies for, if any.
fn literal_form(text: &str) -> Option<StringForm> {
    let (body, form) = match text.strip_suffix('\n') {
        Some(body) => (body, StringForm::LiteralClip),
        None => (text, StringForm::LiteralStrip),
    };
    if body.is_empty() || body.ends_with('\n') {
        // No content, or a second trailing newline: neither is one of this dialect's two chomping states.
        return None;
    }
    for line in body.split('\n') {
        if line.starts_with([' ', '\t']) || line.ends_with([' ', '\t']) {
            return None;
        }
        if line.chars().any(char::is_control) {
            return None;
        }
    }
    Some(form)
}

/// Whether a single-line string reads back as ITSELF without quotes. Whether a single-line scalar spells a STREAM
/// marker at column 0: `---`/ `...`, alone or followed by a space. A tab can never reach this path — it is a control
/// character and already quoted — so space separation is the whole marker surface.
fn is_document_marker(text: &str) -> bool {
    text == "---" || text == "..." || text.starts_with("--- ") || text.starts_with("... ")
}

/// [`plain_admits`] under the caller's collection context: inside a FLOW collection the flow delimiters themselves end
/// a scalar mid-text (`,a,b]` re-decodes as two items), so they are quoted there even though they are ordinary text in
/// block context.
fn plain_admits_in(text: &str, resolution: Resolution, flow: bool) -> bool {
    if flow && text.contains([',', '[', ']', '{', '}']) {
        return false;
    }
    plain_admits(text, resolution)
}

/// Whether a single-line string reads back as ITSELF without quotes. `pub(crate)` so the leaf splice (`encode.rs`'s
/// `render_leaf`) applies the same round-trip law to a plain-style edited value.
pub(crate) fn plain_admits(text: &str, resolution: Resolution) -> bool {
    if text.is_empty() {
        return false;
    }
    if text.starts_with([' ', '\t']) || text.ends_with([' ', '\t']) {
        return false;
    }
    if text.chars().any(char::is_control) {
        return false;
    }
    // A leading BOM is skipped by the scanner at column 0, so a plain scalar that opens with one would re-decode
    // without it: quote it.
    if text.starts_with('\u{FEFF}') {
        return false;
    }
    // A mapping key whose exact text is `<<` is the merge indicator. Emitting it plain makes this codec's own decoder
    // reject the document.
    if text == "<<" {
        return false;
    }
    // A leading indicator changes what the line MEANS, so no plain scalar opens with one.
    if text.starts_with([
        ',', '[', ']', '{', '}', '#', '&', '*', '!', '|', '>', '\'', '"', '%', '@', '`',
    ]) {
        return false;
    }
    // `-`, `?` and `:` indicate only when a space (or the end of the scalar) follows, which is why `-ann` needs no
    // quotes and `- ann` does.
    if matches!(text.as_bytes()[0], b'-' | b'?' | b':') && matches!(text.as_bytes().get(1), None | Some(b' ')) {
        return false;
    }
    // A `: ` ends the key, a ` #` starts a comment, and a trailing `:` turns the scalar into a key. Each would silently
    // truncate the value.
    if text.contains(": ") || text.contains(" #") || text.ends_with(':') {
        return false;
    }
    resolution == Resolution::Tagged || !resolves_as_non_string(text)
}

/// Whether a text can be spelled as a SINGLE-QUOTED YAML scalar and re-decode to the same text: no control character —
/// the quoted-scalar grammar cannot carry one, and a line break is a control character by this codec's law. A multiline
/// value therefore stays on the floor's literal/double-quoted path. The only escape is `''` for a literal quote,
/// applied by the caller.
#[must_use]
pub(crate) fn single_quoted_admits(text: &str) -> bool {
    !text.chars().any(char::is_control)
}

/// Whether a reader would resolve this text as something other than a string.
///
/// The core schema's own set, plus the YAML 1.1 boolean words and the timestamp shape. jqf's own reader resolves
/// neither of those last two, but this dialect writes for every reader, and a `no` that comes back as `false` is the
/// exact data loss the quoting rule exists to prevent.
fn resolves_as_non_string(text: &str) -> bool {
    matches!(
        text,
        "~" | "null"
            | "Null"
            | "NULL"
            | "true"
            | "True"
            | "TRUE"
            | "false"
            | "False"
            | "FALSE"
            | "y"
            | "Y"
            | "yes"
            | "Yes"
            | "YES"
            | "n"
            | "N"
            | "no"
            | "No"
            | "NO"
            | "on"
            | "On"
            | "ON"
            | "off"
            | "Off"
            | "OFF"
    ) || is_core_number(text)
        || is_timestamp_shaped(text)
}

/// Whether this codec's own core-schema reader would resolve the text as a number. The encoder's quoting rule must
/// never emit a string the DECODER reads back as a number — the round-trip law — so the check delegates to the schema's
/// int and float productions instead of re-deriving them: the two grammars are one law, and a duplicated twin can only
/// drift apart.
fn is_core_number(text: &str) -> bool {
    crate::schema::parse_yaml_int(text).is_some() || crate::schema::parse_yaml_float(text).is_ok()
}

/// Whether the text OPENS with a date or a clock time.
///
/// A prefix test, not a full parse: `2026-06-15T12:34:56Z`, `2026-06-15` and `12:34:56` all resolve as timestamps in
/// readers that carry the type, and any text that starts that way is worth a pair of quotes.
fn is_timestamp_shaped(text: &str) -> bool {
    let bytes = text.as_bytes();
    let digits = |from: usize, count: usize| {
        bytes.len() >= from + count && bytes[from..from + count].iter().all(u8::is_ascii_digit)
    };
    if digits(0, 4) && bytes.get(4) == Some(&b'-') && digits(5, 2) && bytes.get(7) == Some(&b'-') {
        return true;
    }
    digits(0, 2) && bytes.get(2) == Some(&b':') && digits(3, 2) && bytes.get(5) == Some(&b':')
}

#[cfg(test)]
mod tests {
    use super::*;
    use jqf_data::{ObjectBuilder, ObjectKey};
    use jqf_resource::{ContinueControl, RequestAccount, ResourceLimits, WorkMeter};

    fn resources<'a>() -> ResourceContext<'a> {
        ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                .expect("account"),
            &ContinueControl,
            WorkMeter::try_new_v1(4096).expect("work"),
        )
        .expect("context")
    }

    fn string(text: &str) -> Value {
        let _resources = resources();
        Value::try_string(text).expect("string")
    }

    /// Renders one owned root as block YAML, no trailing newline.
    fn render(value: &Value) -> String {
        let mut bytes = Vec::new();
        let mut resources = resources();
        render_owned_document(&mut bytes, value, &mut resources).expect("render");
        String::from_utf8(bytes.as_slice().to_vec()).expect("utf8")
    }

    #[test]
    fn root_document_markers_are_quoted() {
        assert_eq!(render(&string("--- x")), "\"--- x\"");
        assert_eq!(render(&string("...")), "\"...\"");
        assert_eq!(render(&string("---")), "\"---\"");
        assert_eq!(render(&string("... x")), "\"... x\"");
        // A normal plain root is untouched.
        assert_eq!(render(&string("hello")), "hello");
    }

    #[test]
    fn nested_marker_lookalike_stays_plain() {
        // A value under a key sits past column 0, where `---` is content.
        let mut builder = ObjectBuilder::new();
        builder
            .try_insert_last(ObjectKey::try_from_str("a").expect("key"), string("--- x"))
            .expect("insert");
        let object = builder.try_finish().expect("finish");
        assert_eq!(render(&Value::Object(object)), "a: --- x");
    }

    #[test]
    fn leading_bom_is_quoted_not_plain() {
        // The scanner skips a BOM at column 0, so a plain scalar opening with one would re-decode without it — quoting
        // is the only round-trip form, at the root and in a key alike.
        assert_eq!(render(&string("\u{FEFF}hello")), "\"\u{FEFF}hello\"");
        assert_eq!(render(&string("\u{FEFF}")), "\"\u{FEFF}\"");
        let mut builder = ObjectBuilder::new();
        builder
            .try_insert_last(ObjectKey::try_from_str("\u{FEFF}key").expect("key"), string("v"))
            .expect("insert");
        let object = builder.try_finish().expect("finish");
        assert_eq!(render(&Value::Object(object)), "\"\u{FEFF}key\": v");
    }

    #[test]
    fn merge_indicator_key_is_quoted() {
        let mut builder = ObjectBuilder::new();
        builder
            .try_insert_last(ObjectKey::try_from_str("<<").expect("key"), string("x"))
            .expect("insert");
        let object = builder.try_finish().expect("finish");
        assert_eq!(render(&Value::Object(object)), "\"<<\": x");
    }
}
