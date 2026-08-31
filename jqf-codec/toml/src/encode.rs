//! Deterministic semantic TOML encode (`toml.jqf-1.0@1` / `toml.jqf-1.1@1`).
//!
//! UTF-8, LF, one final LF, no indent. Bare key when the grammar admits it, else a basic string. Empty root is zero
//! bytes. Topology-normalizing: `a.b = 1` becomes `[a]` plus `b = 1`. Splice rulings live on
//! [`EncoderFactoryImpl::render_edit_append`].

use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::str::from_utf8;

use crate::parse::data_contract;
use crate::provider::DialectKind;

use jqf_codec_core::byte_scan::{PlainString, prefix_len};
use jqf_codec_core::{
    ByteSink, CodecError, CodecFailureKind, CodecRunContext, EditAppendMembers, EditInsertion, EditRemoval,
    EditRemoveMembers, EditRenameMembers, EditReplacement, EncodeItem, EncodeRequest, EncoderFactoryImpl,
    EncoderSession, ErasedEncoderFactory, ErasedEncoderSession, NativeSpellings, PreservationOutcome,
    PreservationReport, PreservationRequest, ProjectableScalar, RecycledSessionState, TagLayer, TrackedProjectionSink,
    classify_scalar, line_statement_cut, project_tag, view_tag_layer,
};
use jqf_data::{DataError, Document, DocumentId, NodeId, NumberView, ScalarView, Value, ValueKind, ValueView};
use jqf_resource::{ResourceContext, WorkAdmission};

const OFFER_BYTES: usize = 16 * 1024;

/// Decimal digit table for the fixed-width offset spelling.
const DIGITS: [u8; 10] = *b"0123456789";

/// The authored string style at a leaf patch site, classified from the retained source bytes the SDK passed. A leading
/// `'` names a LITERAL string; everything else (basic `"..."`, a non-string value, no authored span) renders through
/// the basic-string path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AuthoredStyle {
    Literal,
    Basic,
}

fn authored_style(authored: Option<&[u8]>) -> AuthoredStyle {
    match authored {
        Some(bytes) if bytes.iter().find(|byte| !matches!(byte, b' ' | b'\t')) == Some(&b'\'') => {
            AuthoredStyle::Literal
        }
        _ => AuthoredStyle::Basic,
    }
}

/// Whether a text can be spelled as a TOML LITERAL string and re-decode to the same text: no `'` (TOML has no literal
/// escape) and no forbidden control character (the basic string's escape pass is the only carrier for one). The
/// forbidden set is the literal grammar's: U+0000-0008, U+000A-001F, U+007F, U+2028, U+2029 — a TAB is legal inside a
/// literal string.
fn literal_admits(text: &str) -> bool {
    !text.contains('\'')
        && !text.chars().any(|ch| {
            let code = u32::from(ch);
            matches!(code, 0x00..=0x08 | 0x0A..=0x1F | 0x7F | 0x2028 | 0x2029)
        })
}

/// TOML has first-class date, time, and date-time LITERALS, so all four temporals keep their native spelling and are
/// never projected — the policy's "refusal only where no canonical spelling exists" clause reads the other way here.
/// A byte string has no TOML literal and is projected as base64url text.
///
/// `null` is deliberately absent from both lists: TOML has no null and no canonical text for one, so it remains a
/// refusal rather than becoming a silent `""`.
const TOML_NATIVE: NativeSpellings = NativeSpellings::NONE
    .with(ProjectableScalar::LocalDate)
    .with(ProjectableScalar::LocalTime)
    .with(ProjectableScalar::LocalDateTime)
    .with(ProjectableScalar::OffsetDateTime);

/// The stable identity of the deterministic TOML encoder factory.
pub(crate) fn create_factory(
    request: EncodeRequest<'_, '_>,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedEncoderFactory, CodecError> {
    // Validate the target: exactly one of the two output profiles.
    request.expect_target(
        crate::FORMAT_ID,
        &[crate::TOML_JQF_1_0_DIALECT_ID, crate::TOML_JQF_1_1_DIALECT_ID],
    )?;
    // The two output profiles share one deterministic byte law, so one factory serves both — but the leap-second law
    // differs (TOML 1.1 removed second 60), so the factory carries which profile it serves.
    let profile = output_profile(request.dialect.as_str());
    ErasedEncoderFactory::try_new_factory(request.preservation, resources, move || {
        Ok(TomlEncoderFactory { profile })
    })
}

struct TomlEncoderFactory {
    profile: DialectKind,
}

impl EncoderFactoryImpl for TomlEncoderFactory {
    fn physical_encoder(&self) -> jqf_codec_core::PhysicalRouteId {
        crate::ENCODE_PHYSICAL_ROUTE_ID
    }

    fn start<'item, 'source>(
        &self,
        item: EncodeItem<'item, 'source>,
        preservation: PreservationRequest,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedEncoderSession<'item, 'source>, CodecError> {
        ErasedEncoderSession::try_new(item, preservation, || Ok(TomlEncoder::new(self.profile)))
    }

    fn try_restart(
        &self,
        state: &mut RecycledSessionState<'_>,
        _item: EncodeItem<'_, '_>,
        _preservation: PreservationRequest,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        let Some(encoder) = state.downcast_mut::<TomlEncoder>() else {
            return Ok(false);
        };
        encoder.reset();
        Ok(true)
    }

    fn render_leaf(
        &self,
        _document: &Document<'_>,
        _node: NodeId,
        _path: &[String],
        _source: &[u8],
        value: &Value,
        authored: Option<&[u8]>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Vec<u8>, CodecError> {
        // The bare-value grammar: a leaf patch replaces a value's authored span with these exact bytes, so they must be
        // the value as it appears AFTER a `key = ` — no key, no newline. This is the same `encode_owned_value` the
        // document encoder pushes after a key, so a patched file re-decodes to the same document. A string whose site
        // is a LITERAL string keeps the literal pair when the new text is literal-safe (ruling 6); every other value
        // renders through the ordinary path.
        let mut encoder = TomlEncoder::new(self.profile);
        if let Value::String(text) = value
            && authored_style(authored) == AuthoredStyle::Literal
            && literal_admits(text)
        {
            encoder.push(b"'");
            encoder.push(text.as_bytes());
            encoder.push(b"'");
        } else {
            encoder.encode_owned_value(value, resources)?;
        }
        Ok(encoder.bytes.as_slice().to_vec())
    }

    fn render_edit_append(
        &self,
        document: &Document<'_>,
        container: NodeId,
        path: &[String],
        source: &[u8],
        members: EditAppendMembers<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<alloc::vec::Vec<EditInsertion>, CodecError> {
        let node = document.node_handle(container).map_err(map_data)?;
        let view = document.value_view(node).map_err(map_data)?;
        match members {
            EditAppendMembers::Table(members) => {
                let span = document.node_source_span(container).map_err(map_data)?;
                if let Some(span) = span
                    && source.get(span.start() as usize) == Some(&b'{')
                {
                    // An INLINE table: the new members render inside the closing `}` — inline-table edits stay
                    // inline.
                    render_inline_append(view, span, source, members, self.profile, resources)
                } else {
                    render_table_append(
                        document,
                        view,
                        container,
                        path,
                        source,
                        members,
                        self.profile,
                        resources,
                    )
                }
            }
            EditAppendMembers::Array(items) => {
                render_array_append(document, view, path, source, items, self.profile, resources)
            }
        }
    }

    fn render_edit_remove(
        &self,
        document: &Document<'_>,
        container: NodeId,
        _path: &[String],
        source: &[u8],
        members: EditRemoveMembers<'_>,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<alloc::vec::Vec<EditRemoval>, CodecError> {
        // A key/value statement owns its whole line, so a LEAF removal is the line cut, and a whole `[table]` section
        // removes as its header line (with its comment block) through the subtree's last line. An INLINE table's
        // members are comma-separated inside one `{}`: removing one is punctuation surgery this policy does not name,
        // so it declines to the whole-document floor, exactly as it always has.
        if let Some(span) = document.node_source_span(container).map_err(map_data)?
            && source.get(span.start() as usize) == Some(&b'{')
        {
            return Ok(alloc::vec::Vec::new());
        }
        match members {
            EditRemoveMembers::Table(members) => {
                let mut removals = alloc::vec::Vec::new();
                for (_, node) in members {
                    let node_id = *node;
                    let view = document
                        .value_view(document.node_handle(node_id).map_err(map_data)?)
                        .map_err(map_data)?;
                    let removal = if is_child_section(document, view, source)? {
                        // A whole [table]: its header is a line the value span does not reach, so the cut runs from the
                        // header's comment block through the subtree's last line. An array-of-tables member has no
                        // header span and declines.
                        table_section_cut(document, view, source)?
                    } else {
                        let Some(span) = document.node_source_span(node_id).map_err(map_data)? else {
                            return Ok(alloc::vec::Vec::new());
                        };
                        line_statement_cut(source, span)
                    };
                    let Some(removal) = removal else {
                        return Ok(alloc::vec::Vec::new());
                    };
                    removals.push(removal);
                }
                Ok(removals)
            }
            EditRemoveMembers::Array(items) => render_array_remove(document, container, source, items),
        }
    }

    fn render_edit_rename(
        &self,
        document: &Document<'_>,
        container: NodeId,
        _path: &[String],
        source: &[u8],
        members: EditRenameMembers<'_>,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<alloc::vec::Vec<EditReplacement>, CodecError> {
        // An INLINE table's members are comma-separated inside one `{}`: naming a member's key token needs the comma
        // walk this policy does not implement (the line-start walk would swallow the whole statement), so it declines
        // to the whole-document floor, exactly as the remove seam does. A statement's key token is otherwise found from
        // its value's authored span: `key = value`, one statement per line.
        if let Some(span) = document.node_source_span(container).map_err(map_data)?
            && source.get(span.start() as usize) == Some(&b'{')
        {
            return Ok(alloc::vec::Vec::new());
        }
        let node = document.node_handle(container).map_err(map_data)?;
        let view = document.value_view(node).map_err(map_data)?;
        let Some(object) = view.object().map_err(map_data)? else {
            return Ok(alloc::vec::Vec::new());
        };
        let EditRenameMembers(pairs) = members;
        let mut replacements = alloc::vec::Vec::new();
        for (old, new) in pairs {
            // The member's VALUE node anchors the statement: walk back from its authored span to the key token. A value
            // without a retained span (a span-less build), or a statement whose walk cannot name the key token,
            // declines the whole rename.
            let Some(value) = object.get(old) else {
                return Ok(alloc::vec::Vec::new());
            };
            let Some(span) = document.node_source_span(value.node()).map_err(map_data)? else {
                return Ok(alloc::vec::Vec::new());
            };
            let Some((start, end)) = toml_key_span(source, span.start() as usize) else {
                return Ok(alloc::vec::Vec::new());
            };
            let Some(bytes) = toml_rename_bytes(&source[start..end], old, new) else {
                return Ok(alloc::vec::Vec::new());
            };
            replacements.push(EditReplacement {
                at: start,
                region_len: end - start,
                bytes,
            });
        }
        Ok(replacements)
    }
}

/// The authored key token's byte region of the statement whose VALUE starts at `value_start`: from the line's first
/// significant byte to the byte before the `=` separator, skipping the value's own closing quote (the string span
/// convention is INNER content) and the separator's spacing. Returns `None` when the walk cannot name the key token —
/// a dotted-key member's value sits on a line whose key region covers the whole dotted path, which the caller's
/// region-vs-old-key check then declines.
fn toml_key_span(source: &[u8], value_start: usize) -> Option<(usize, usize)> {
    let mut at = value_start;
    while at > 0 && matches!(source[at - 1], b' ' | b'\t' | b'"' | b'\'') {
        at -= 1;
    }
    if at == 0 || source[at - 1] != b'=' {
        return None;
    }
    at -= 1;
    while at > 0 && matches!(source[at - 1], b' ' | b'\t') {
        at -= 1;
    }
    let key_end = at;
    while at > 0 && source[at - 1] != b'\n' {
        at -= 1;
    }
    let mut start = at;
    while start < key_end && matches!(source[start], b' ' | b'\t') {
        start += 1;
    }
    (start < key_end).then_some((start, key_end))
}

/// The replacement bytes for one renamed TOML key: the new key rendered in the OLD key token's own spelling when it
/// still fits (bare stays bare, basic-quoted stays basic-quoted, literal-quoted stays literal-quoted), and at ANY byte
/// length — the same-length half needs no move and the caller applies it as an in-place overwrite; a DIFFERENT-length
/// half splices the region at the new length and shifts the following bytes, keeping the entry (and its comments, which
/// follow the key) in place. A bare key whose new text the bare grammar refuses renders basic-quoted — the encoder's
/// own law (a key the bare grammar refuses becomes a double-quoted basic string). `None` declines the rename: an
/// escaped old key (the walk cannot verify the pairing), a dotted path (the region covers more than the renamed
/// member), or a new text no spelling can carry.
fn toml_rename_bytes(region: &[u8], old: &str, new: &str) -> Option<Vec<u8>> {
    if region.len() >= 2 && region[0] == b'"' && region[region.len() - 1] == b'"' {
        // A basic-quoted key. The region's inner text must BE the old key (an escaped spelling declines); the new key
        // renders basic-quoted through the grammar's own escape pass, at any length.
        let inner = &region[1..region.len() - 1];
        if inner != old.as_bytes() {
            return None;
        }
        let mut bytes = Vec::with_capacity(new.len() + 2);
        bytes.push(b'"');
        escape_basic_string(new, |chunk| {
            bytes.extend_from_slice(chunk);
            Ok::<(), core::convert::Infallible>(())
        })
        .ok()?;
        bytes.push(b'"');
        Some(bytes)
    } else if region.len() >= 2 && region[0] == b'\'' && region[region.len() - 1] == b'\'' {
        let inner = &region[1..region.len() - 1];
        if from_utf8(inner).ok()?.replace("''", "'") != old {
            return None;
        }
        if literal_admits(new) {
            Some(format!("'{new}'").into_bytes())
        } else {
            let mut bytes = Vec::with_capacity(new.len() + 2);
            bytes.push(b'"');
            escape_basic_string(new, |chunk| {
                bytes.extend_from_slice(chunk);
                Ok::<(), core::convert::Infallible>(())
            })
            .ok()?;
            bytes.push(b'"');
            Some(bytes)
        }
    } else {
        // A bare key: the region IS the old key. The new key stays bare when the grammar admits it; otherwise it
        // renders basic-quoted.
        if region != old.as_bytes() {
            return None;
        }
        if is_bare_key(new) {
            Some(new.as_bytes().to_vec())
        } else {
            let mut bytes = Vec::with_capacity(new.len() + 2);
            bytes.push(b'"');
            escape_basic_string(new, |chunk| {
                bytes.extend_from_slice(chunk);
                Ok::<(), core::convert::Infallible>(())
            })
            .ok()?;
            bytes.push(b'"');
            Some(bytes)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EncodeState {
    Active,
    InputFinished,
}

/// Every node's comment texts by position, indexed once per located document. The decoder attaches `toml.comment@1`
/// leading comments to a statement's VALUE node, `toml.comment_inline@1` to the statement's own line, and
/// `toml.comment_foot@1` to the section the run precedes. Re- emitting them — leading before the statement, inline
/// after its value, foot after the section's block, and the ROOT's leading as the document trailer — is what keeps a
/// TOML-to-TOML pipeline from deleting every comment in a config file.
#[derive(Clone, Default)]
struct CommentIndex {
    leading: alloc::collections::BTreeMap<jqf_data::NodeId, Vec<String>>,
    inline: alloc::collections::BTreeMap<jqf_data::NodeId, Vec<String>>,
    foot: alloc::collections::BTreeMap<jqf_data::NodeId, Vec<String>>,
}

impl CommentIndex {
    fn new() -> Self {
        Self::default()
    }
}

struct TomlEncoder {
    bytes: Vec<u8>,
    state: EncodeState,
    root_done: bool,
    /// Comments to re-emit on the located route, by owning node and position. Empty for owned values, which have no
    /// document behind them.
    comments: CommentIndex,
    /// Document identity the cached comment store was built from.
    comment_source: Option<DocumentId>,
    /// Pristine comment index; cloned into `comments` per item because emission consumes entries.
    comment_store: CommentIndex,
    /// Which output profile this encoder serves. The byte laws are shared; only the leap-second law differs (TOML 1.1
    /// removed second 60, so a second-60 time refuses under that profile).
    profile: DialectKind,
}

impl TomlEncoder {
    fn new(profile: DialectKind) -> Self {
        let mut bytes = Vec::new();
        let _ = bytes.try_reserve_exact(OFFER_BYTES);
        Self {
            bytes,
            state: EncodeState::Active,
            root_done: false,
            comments: CommentIndex::new(),
            comment_source: None,
            comment_store: CommentIndex::new(),
            profile,
        }
    }

    /// Reinitializes one recycled encoder for one more ordered item: drops every byte and flag a previous item may have
    /// left behind — including one that aborted mid-offer, whose partial staging and re-emitted comments must never
    /// reach the next item — leaving exactly the state a fresh [`EncoderFactoryImpl::start`] would have produced.
    fn reset(&mut self) {
        self.bytes.clear();
        self.state = EncodeState::Active;
        self.root_done = false;
        self.comments = CommentIndex::new();
    }

    fn push(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    /// Encodes the complete TOML document for one item into the staging buffer. The item must project to an object (a
    /// table).
    fn encode_item(&mut self, item: EncodeItem<'_, '_>, resources: &mut ResourceContext<'_>) -> Result<(), CodecError> {
        match item {
            EncodeItem::Owned(value) => {
                self.comment_source = None;
                self.comment_store = CommentIndex::new();
                self.encode_owned_root(value, resources)
            }
            EncodeItem::Located { product, node } => {
                let key = product.document().key();
                if self.comment_source != Some(key) {
                    self.comment_store = build_comment_index(product.document(), resources)?;
                    self.comment_source = Some(key);
                }
                self.comments = self.comment_store.clone();
                let view = product.document().value_view(node).map_err(map_data)?;
                self.encode_located_root(&view, resources)
            }
        }
    }

    /// Emits one node's leading comments, each as a `# text` line, with the cursor at column zero and the owner's own
    /// line still to come. The text is truncated at its first line break — the decoder never produces one, but a
    /// hand-built fact must not be able to smuggle a second statement.
    fn write_leading_comments(&mut self, node: jqf_data::NodeId) {
        let Some(texts) = self.comments.leading.remove(&node) else {
            return;
        };
        for text in texts {
            self.push(b"# ");
            let single_line = text.split(['\n', '\r']).next().unwrap_or("");
            self.push(single_line.as_bytes());
            self.push(b"\n");
        }
    }

    /// Emits one node's inline comment after its statement's value, before the statement's line terminator: ` # text`.
    fn write_inline_comment(&mut self, node: jqf_data::NodeId) {
        let Some(texts) = self.comments.inline.remove(&node) else {
            return;
        };
        for text in texts {
            self.push(b" # ");
            let single_line = text.split(['\n', '\r']).next().unwrap_or("");
            self.push(single_line.as_bytes());
        }
    }

    /// Emits one section's foot comments after its block, each as a `# text` line at column zero: the run the decoder
    /// attributed to the section between its last statement and the next `[header]`.
    fn write_foot_comments(&mut self, node: jqf_data::NodeId) {
        let Some(texts) = self.comments.foot.remove(&node) else {
            return;
        };
        for text in texts {
            self.push(b"# ");
            let single_line = text.split(['\n', '\r']).next().unwrap_or("");
            self.push(single_line.as_bytes());
            self.push(b"\n");
        }
    }

    fn encode_owned_root(&mut self, value: &Value, resources: &ResourceContext<'_>) -> Result<(), CodecError> {
        let Value::Object(object) = value else {
            return Err(unrepresentable());
        };
        // The root table's direct assignments and inline values first.
        for entry in object {
            if matches!(entry.value(), Value::Object(_)) {
                continue;
            }
            self.push_key(entry.key());
            self.push(b" = ");
            self.encode_owned_value(entry.value(), resources)?;
            self.push(b"\n");
        }
        // Child tables depth-first.
        for entry in object {
            if matches!(entry.value(), Value::Object(_)) {
                let mut path = Vec::new();
                self.encode_owned_child_table(entry.key(), entry.value(), &mut path, resources)?;
            }
        }
        Ok(())
    }

    fn encode_owned_child_table(
        &mut self,
        key: &str,
        value: &Value,
        path: &mut Vec<String>,
        resources: &ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        let Value::Object(object) = value else {
            return Err(unrepresentable());
        };
        path.push(key.to_owned());
        if !self.bytes.is_empty() {
            self.push(b"\n");
        }
        self.push(b"[");
        self.push_quoted_path(path);
        self.push(b"]\n");
        for entry in object {
            if matches!(entry.value(), Value::Object(_)) {
                continue;
            }
            self.push_key(entry.key());
            self.push(b" = ");
            self.encode_owned_value(entry.value(), resources)?;
            self.push(b"\n");
        }
        for entry in object {
            if matches!(entry.value(), Value::Object(_)) {
                self.encode_owned_child_table(entry.key(), entry.value(), path, resources)?;
            }
        }
        path.pop();
        Ok(())
    }

    /// Encodes one NEW table member the splice policy is appending: its `[path]` header (or `[[path]]` when `double`),
    /// its direct statements, and its nested tables as deeper sections — the same shape
    /// [`Self::encode_owned_child_table`] gives an existing table's subtree, without the blank line before the header
    /// (a splice preserves the source's own whitespace instead of inventing separators).
    fn encode_appended_table(
        &mut self,
        path: &[String],
        value: &Value,
        double: bool,
        resources: &ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        let Value::Object(object) = value.untagged() else {
            return Err(unrepresentable());
        };
        self.push(if double { b"[[" } else { b"[" });
        self.push_quoted_path(path);
        self.push(if double { b"]]\n" } else { b"]\n" });
        for entry in object {
            if matches!(entry.value(), Value::Object(_)) {
                continue;
            }
            self.push_key(entry.key());
            self.push(b" = ");
            self.encode_owned_value(entry.value(), resources)?;
            self.push(b"\n");
        }
        let mut child_path = path.to_vec();
        for entry in object {
            if matches!(entry.value(), Value::Object(_)) {
                child_path.push(entry.key().to_owned());
                self.encode_appended_table(&child_path, entry.value(), false, resources)?;
                child_path.pop();
            }
        }
        Ok(())
    }

    /// Writes a table header path: each component in its canonical spelling, joined with `.` and no surrounding
    /// whitespace.
    fn push_quoted_path(&mut self, path: &[String]) {
        for (index, component) in path.iter().enumerate() {
            if index > 0 {
                self.push(b".");
            }
            self.push_key(component);
        }
    }

    /// Writes one key in its canonical spelling: BARE when TOML's bare-key grammar admits it, a double-quoted basic
    /// string otherwise.
    ///
    /// Determinism is unaffected — the choice is a total function of the key's own bytes, so one key always spells
    /// one way. Quoting every key was deterministic too, and produced `"name" = "app"`, which is valid TOML that no
    /// hand-written file contains: an edited config's diff touched every line it did not change.
    fn push_key(&mut self, key: &str) {
        if is_bare_key(key) {
            self.push(key.as_bytes());
            return;
        }
        self.push(b"\"");
        self.push_escaped(key);
        self.push(b"\"");
    }

    /// Writes one scalar TOML has no native spelling for as a quoted canonical string, through the shared projection
    /// layer that records the event.
    ///
    /// Projected text is escape-free by the sink's contract, so the basic string needs no escape pass.
    fn push_projected_scalar(
        &mut self,
        scalar: &ScalarView<'_>,
        resources: &ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        let projection = classify_scalar(scalar, TOML_NATIVE, resources).ok_or_else(unrepresentable)?;
        self.push(b"\"");
        projection.write(&mut TrackedProjectionSink::new(&mut self.bytes), resources)?;
        self.push(b"\"");
        Ok(())
    }

    fn encode_owned_value(&mut self, value: &Value, resources: &ResourceContext<'_>) -> Result<(), CodecError> {
        match value {
            Value::String(text) => {
                self.push(b"\"");
                self.push_escaped(text.as_str());
                self.push(b"\"");
                Ok(())
            }
            Value::Bool(boolean) => {
                self.push(if *boolean { b"true" } else { b"false" });
                Ok(())
            }
            Value::Number(number) => self.encode_owned_number(number),
            Value::Array(array) => {
                self.push(b"[");
                for (index, item) in array.iter().enumerate() {
                    if index > 0 {
                        self.push(b", ");
                    }
                    self.encode_owned_value(item, resources)?;
                }
                self.push(b"]");
                Ok(())
            }
            Value::Object(object) => {
                // An object nested inside an array (or inline-table value) renders as a TOML inline table.
                self.push(b"{");
                for (index, entry) in object.iter().enumerate() {
                    if index > 0 {
                        self.push(b", ");
                    }
                    self.push_key(entry.key());
                    self.push(b" = ");
                    self.encode_owned_value(entry.value(), resources)?;
                }
                self.push(b"}");
                Ok(())
            }
            Value::Null => Err(unrepresentable()),
            Value::Bytes(bytes) => self.push_projected_scalar(&ScalarView::Bytes(bytes), resources),
            Value::Tagged { payload, .. } => {
                project_tag(resources);
                self.encode_owned_value(payload, resources)
            }
            // Under the flag an owned temporal writes as quoted text (a builtin-minted value; the document's own kinds
            // never reach this arm uncast).
            value if value.is_temporal() && resources.types_as_strings() => {
                self.push_projected_scalar(&ScalarView::from_value(value).ok_or_else(unrepresentable)?, resources)
            }
            Value::LocalDate(date) => {
                self.push_local_date(*date);
                Ok(())
            }
            Value::LocalTime(time) => self.push_local_time(time),
            Value::LocalDateTime(datetime) => {
                self.push_local_date(datetime.date);
                self.push(b"T");
                self.push_local_time(&datetime.time)
            }
            Value::OffsetDateTime(datetime) => {
                self.push_local_date(datetime.local.date);
                self.push(b"T");
                self.push_local_time(&datetime.local.time)?;
                self.push_offset(datetime.offset)
            }
        }
    }

    fn push_local_date(&mut self, date: jqf_data::LocalDate) {
        let mut buffer = [0u8; 10];
        // `LocalDate::new` caps the year at 9999, so every digit is one byte.
        let year = u32::from(date.year());
        buffer[0] = b'0' + (year / 1000 % 10) as u8;
        buffer[1] = b'0' + (year / 100 % 10) as u8;
        buffer[2] = b'0' + (year / 10 % 10) as u8;
        buffer[3] = b'0' + (year % 10) as u8;
        buffer[4] = b'-';
        buffer[5] = b'0' + date.month() / 10;
        buffer[6] = b'0' + date.month() % 10;
        buffer[7] = b'-';
        buffer[8] = b'0' + date.day() / 10;
        buffer[9] = b'0' + date.day() % 10;
        self.push(&buffer);
    }

    fn push_local_time(&mut self, time: &jqf_data::LocalTime) -> Result<(), CodecError> {
        // TOML 1.1 removed the leap second: second 60 has no 1.1 spelling, and emitting `23:59:60` would write bytes
        // the sibling 1.1 decoder rejects (the never-writes-unreadable-bytes law).
        if self.profile == DialectKind::Toml11 && time.second() == 60 {
            return Err(unrepresentable());
        }
        let mut buffer = [0u8; 9];
        buffer[0] = b'0' + time.hour() / 10;
        buffer[1] = b'0' + time.hour() % 10;
        buffer[2] = b':';
        buffer[3] = b'0' + time.minute() / 10;
        buffer[4] = b'0' + time.minute() % 10;
        buffer[5] = b':';
        buffer[6] = b'0' + time.second() / 10;
        buffer[7] = b'0' + time.second() % 10;
        let fraction = time.fraction().digits();
        if !fraction.is_empty() {
            buffer[8] = b'.';
            self.push(&buffer[..9]);
            self.push(fraction.as_bytes());
            return Ok(());
        }
        self.push(&buffer[..8]);
        Ok(())
    }

    fn push_offset(&mut self, offset: jqf_data::UtcOffset) -> Result<(), CodecError> {
        // The TOML offset grammar has minute granularity; a sub-minute offset has no spelling, and dropping the seconds
        // (writing `+00:00` for a 30-second offset) would be a silent value change. The guard belongs here, before
        // delegating to the shared writer.
        if let jqf_data::UtcOffset::KnownSeconds(known) = offset
            && known.seconds() % 60 != 0
        {
            return Err(unrepresentable());
        }
        match offset {
            jqf_data::UtcOffset::UnknownLocalOffset => self.push(b"-00:00"),
            jqf_data::UtcOffset::KnownSeconds(known) if known.seconds() == 0 => self.push(b"Z"),
            jqf_data::UtcOffset::KnownSeconds(known) => {
                let seconds = known.seconds();
                let mut buffer = [0u8; 6];
                buffer[0] = if seconds < 0 { b'-' } else { b'+' };
                let magnitude = seconds.unsigned_abs();
                let hours = magnitude / 3600;
                let minutes = magnitude % 3600 / 60;
                buffer[1] = DIGITS[(hours / 10) as usize];
                buffer[2] = DIGITS[(hours % 10) as usize];
                buffer[3] = b':';
                buffer[4] = DIGITS[(minutes / 10) as usize];
                buffer[5] = DIGITS[(minutes % 10) as usize];
                self.push(&buffer);
            }
        }
        Ok(())
    }

    fn encode_owned_number(&mut self, number: &jqf_data::Number) -> Result<(), CodecError> {
        // The inline machine arm renders its canonical spelling on demand; the boxed arm borrows its retained one.
        match number.to_integer() {
            // TOML 1.0.0 integers are a signed 64-bit range; a jqf exact integer outside it (the whole point of the
            // exact-integer kind is to hold values an `i64` cannot) has no valid TOML spelling.
            Some(integer) if integer.to_i64().is_some() => {
                self.push(integer.as_str().as_bytes());
                Ok(())
            }
            Some(_) => Err(unrepresentable()),
            None => match number.as_decimal() {
                // An exact decimal renders through the float path, with a `.0` reparse suffix when the spelling would
                // otherwise read back as a TOML integer — a JSON `1.5` encodes where the pre-decimal lane refused.
                Some(decimal) => self.push_decimal(decimal.coefficient().as_str(), decimal.scale()),
                None => match number.as_float() {
                    Some(float) => self.push_float(float),
                    None => Err(unrepresentable()),
                },
            },
        }
    }

    fn push_float(&mut self, float: jqf_data::Float) -> Result<(), CodecError> {
        let value = float.get();
        if value.is_nan() {
            let bits = float.bits();
            if bits == 0x7ff8_0000_0000_0000 {
                self.push(b"nan");
                Ok(())
            } else if bits == 0xfff8_0000_0000_0000 {
                self.push(b"-nan");
                Ok(())
            } else {
                // Any other NaN payload is unrepresentable in the deterministic subset (the fixed-NaN law).
                Err(unrepresentable())
            }
        } else if value.is_infinite() {
            self.push(if value.is_sign_positive() { b"inf" } else { b"-inf" });
            Ok(())
        } else if let Some(text) = jqf_data::format_binary64(value) {
            let rendered = text.as_str();
            // The deterministic float law must round-trip: a shortest rendering with NO decimal point or exponent (`1`,
            // `0`, `-0`, `1000000000000000`) would reparse as a TOML INTEGER, silently changing the value's category.
            // TOML requires digits on both sides of the point, so the `.0` suffix is the canonical integer-valued float
            // spelling.
            self.push(rendered.as_bytes());
            if !rendered.contains(['.', 'e', 'E']) {
                self.push(b".0");
            }
            Ok(())
        } else {
            Err(unrepresentable())
        }
    }

    /// Writes one exact decimal as a plain TOML number, with a `.0` suffix when the spelling would otherwise reparse as
    /// an integer.
    fn push_decimal(&mut self, coefficient: &str, scale: i64) -> Result<(), CodecError> {
        jqf_codec_core::decimal_render_into(coefficient, scale, true, &mut self.bytes)
    }

    fn is_object_view(view: &ValueView<'_, '_>) -> Result<bool, CodecError> {
        Ok(view.object().map_err(map_data)?.is_some())
    }

    fn encode_located_root(
        &mut self,
        view: &ValueView<'_, '_>,
        resources: &ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        let Some(object) = view.object().map_err(map_data)? else {
            return Err(unrepresentable());
        };
        let mut sections = Vec::new();
        for index in 0..object.len() {
            let entry = object.get_index(index).map_err(map_data)?.ok_or_else(unrepresentable)?;
            if Self::is_object_view(&entry.value())? {
                sections.push(index);
                continue;
            }
            self.write_leading_comments(entry.value().node());
            self.push_key(entry.key());
            self.push(b" = ");
            self.encode_located_value(&entry.value(), resources)?;
            self.write_inline_comment(entry.value().node());
            self.push(b"\n");
        }
        for index in sections {
            let entry = object.get_index(index).map_err(map_data)?.ok_or_else(unrepresentable)?;
            let mut path = Vec::new();
            self.encode_located_child_table(entry.key(), &entry.value(), &mut path, resources)?;
        }
        // The ROOT's leading comment is the document trailer (the decoder's ownership law: a comment with no following
        // node pins to the root), re-emitted after the whole document body where it was written.
        self.write_leading_comments(view.node());
        Ok(())
    }

    fn encode_located_child_table(
        &mut self,
        key: &str,
        child: &ValueView<'_, '_>,
        path: &mut Vec<String>,
        resources: &ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        let Some(object) = child.object().map_err(map_data)? else {
            return Err(unrepresentable());
        };
        path.push(key.to_owned());
        if !self.bytes.is_empty() {
            self.push(b"\n");
        }
        // A comment on the table's header statement is attached to the table node itself.
        self.write_leading_comments(child.node());
        self.push(b"[");
        self.push_quoted_path(path);
        self.push(b"]\n");
        let mut sections = Vec::new();
        for index in 0..object.len() {
            let entry = object.get_index(index).map_err(map_data)?.ok_or_else(unrepresentable)?;
            if Self::is_object_view(&entry.value())? {
                sections.push(index);
                continue;
            }
            self.write_leading_comments(entry.value().node());
            self.push_key(entry.key());
            self.push(b" = ");
            self.encode_located_value(&entry.value(), resources)?;
            self.write_inline_comment(entry.value().node());
            self.push(b"\n");
        }
        for index in sections {
            let entry = object.get_index(index).map_err(map_data)?.ok_or_else(unrepresentable)?;
            self.encode_located_child_table(entry.key(), &entry.value(), path, resources)?;
        }
        // The section's foot: the comment run the decoder attributed to this table between its last statement and the
        // next `[header]`.
        self.write_foot_comments(child.node());
        path.pop();
        Ok(())
    }

    fn encode_located_value(
        &mut self,
        view: &ValueView<'_, '_>,
        resources: &ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        // TOML spells no tag, so a tagged value publishes its payload — and says so. Before the projection layer this
        // path dropped the tag SILENTLY while the owned path refused: the same seam, two policies.
        if let TagLayer::Tagged(_) = view_tag_layer(*view)? {
            project_tag(resources);
        }
        if matches!(view.kind().map_err(map_data)?, jqf_data::ValueKind::Null) {
            return Err(unrepresentable());
        }
        if let Some(array) = view.array().map_err(map_data)? {
            self.push(b"[");
            for index in 0..array.len() {
                if index > 0 {
                    self.push(b", ");
                }
                let item = array.get(index).ok_or_else(unrepresentable)?;
                self.encode_located_value(&item, resources)?;
            }
            self.push(b"]");
            return Ok(());
        }
        if let Some(object) = view.object().map_err(map_data)? {
            // Inline table.
            self.push(b"{");
            for index in 0..object.len() {
                if index > 0 {
                    self.push(b", ");
                }
                let entry = object.get_index(index).map_err(map_data)?.ok_or_else(unrepresentable)?;
                self.push_key(entry.key());
                self.push(b" = ");
                self.encode_located_value(&entry.value(), resources)?;
            }
            self.push(b"}");
            return Ok(());
        }
        let scalar = view.scalar().map_err(map_data)?.ok_or_else(unrepresentable)?;
        // Under `--types-as-strings` a temporal writes as its quoted plain text on the located route too — the same
        // text a materialized string carries.
        if resources.types_as_strings() {
            match scalar {
                scalar if scalar.is_temporal() => {
                    return self.push_projected_scalar(&scalar, resources);
                }
                _ => {}
            }
        }
        match scalar {
            ScalarView::String(text) => {
                self.push(b"\"");
                self.push_escaped(text);
                self.push(b"\"");
                Ok(())
            }
            ScalarView::Bool(boolean) => {
                self.push(if boolean { b"true" } else { b"false" });
                Ok(())
            }
            ScalarView::Number(number) => self.encode_located_number(number),
            ScalarView::Null => Err(unrepresentable()),
            ScalarView::Bytes(bytes) => self.push_projected_scalar(&ScalarView::Bytes(bytes), resources),
            ScalarView::LocalDate(date) => {
                self.push_local_date(*date);
                Ok(())
            }
            ScalarView::LocalTime(time) => self.push_located_local_time(&time),
            ScalarView::LocalDateTime(datetime) => {
                self.push_local_date(datetime.date);
                self.push(b"T");
                self.push_located_local_time(&datetime.time)
            }
            ScalarView::OffsetDateTime(datetime) => {
                self.push_local_date(datetime.local.date);
                self.push(b"T");
                self.push_located_local_time(&datetime.local.time)?;
                self.push_offset(datetime.offset)
            }
        }
    }

    fn push_located_local_time(&mut self, time: &jqf_data::LocalTimeView<'_>) -> Result<(), CodecError> {
        // Same leap-second law as `push_local_time`.
        if self.profile == DialectKind::Toml11 && time.second == 60 {
            return Err(unrepresentable());
        }
        let mut buffer = [0u8; 9];
        buffer[0] = b'0' + time.hour / 10;
        buffer[1] = b'0' + time.hour % 10;
        buffer[2] = b':';
        buffer[3] = b'0' + time.minute / 10;
        buffer[4] = b'0' + time.minute % 10;
        buffer[5] = b':';
        buffer[6] = b'0' + time.second / 10;
        buffer[7] = b'0' + time.second % 10;
        if !time.fraction.is_empty() {
            buffer[8] = b'.';
            self.push(&buffer[..9]);
            self.push(time.fraction.as_bytes());
            return Ok(());
        }
        self.push(&buffer[..8]);
        Ok(())
    }

    fn encode_located_number(&mut self, number: NumberView<'_>) -> Result<(), CodecError> {
        match number {
            // Same TOML 1.0.0 i64 ceiling as `encode_owned_number`, checked on the borrowed canonical text directly (it
            // parses exactly when it fits, matching `Integer::to_i64`'s own heap-arm check).
            NumberView::Integer(text) if integer_fits_i64(text) => {
                self.push(text.as_bytes());
                Ok(())
            }
            NumberView::Integer(_) => Err(unrepresentable()),
            NumberView::Decimal { coefficient, scale } => self.push_decimal(coefficient, scale),
            NumberView::Number(number) => self.encode_owned_number(number),
            NumberView::Float(float) => self.push_float(float),
        }
    }

    fn push_escaped(&mut self, text: &str) {
        escape_basic_string(text, |bytes| -> Result<(), ()> {
            self.push(bytes);
            Ok(())
        })
        .expect("the infallible sink never fails");
    }

    const fn report() -> PreservationReport {
        PreservationReport::new(
            PreservationOutcome::Exact,
            PreservationOutcome::Omitted,
            PreservationOutcome::Exact,
            PreservationOutcome::Normalized,
        )
    }
}

/// Builds the leading-comment index for one located document (`toml.comment@1`: a list of texts on the statement's
/// value node).
///
/// A document from owned values, or one whose codec attaches no facts, has no fact reader: the index stays empty and
/// the render is plain.
fn build_comment_index(
    document: &jqf_data::Document<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<CommentIndex, CodecError> {
    use core::ops::ControlFlow;
    use jqf_data::FactPayloadView;

    let mut index = CommentIndex::new();
    let mut reader = match document.fact_reader(resources) {
        Ok(reader) => reader,
        Err(jqf_data::DataError::CapabilityUnavailable {
            capability: jqf_data::DocumentCapability::AttachedFacts,
        }) => {
            jqf_codec_core::comment::apply_encode_overlay(
                &mut index.leading,
                &mut index.inline,
                &mut index.foot,
                resources,
            );
            return Ok(index);
        }
        // Resource-class failures keep their cause (a ceiling refusal must read as the ceiling, never as an internal
        // bug); map_data routes them through their own classes and collapses only genuinely impossible local states to
        // the contract violation.
        Err(error) => return Err(map_data(error)),
    };
    let _ = reader
        .drain(resources, |fact| {
            let jqf_data::LocalOwnerRef::Node(node) = fact.owner() else {
                return ControlFlow::Continue(());
            };
            let map = if fact.role().as_str() == crate::parse::COMMENT_FACT {
                &mut index.leading
            } else if fact.role().as_str() == crate::parse::COMMENT_INLINE_FACT {
                &mut index.inline
            } else if fact.role().as_str() == crate::parse::COMMENT_FOOT_FACT {
                &mut index.foot
            } else {
                return ControlFlow::Continue(());
            };
            let FactPayloadView::List(texts) = fact.payload() else {
                return ControlFlow::Continue(());
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
            ControlFlow::<()>::Continue(())
        })
        .map_err(map_data)?;
    jqf_codec_core::comment::apply_encode_overlay(&mut index.leading, &mut index.inline, &mut index.foot, resources);
    Ok(index)
}

/// Whether TOML's bare-key grammar admits `key` verbatim.
///
/// The TOML 1.0.0 set — ASCII letters, ASCII digits, `_` and `-` — over a non-empty key. TOML 1.1's wider Unicode
/// bare keys are deliberately NOT taken: this encoder serves both output profiles, and the 1.0 set is the one every
/// TOML parser in the field accepts. An all-digit key such as `1234` is a legal bare key and reparses as the string
/// `"1234"`, so it needs no quoting either.
pub(crate) fn is_bare_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// Appends `text` to the sink escaped as the CONTENT of a TOML basic string (no surrounding quotes): the exact inverse
/// of the grammar's `\b \t \n \f \r \" \\ \u \U` decoder. The one source of the escape law, shared by the encoder (a
/// key the bare grammar refuses becomes a double-quoted basic string) and the walk's implicit-table synthesis (dotted
/// key components rebuilt from their decoded text). The sink is generic so the encoder pushes through its accounted
/// `Vec<u8>` without an intermediate buffer.
pub(crate) fn escape_basic_string<E>(text: &str, mut push: impl FnMut(&[u8]) -> Result<(), E>) -> Result<(), E> {
    let bytes = text.as_bytes();
    let mut start = 0;
    while start < bytes.len() {
        let run = prefix_len::<PlainString>(&bytes[start..]);
        if run > 0 {
            push(&bytes[start..start + run])?;
            start += run;
        }
        if start >= bytes.len() {
            break;
        }
        let ch = text[start..].chars().next().expect("stop is a char boundary");
        match ch {
            '"' => push(b"\\\"")?,
            '\\' => push(b"\\\\")?,
            '\u{0008}' => push(b"\\b")?,
            '\t' => push(b"\\t")?,
            '\n' => push(b"\\n")?,
            '\u{000C}' => push(b"\\f")?,
            '\r' => push(b"\\r")?,
            '\u{0000}'..='\u{001F}' | '\u{007F}' | '\u{2028}' | '\u{2029}' => {
                let mut buffer = [0u8; 6];
                push(encode_unicode_escape(ch as u32, &mut buffer))?;
            }
            ch if (ch as u32) > 0xFFFF => {
                let mut buffer = [0u8; 10];
                push(encode_unicode_escape_long(ch as u32, &mut buffer))?;
            }
            _ => {
                let mut buffer = [0u8; 4];
                push(ch.encode_utf8(&mut buffer).as_bytes())?;
            }
        }
        start += ch.len_utf8();
    }
    Ok(())
}

/// Length-first i64 range check: 18 or fewer digits always fit; 19/20-digit spellings straddle `i64::MAX/MIN` and still
/// parse.
fn integer_fits_i64(text: &str) -> bool {
    let digits = text.as_bytes().iter().copied().filter(u8::is_ascii_digit).count();
    if digits <= 18 {
        return true;
    }
    text.parse::<i64>().is_ok()
}

/// Encodes `\uXXXX` into `buffer`. Supplementary scalars (> U+FFFF) never reach this helper: the caller routes them to
/// [`encode_unicode_escape_long`] instead.
fn encode_unicode_escape(value: u32, buffer: &mut [u8; 6]) -> &[u8] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let nibble = |shift: u32| HEX[((value >> shift) & 0xF) as usize];
    buffer[0] = b'\\';
    buffer[1] = b'u';
    buffer[2] = nibble(12);
    buffer[3] = nibble(8);
    buffer[4] = nibble(4);
    buffer[5] = nibble(0);
    &buffer[..6]
}

/// Encodes `\U` plus eight lowercase hex digits into `buffer` (10 bytes).
fn encode_unicode_escape_long(value: u32, buffer: &mut [u8; 10]) -> &[u8] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    buffer[0] = b'\\';
    buffer[1] = b'U';
    for index in 0..8 {
        let shift = (7 - index) * 4;
        buffer[2 + index] = HEX[((value >> shift) & 0xF) as usize];
    }
    &buffer[..10]
}

// ------------------------------------------------------------------------- The splice policy (`render_edit_append`).
//
// The structural-edit law for TOML, written as rulings:
//
// 1. A new member of an INLINE table stays inline: it renders inside the    closing `}` (`point = { x = 1, z = 3 }`).
// 2. A new scalar/array member of a SECTION table lands after the table's last direct statement — the section keeps
//    its shape and its comments.
// 3. A new nested key whose `[a.b]` section EXISTS lands inside that section (its members are the section's
//    statements).
// 4. A new table member (whose section does not exist) opens a new section AFTER the parent's last entry — the
//    parent's subtree end.
// 5. A new root scalar lands among the root's direct statements (TOML's grammar forces root keys before every section
//    header); a new root    table lands after the last section.
// 6. A new item of an ARRAY-OF-TABLES appends a `[[path]]` block after the last element's subtree; a new item of an
//    inline array splices before    the closing `]`.
//
// Every splice re-verifies by re-decode: a splice the codec cannot prove (a direct statement without a span, a corner
// the byte tests misclassify) returns an empty insertion set and the caller falls back to the whole-document floor.

/// Whether a member value is a CHILD SECTION of its table: an object whose node span starts with `[` (a `[a]`/`[[a]]`
/// header line). An inline table's span starts with `{`; a span-less object (a dotted-key implicit table) has no header
/// of its own and is descended as a section.
fn is_child_section(document: &Document<'_>, view: ValueView<'_, '_>, source: &[u8]) -> Result<bool, CodecError> {
    if view.kind().map_err(map_data)? != ValueKind::Object {
        return Ok(false);
    }
    Ok(match document.node_source_span(view.node()).map_err(map_data)? {
        Some(span) => source.get(span.start() as usize).copied() == Some(b'['),
        None => true,
    })
}

/// The anchor for a new DIRECT statement of a table: the end of the value span of its last direct member (child
/// sections excluded), or the table's own header line end when it has no direct members at all. `None` means the anchor
/// cannot be named (a direct statement without a retained span would place the splice mid-section) and the caller
/// declines to the floor.
fn last_direct_anchor(
    document: &Document<'_>,
    view: ValueView<'_, '_>,
    source: &[u8],
) -> Result<Option<usize>, CodecError> {
    let object = view.object().map_err(map_data)?.ok_or_else(data_contract)?;
    let mut last_direct: Option<(usize, Option<usize>)> = None;
    for (index, entry) in object.iter().enumerate() {
        let entry = entry.map_err(map_data)?;
        let value = entry.value();
        if is_child_section(document, value, source)? {
            continue;
        }
        let span = document.node_source_span(value.node()).map_err(map_data)?;
        last_direct = Some((index, span.map(|span| span.end() as usize)));
    }
    match last_direct {
        Some((_, Some(end))) => Ok(Some(end)),
        Some((_, None)) => Ok(None),
        None => {
            // No direct members: the table's own header line anchors an empty section's first statement.
            let span = document.node_source_span(view.node()).map_err(map_data)?;
            Ok(span.map(|span| span.end() as usize))
        }
    }
}

/// The end of a table's whole subtree — the position a new child SECTION appends after (ruling 4: "after the parent's
/// last entry"). Descends the last-member chain while it is a child section, then anchors on the bottom table's last
/// direct statement (or its header line when it has none).
fn table_subtree_end(
    document: &Document<'_>,
    view: ValueView<'_, '_>,
    source: &[u8],
) -> Result<Option<usize>, CodecError> {
    let mut current = view;
    loop {
        let object = current.object().map_err(map_data)?.ok_or_else(data_contract)?;
        if object.is_empty() {
            return last_direct_anchor(document, current, source);
        }
        let last = object
            .get_index(object.len() - 1)
            .map_err(map_data)?
            .ok_or_else(data_contract)?;
        let last_value = last.value();
        if is_child_section(document, last_value, source)? {
            current = last_value;
            continue;
        }
        return last_direct_anchor(document, current, source);
    }
}

/// A whole `[table]` section's removal: from the header line's comment block through the end of the section's last
/// line. The table node's own span is its HEADER line alone, so the cut is assembled from the header (the start, with
/// the same leading-comment walk the line cut runs) and the subtree end (`table_subtree_end`, the same anchor the
/// structural append closes on, extended to the line break so a trailing comment on the last entry leaves with it).
fn table_section_cut(
    document: &Document<'_>,
    view: ValueView<'_, '_>,
    source: &[u8],
) -> Result<Option<EditRemoval>, CodecError> {
    let Some(header) = document.node_source_span(view.node()).map_err(map_data)? else {
        return Ok(None);
    };
    let span_start = header.start() as usize;
    let mut start = line_start(source, span_start);
    while start > 0 {
        let previous = line_start(source, start - 1);
        let line = &source[previous..start];
        if line.trim_ascii().is_empty() || line.trim_ascii_start().starts_with(b"#") {
            start = previous;
        } else {
            break;
        }
    }
    let Some(subtree_end) = table_subtree_end(document, view, source)? else {
        return Ok(None);
    };
    let tail = source[subtree_end..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(source.len(), |position| subtree_end + position);
    let end = if tail < source.len() { tail + 1 } else { tail };
    Ok(Some(EditRemoval {
        start,
        end,
        replacement: alloc::vec::Vec::new(),
    }))
}

/// A literal inline array's element removals, each following the JSON splice's member-cut law: the element plus exactly
/// one adjacent separator, in the file's own layout. An array-of-tables has no literal `[...]` region (its node binds
/// no span) and declines to the floor.
fn render_array_remove(
    document: &Document<'_>,
    container: NodeId,
    source: &[u8],
    items: &[(usize, NodeId)],
) -> Result<alloc::vec::Vec<EditRemoval>, CodecError> {
    let Some(span) = document.node_source_span(container).map_err(map_data)? else {
        return Ok(alloc::vec::Vec::new());
    };
    let open = span.start() as usize;
    if source.get(open) != Some(&b'[') {
        return Ok(alloc::vec::Vec::new());
    }
    let mut removals = alloc::vec::Vec::new();
    for (_, node) in items {
        let Some(item_span) = document.node_source_span(*node).map_err(map_data)? else {
            return Ok(alloc::vec::Vec::new());
        };
        let (token_start, value_end) = toml_value_extent(source, item_span);
        removals.push(toml_element_cut(source, open, token_start, value_end));
    }
    Ok(removals)
}

/// The element's authored extent: its span already covers the value bytes for a number/bool/temporal; a quoted string's
/// span is its INNER content, so the cut widens over the matching quote pair, exactly as the line cut does.
fn toml_value_extent(source: &[u8], span: jqf_source::Span) -> (usize, usize) {
    let mut start = span.start() as usize;
    let mut end = span.end() as usize;
    if start > 0
        && end < source.len()
        && let Some(quote) = source.get(start - 1).copied()
        && matches!(quote, b'"' | b'\'')
        && source.get(end) == Some(&quote)
    {
        start -= 1;
        end += 1;
    }
    (start, end)
}

/// The element cut, in two shapes. An element that OWNS its line — only whitespace before it on the line, and after
/// its value only its comma and then whitespace or a `#` comment to the line end — cuts as that whole line, comment
/// included: the shape a multi-line array's per-element comments live in, and the only cut that removes the element's
/// trailing comma without dangling it. Everything else is the JSON member cut: the preceding comma (the following one
/// when the element is first), plus the whitespace run the element owned.
fn toml_element_cut(source: &[u8], open: usize, token_start: usize, value_end: usize) -> EditRemoval {
    let line_begin = line_start(source, token_start);
    let line_has_only_element = source[line_begin..token_start].iter().all(u8::is_ascii_whitespace);
    let mut rest = value_end;
    let line_ends_with_element = loop {
        match source.get(rest) {
            Some(b' ' | b'\t') => rest += 1,
            Some(b',') => {
                rest += 1;
                let line_end = source[rest..]
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .map_or(source.len(), |position| rest + position);
                let tail = &source[rest..line_end];
                break tail.trim_ascii().is_empty() || tail.trim_ascii_start().starts_with(b"#");
            }
            _ => break false,
        }
    };
    if line_has_only_element && line_ends_with_element {
        let mut start = line_begin;
        while start > open {
            let previous = line_start(source, start - 1);
            let line = &source[previous..start];
            if line.trim_ascii().is_empty() || line.trim_ascii_start().starts_with(b"#") {
                start = previous;
            } else {
                break;
            }
        }
        let tail = source[value_end..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(source.len(), |position| value_end + position);
        let end = if tail < source.len() { tail + 1 } else { tail };
        return EditRemoval {
            start,
            end,
            replacement: alloc::vec::Vec::new(),
        };
    }
    member_cut(source, open, token_start, value_end)
}

/// The JSON splice's member cut, spelled for TOML's array separators: the preceding comma and the whitespace run the
/// element owned, or — for the FIRST element — the following comma, with the line break before the element as its
/// owned whitespace.
fn member_cut(source: &[u8], open: usize, token_start: usize, value_end: usize) -> EditRemoval {
    let mut c = token_start;
    while c > open && is_array_ws(source[c - 1]) {
        c -= 1;
    }
    if c > open && source.get(c - 1) == Some(&b',') {
        return EditRemoval {
            start: c - 1,
            end: value_end,
            replacement: alloc::vec::Vec::new(),
        };
    }
    // First element: the line break before it (or the opening delimiter) is its owned whitespace; the cut runs through
    // the FOLLOWING comma when one exists.
    let mut ls = token_start;
    while ls > open && source[ls - 1] != b'\n' {
        ls -= 1;
    }
    let start = if ls > open && source[ls - 1] == b'\n' {
        ls - 1
    } else {
        open + 1
    };
    let mut e = value_end;
    while e < source.len() && is_array_ws(source[e]) {
        e += 1;
    }
    let end = if source.get(e) == Some(&b',') { e + 1 } else { value_end };
    EditRemoval {
        start,
        end,
        replacement: alloc::vec::Vec::new(),
    }
}

const fn is_array_ws(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}

/// The start of the line containing `at`.
fn line_start(source: &[u8], at: usize) -> usize {
    source[..at]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |position| position + 1)
}

/// The splice that lands new statement text on its own line after `anchor`'s line: the position is the end of the line
/// containing `anchor` (so a trailing comment on that line survives), the text gets a leading newline only when that
/// line is not already terminated, and exactly one trailing newline.
fn line_splice(segment: &[u8], anchor: usize, text: &[u8]) -> EditInsertion {
    let line_end = segment[anchor..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(segment.len(), |position| anchor + position);
    let at = if line_end < segment.len() {
        line_end + 1
    } else {
        line_end
    };
    let mut bytes = Vec::with_capacity(text.len() + 2);
    if at == segment.len() && !segment.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    bytes.extend_from_slice(text);
    if !text.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    EditInsertion {
        at,
        bytes,
        replace: None,
    }
}

/// The inline-table splice (ruling 1): the added members render inside the closing `}` — `point = { x = 1, z = 3 }`.
fn render_inline_append(
    view: ValueView<'_, '_>,
    span: jqf_source::Span,
    source: &[u8],
    members: &[(&str, &Value)],
    profile: DialectKind,
    resources: &mut ResourceContext<'_>,
) -> Result<alloc::vec::Vec<EditInsertion>, CodecError> {
    let object = view.object().map_err(map_data)?.ok_or_else(data_contract)?;
    let mut text = Vec::new();
    if !object.is_empty() {
        text.extend_from_slice(b", ");
    }
    let mut encoder = fresh_encoder(profile);
    for (index, (key, value)) in members.iter().enumerate() {
        if index > 0 {
            encoder.push(b", ");
        }
        encoder.push_key(key);
        encoder.push(b" = ");
        encoder.encode_owned_value(value, resources)?;
    }
    text.extend_from_slice(encoder.bytes.as_slice());
    // Land the splice where the closing `}`'s leading whitespace begins, so `{ x = 1, y = 2 }` grows to `{ x = 1, y =
    // 2, z = 3 }` — the new member reads like a member, the original spacing survives.
    let mut at = span.end() as usize - 1;
    while at > span.start() as usize && matches!(source[at - 1], b' ' | b'\t') {
        at -= 1;
    }
    Ok(alloc::vec![EditInsertion {
        at,
        bytes: text,
        replace: None,
    }])
}

/// The TABLE-growth splice (rulings 2-5).
#[allow(clippy::too_many_arguments)]
fn render_table_append(
    document: &Document<'_>,
    view: ValueView<'_, '_>,
    container: NodeId,
    path: &[String],
    source: &[u8],
    members: &[(&str, &Value)],
    profile: DialectKind,
    resources: &mut ResourceContext<'_>,
) -> Result<alloc::vec::Vec<EditInsertion>, CodecError> {
    let is_root = container == document.root();
    let (scalars, tables): (alloc::vec::Vec<_>, alloc::vec::Vec<_>) = members
        .iter()
        .partition(|(_, value)| !matches!(value.untagged(), Value::Object(_)));
    let mut insertions = alloc::vec::Vec::new();
    if !scalars.is_empty() {
        let text = statement_text(profile, &scalars, resources)?;
        match last_direct_anchor(document, view, source)? {
            Some(anchor) => insertions.push(line_splice(source, anchor, &text)),
            None if is_root => {
                // A root with no direct statements: TOML's grammar forces root keys before every section header, so the
                // new statements open the file.
                insertions.push(EditInsertion {
                    at: 0,
                    bytes: text,
                    replace: None,
                });
            }
            None => return Ok(alloc::vec::Vec::new()),
        }
    }
    if !tables.is_empty() {
        let text = section_text(profile, path, &tables, resources)?;
        match table_subtree_end(document, view, source)? {
            Some(anchor) => insertions.push(line_splice(source, anchor, &text)),
            None if is_root => {
                // A root with no content at all: the new section opens the (possibly comment-only) file.
                insertions.push(EditInsertion {
                    at: source.len(),
                    bytes: text,
                    replace: None,
                });
            }
            None => return Ok(alloc::vec::Vec::new()),
        }
    }
    Ok(insertions)
}

/// The ARRAY-growth splice (ruling 6): a literal inline array grows before its closing `]`; an array-of-tables appends
/// `[[path]]` blocks after the last element's subtree.
fn render_array_append(
    document: &Document<'_>,
    view: ValueView<'_, '_>,
    path: &[String],
    source: &[u8],
    items: &[&Value],
    profile: DialectKind,
    resources: &mut ResourceContext<'_>,
) -> Result<alloc::vec::Vec<EditInsertion>, CodecError> {
    let span = document.node_source_span(view.node()).map_err(map_data)?;
    let Some(span) = span else {
        // An array-of-tables has no single value region (`[[p]]` blocks): the new elements append as `[[path]]` blocks
        // after the last element's subtree end.
        let array = view.array().map_err(map_data)?.ok_or_else(data_contract)?;
        let Some(last) = array.get(array.len().checked_sub(1).ok_or_else(data_contract)?) else {
            return Ok(alloc::vec::Vec::new());
        };
        let Some(anchor) = table_subtree_end(document, last, source)? else {
            return Ok(alloc::vec::Vec::new());
        };
        let mut encoder = fresh_encoder(profile);
        for item in items {
            encoder.encode_appended_table(path, item, true, resources)?;
        }
        let text = encoder.bytes.as_slice().to_vec();
        return Ok(alloc::vec![line_splice(source, anchor, &text)]);
    };
    // A literal inline array: the items splice before the closing `]`.
    let array = view.array().map_err(map_data)?.ok_or_else(data_contract)?;
    let mut text = Vec::new();
    if !array.is_empty() {
        text.extend_from_slice(b", ");
    }
    let mut encoder = fresh_encoder(profile);
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            encoder.push(b", ");
        }
        encoder.encode_owned_value(item, resources)?;
    }
    text.extend_from_slice(encoder.bytes.as_slice());
    Ok(alloc::vec![EditInsertion {
        at: span.end() as usize - 1,
        bytes: text,
        replace: None,
    }])
}

/// Renders each new DIRECT statement as `key = value\n` lines, in member order.
fn statement_text(
    profile: DialectKind,
    members: &[(&str, &Value)],
    resources: &mut ResourceContext<'_>,
) -> Result<alloc::vec::Vec<u8>, CodecError> {
    let mut encoder = fresh_encoder(profile);
    for (key, value) in members {
        encoder.push_key(key);
        encoder.push(b" = ");
        encoder.encode_owned_value(value, resources)?;
        encoder.push(b"\n");
    }
    Ok(encoder.bytes.as_slice().to_vec())
}

/// Renders each new TABLE member as its section tree: `[parent.key]` headers with the member's direct statements,
/// nested tables as deeper sections.
fn section_text(
    profile: DialectKind,
    parent_path: &[String],
    tables: &[(&str, &Value)],
    resources: &mut ResourceContext<'_>,
) -> Result<alloc::vec::Vec<u8>, CodecError> {
    let mut encoder = fresh_encoder(profile);
    for (key, value) in tables {
        let mut path = parent_path.to_vec();
        path.push((*key).to_owned());
        encoder.encode_appended_table(&path, value, false, resources)?;
    }
    Ok(encoder.bytes.as_slice().to_vec())
}

/// A fresh encoder for splice text (the bare-value grammar, no document behind it).
fn fresh_encoder(profile: DialectKind) -> TomlEncoder {
    TomlEncoder::new(profile)
}

/// The output profile a target dialect names. The two profiles share one byte law except the leap-second rule, so this
/// is the only fact the encoder needs from the target.
fn output_profile(dialect: &str) -> DialectKind {
    if dialect == crate::TOML_JQF_1_1_DIALECT_ID {
        DialectKind::Toml11
    } else {
        DialectKind::Toml10
    }
}

fn unrepresentable() -> CodecError {
    CodecError::new(CodecFailureKind::UnsupportedRepresentation)
}

fn map_data(error: DataError) -> CodecError {
    jqf_codec_core::map_data(error, "TOML encoder document access")
}

impl EncoderSession for TomlEncoder {
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
            if self.state == EncodeState::InputFinished {
                if !self.bytes.is_empty() {
                    sink.write_all(&self.bytes, context.resources())?;
                    self.bytes.clear();
                }
                return Ok(Self::report());
            }
            if self.root_done {
                self.state = EncodeState::InputFinished;
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
                        // Exactly one final LF; an empty root emits zero bytes.
                        if self.bytes.is_empty() {
                            self.state = EncodeState::InputFinished;
                        } else if self.bytes.as_slice().last() != Some(&b'\n') {
                            self.push(b"\n");
                        }
                        self.root_done = true;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jqf_data::{FractionalSecond, KnownUtcOffset, LocalDate, LocalDateTime, LocalTime, UtcOffset};

    fn test_encoder(profile: DialectKind) -> TomlEncoder {
        TomlEncoder::new(profile)
    }

    fn offset_value(seconds: i32) -> Value {
        Value::OffsetDateTime(jqf_data::OffsetDateTime {
            local: LocalDateTime {
                date: LocalDate::new(2024, 1, 1).expect("date"),
                time: LocalTime::new(12, 0, 0, FractionalSecond::parse("").expect("fraction")).expect("time"),
            },
            offset: UtcOffset::KnownSeconds(KnownUtcOffset::new(seconds).expect("offset")),
        })
    }

    #[test]
    fn output_profile_maps_the_two_dialects() {
        assert_eq!(output_profile(crate::TOML_JQF_1_0_DIALECT_ID), DialectKind::Toml10);
        assert_eq!(output_profile(crate::TOML_JQF_1_1_DIALECT_ID), DialectKind::Toml11);
    }

    #[test]
    fn sub_minute_offset_refuses() {
        let resources = crate::test_support::resources();
        let mut encoder = test_encoder(DialectKind::Toml10);
        let error = encoder
            .encode_owned_value(&offset_value(30), &resources)
            .expect_err("a 30-second offset has no TOML spelling");
        assert_eq!(error.kind(), CodecFailureKind::UnsupportedRepresentation);
        // The date-time prefix is staged before the offset refusal, exactly like the out-of-range-integer arm; the
        // request aborts so the staging is discarded.

        // Minute-aligned offsets still encode.
        let mut encoder = test_encoder(DialectKind::Toml10);
        encoder
            .encode_owned_value(&offset_value(7200), &resources)
            .expect("a two-hour offset encodes");
        assert!(encoder.bytes.as_slice().ends_with(b"+02:00"));
    }

    #[test]
    fn leap_second_refuses_under_the_1_1_profile() {
        let leap = LocalTime::new(23, 59, 60, FractionalSecond::parse("").expect("fraction")).expect("second 60");
        let resources = crate::test_support::resources();

        // TOML 1.0 keeps the leap second.
        let mut encoder = test_encoder(DialectKind::Toml10);
        encoder
            .encode_owned_value(&Value::LocalTime(leap.clone()), &resources)
            .expect("1.0 emits the leap second");
        assert_eq!(encoder.bytes.as_slice(), b"23:59:60");

        // The 1.1 profile refuses rather than emitting bytes its sibling decoder rejects.
        let mut encoder = test_encoder(DialectKind::Toml11);
        let error = encoder
            .encode_owned_value(&Value::LocalTime(leap), &resources)
            .expect_err("1.1 has no leap-second spelling");
        assert_eq!(error.kind(), CodecFailureKind::UnsupportedRepresentation);
    }

    #[test]
    fn decoded_1_0_leap_second_refuses_under_the_1_1_profile() {
        let resources = crate::test_support::resources();
        // Decode `t = 23:59:60` under TOML 1.0 (the only profile that admits it), then re-encode under each output
        // profile.
        let value = crate::tests::internal_tree_route_materialize(b"t = 23:59:60\n").expect("1.0 parse");

        let mut encoder = test_encoder(DialectKind::Toml10);
        encoder
            .encode_owned_root(&value, &resources)
            .expect("1.0 re-emits the leap second");
        assert_eq!(encoder.bytes.as_slice(), b"t = 23:59:60\n");

        let mut encoder = test_encoder(DialectKind::Toml11);
        let error = encoder
            .encode_owned_root(&value, &resources)
            .expect_err("1.1 output refuses second 60");
        assert_eq!(error.kind(), CodecFailureKind::UnsupportedRepresentation);
    }
}
