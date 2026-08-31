//! The deterministic flat-config encoder.
//!
//! `key<sep>value` lines, LF-terminated, with the canonical `=` separator; the INI grammar additionally writes
//! `[section]` headers for the one legal nesting level (root → sections). The preflight law: String, Integer, Decimal,
//! Float and Bool render as their canonical scalar text; `Null`, arrays, tags, and objects beyond the one INI section
//! level are UNREPRESENTABLE. A rendered value containing a line terminator is escaped under properties,
//! unrepresentable under ini, and double-quoted under dotenv. A LEADING blank cannot sit unquoted in any of the three
//! grammars — every decoder skips blanks between separator and value — so properties escapes it (`\ `), ini refuses,
//! and dotenv quotes. A TRAILING blank round-trips under properties verbatim (the properties decoder preserves trailing
//! raw whitespace), so properties writes it raw; ini refuses a value it would trim; dotenv quotes.
//!
//! # The edit splice policy
//!
//! The edit lane's three seams, all pinned by `tools/jqf-edit-differential.py`'s flat-config arms:
//!
//! 1. **A leaf patch replaces the VALUE's authored span** — the bytes as they appear AFTER `key<sep>`, never a key and
//!    never a newline ([`EncoderFactoryImpl::render_leaf`]). The new value renders through the same `push_scalar` law
//!    the document encoder writes, so a patched file re-decodes to the same document; a value the grammar cannot
//!    round-trip (a line terminator or leading/trailing blank under ini; an `export `-shaped key under dotenv) is
//!    unrepresentable and the SDK falls back to the floor.
//! 2. **A grown container splices new statements in the local syntax** ([`EncoderFactoryImpl::render_edit_append`]):
//!    - a new key in an existing section lands after that section's last entry, before any comment block that belongs
//!      to the next section's leading comments;
//!    - a new root key lands after the last root key, or at the top of the file when there is none;
//!    - a new section lands at end of file, preceded by exactly one blank line when the file does not already end in
//!      one;
//!    - the separator and spacing of the nearest sibling entry are COPIED (`a = 1` neighbours produce `b = 2`; `a=1`
//!      produce `b=2`) — a splice with no sibling (a new section's members, a root key at the top of an empty root)
//!      uses the canonical `=`;
//!    - a splice the policy cannot place (an array member, an object under properties/dotenv, a nested object under a
//!      section) returns an empty insertion set and the SDK falls back to the whole-document floor; the re-decode
//!      verification makes a wrong splice degrade the same way, never corrupt bytes.
//! 3. **A removed entry is its line cut** ([`EncoderFactoryImpl::render_edit_remove`]): the shared [`flat_line_cut`],
//!    which absorbs `#` / `;` / `!` comment lines and blank lines above the statement. A removed SECTION (an INI object
//!    member) is NOT one line — its member lines below the header would survive the header's removal and re-decode as
//!    root keys — so the policy declines and the floor re-encodes.
//!
//! **A duplicated key:** an edit to a key that occurs twice patches the WINNING (last) occurrence — the document holds
//! one value node per key (the `ObjectBuilder` last-wins law), so the diff addresses exactly the winner's span and the
//! shadowed earlier occurrences stay untouched bytes. The re-decode verification passes by construction.

use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec::Vec;

use jqf_codec_core::{
    ByteSink, CodecError, CodecFailureKind, CodecRunContext, EditAppendMembers, EditInsertion, EditRemoval,
    EditRemoveMembers, EncodeItem, EncodeRequest, EncoderFactoryImpl, EncoderSession, ErasedEncoderFactory,
    ErasedEncoderSession, PhysicalRouteId, PreservationOutcome, PreservationReport, PreservationRequest,
    RecycledSessionState,
};
use jqf_data::{NodeId, Value, ValueKind};
use jqf_resource::{ResourceContext, WorkAdmission};
use jqf_source::{Namespace, Severity};

use crate::options::Grammar;

const OFFER_BYTES: usize = 16 * 1024;

/// Builds the encoder factory for one output profile.
pub(crate) fn create_factory(
    request: EncodeRequest<'_, '_>,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedEncoderFactory, CodecError> {
    let grammar = match request.dialect.as_str() {
        crate::PROPERTIES_JQF_1_0_DIALECT_ID => Grammar::Properties,
        crate::INI_JQF_1_0_DIALECT_ID => Grammar::Ini,
        crate::DOTENV_JQF_1_0_DIALECT_ID => Grammar::Dotenv,
        _ => return Err(CodecError::new(CodecFailureKind::RequirementMismatch)),
    };
    if request.format.as_str() != grammar.format_id() {
        return Err(CodecError::new(CodecFailureKind::RequirementMismatch));
    }
    ErasedEncoderFactory::try_new_factory(request.preservation, resources, move || {
        Ok(FlatEncoderFactory { grammar })
    })
}

struct FlatEncoderFactory {
    grammar: Grammar,
}

impl EncoderFactoryImpl for FlatEncoderFactory {
    fn physical_encoder(&self) -> PhysicalRouteId {
        crate::encode_route_id(self.grammar)
    }

    fn start<'item, 'source>(
        &self,
        item: EncodeItem<'item, 'source>,
        preservation: PreservationRequest,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedEncoderSession<'item, 'source>, CodecError> {
        ErasedEncoderSession::try_new(item, preservation, || {
            Ok(FlatEncoder {
                bytes: Vec::new(),
                root_done: false,
                grammar: self.grammar,
                workspace: jqf_data::MaterializeWorkspace::new(),
                comments: CommentIndex::default(),
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
        let Some(encoder) = state.downcast_mut::<FlatEncoder>() else {
            return Ok(false);
        };
        encoder.reset();
        Ok(true)
    }

    fn render_leaf(
        &self,
        _document: &jqf_data::Document<'_>,
        _node: NodeId,
        _path: &[String],
        _source: &[u8],
        value: &Value,
        _authored: Option<&[u8]>,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<Vec<u8>, CodecError> {
        // The bare-value grammar: a leaf patch replaces a value's authored span with these exact bytes, so they must be
        // the value as it appears AFTER `key<sep>` — no key, no newline. Same `push_scalar` law as the document encoder
        // (properties escapes; ini/dotenv literal with the round-trip preflight), so a patched file re-decodes to the
        // same document; an unrenderable value declines to the floor through the SDK's leaf fallback.
        let mut encoder = FlatEncoder {
            bytes: Vec::new(),
            root_done: false,
            grammar: self.grammar,
            workspace: jqf_data::MaterializeWorkspace::new(),
            comments: CommentIndex::default(),
        };
        encoder.push_scalar(value)?;
        Ok(encoder.bytes)
    }

    fn render_edit_append(
        &self,
        document: &jqf_data::Document<'_>,
        container: NodeId,
        _path: &[String],
        source: &[u8],
        members: EditAppendMembers<'_>,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<alloc::vec::Vec<EditInsertion>, CodecError> {
        let EditAppendMembers::Table(members) = members else {
            // A flat config has no arrays: an array member cannot render as a `key=value` line, so the splice declines
            // to the floor.
            return Ok(alloc::vec::Vec::new());
        };
        if members.is_empty() {
            return Ok(alloc::vec::Vec::new());
        }
        render_table_append(document, container, source, members, self.grammar)
    }

    fn render_edit_remove(
        &self,
        document: &jqf_data::Document<'_>,
        _container: NodeId,
        _path: &[String],
        source: &[u8],
        members: EditRemoveMembers<'_>,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<alloc::vec::Vec<EditRemoval>, CodecError> {
        let mut removals = alloc::vec::Vec::new();
        for node in members.nodes() {
            // A removed SECTION (an INI object member) is not a one-line cut: its member lines below the header would
            // survive the header's removal and re-decode as root keys. The policy does not name that splice, so it
            // declines to the floor (the whole-document re-encode), never a wrong cut.
            let handle = document.node_handle(node).map_err(map_data)?;
            let kind = document
                .value_view(handle)
                .map_err(map_data)?
                .kind()
                .map_err(map_data)?;
            if kind == ValueKind::Object {
                return Ok(alloc::vec::Vec::new());
            }
            let Some(span) = document.node_source_span(node).map_err(map_data)? else {
                return Ok(alloc::vec::Vec::new());
            };
            // A key/value entry owns its whole line: the shared line cut (which absorbs the `#`-comment and blank lines
            // above the statement).
            let Some(removal) = flat_line_cut(source, span) else {
                return Ok(alloc::vec::Vec::new());
            };
            removals.push(removal);
        }
        Ok(removals)
    }
}

struct FlatEncoder {
    bytes: Vec<u8>,
    root_done: bool,
    grammar: Grammar,
    workspace: jqf_data::MaterializeWorkspace,
    /// The comments to re-emit on the located route, by owning node and position. Empty for owned values, which have no
    /// document behind them.
    comments: CommentIndex,
}

/// The comment texts to re-emit, by owning node and position. The flat-config grammars have ONE leading position per
/// entry (the `comment@1` fact on the value node) and ONE foot position on the root (the document trailer,
/// `comment_foot@1`); the INLINE position is absent by grammar (an inline `;`/`#` after a value is value text).
#[derive(Default)]
struct CommentIndex {
    leading: alloc::collections::BTreeMap<jqf_data::NodeId, Vec<String>>,
    foot: alloc::collections::BTreeMap<jqf_data::NodeId, Vec<String>>,
}

impl FlatEncoder {
    fn reset(&mut self) {
        self.bytes.clear();
        self.root_done = false;
        self.workspace = jqf_data::MaterializeWorkspace::new();
        self.comments = CommentIndex::default();
    }

    fn push(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    fn encode_item(&mut self, item: EncodeItem<'_, '_>, resources: &mut ResourceContext<'_>) -> Result<(), CodecError> {
        match item {
            EncodeItem::Owned(value) => {
                self.comments = CommentIndex::default();
                self.encode_root(value)
            }
            EncodeItem::Located { product, node } => {
                self.comments = build_comment_index(product.document(), self.grammar, resources)?;
                self.encode_located_root(product.document(), node, resources)
            }
        }
    }

    /// Encodes one complete document into the staging buffer.
    fn encode_root(&mut self, value: &Value) -> Result<(), CodecError> {
        let Value::Object(object) = value else {
            return Err(unrepresentable("the root must be an object"));
        };
        // Root-level scalar members first, in order.
        for entry in object {
            if matches!(entry.value(), Value::Object(_)) {
                continue;
            }
            self.push_key(entry.key())?;
            self.push(b"=");
            self.push_scalar(entry.value())?;
            self.push(b"\n");
        }
        if self.grammar != Grammar::Ini {
            // Only the INI grammar has a section level; a nested object under properties or dotenv is unrepresentable.
            for entry in object {
                if matches!(entry.value(), Value::Object(_)) {
                    return Err(unrepresentable("objects nest only one level under the ini grammar"));
                }
            }
            return Ok(());
        }
        for entry in object {
            let Value::Object(section) = entry.value() else {
                continue;
            };
            self.push(b"[");
            push_section_name(&mut self.bytes, entry.key())?;
            self.push(b"]\n");
            for member in section {
                if matches!(member.value(), Value::Object(_)) {
                    return Err(unrepresentable("objects nest only one level under the ini grammar"));
                }
                self.push_key(member.key())?;
                self.push(b"=");
                self.push_scalar(member.value())?;
                self.push(b"\n");
            }
        }
        Ok(())
    }

    /// Encodes one LOCATED document, re-emitting the comment facts the decoder attached: each entry's leading comments
    /// as `# text` lines before its own line, and the ROOT's foot (the document trailer) after the whole body. The
    /// located walk reads node ids from the document view so the index can key on them; the owned path has no document
    /// and re-emits nothing.
    fn encode_located_root(
        &mut self,
        document: &jqf_data::Document<'_>,
        node: jqf_data::NodeHandle,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        let view = document.value_view(node).map_err(map_data)?;
        let root_id = view.node();
        let object = view
            .object()
            .map_err(map_data)?
            .ok_or_else(|| unrepresentable("the root must be an object"))?;
        // Root-level scalar members first, in order.
        for index in 0..object.len() {
            let entry = object.get_index(index).map_err(map_data)?.ok_or_else(data_contract)?;
            if matches!(entry.value().kind().map_err(map_data)?, ValueKind::Object) {
                continue;
            }
            self.write_leading_comments(entry.value().node());
            self.push_key(entry.key())?;
            self.push(b"=");
            self.push_located_scalar(document, entry.value().node(), resources)?;
            self.push(b"\n");
        }
        if self.grammar != Grammar::Ini {
            // Only the INI grammar has a section level; a nested object under properties or dotenv is unrepresentable.
            for index in 0..object.len() {
                let entry = object.get_index(index).map_err(map_data)?.ok_or_else(data_contract)?;
                if matches!(entry.value().kind().map_err(map_data)?, ValueKind::Object) {
                    return Err(unrepresentable("objects nest only one level under the ini grammar"));
                }
            }
            self.write_foot_comments(root_id);
            return Ok(());
        }
        for index in 0..object.len() {
            let entry = object.get_index(index).map_err(map_data)?.ok_or_else(data_contract)?;
            if !matches!(entry.value().kind().map_err(map_data)?, ValueKind::Object) {
                continue;
            }
            self.push(b"[");
            push_section_name(&mut self.bytes, entry.key())?;
            self.push(b"]\n");
            let section = entry.value().object().map_err(map_data)?.ok_or_else(data_contract)?;
            for member_index in 0..section.len() {
                let member = section
                    .get_index(member_index)
                    .map_err(map_data)?
                    .ok_or_else(data_contract)?;
                if matches!(member.value().kind().map_err(map_data)?, ValueKind::Object) {
                    return Err(unrepresentable("objects nest only one level under the ini grammar"));
                }
                self.write_leading_comments(member.value().node());
                self.push_key(member.key())?;
                self.push(b"=");
                self.push_located_scalar(document, member.value().node(), resources)?;
                self.push(b"\n");
            }
        }
        // The ROOT's foot: the document trailer (comment lines after the last entry), re-emitted after the body where
        // it was written.
        self.write_foot_comments(root_id);
        Ok(())
    }

    /// Materializes one member's value node and renders it as a scalar (the located walk's per-member equivalent of
    /// `push_scalar` on an owned value).
    fn push_located_scalar(
        &mut self,
        document: &jqf_data::Document<'_>,
        node: NodeId,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        let handle = document.node_handle(node).map_err(map_data)?;
        let owned = document
            .materialize_node_with(&mut self.workspace, handle, resources)
            .map_err(map_data)?;
        self.push_scalar(&owned)
    }

    /// Emits one node's leading comment texts as `# text` lines, removing them from the index so they are never emitted
    /// twice.
    fn write_leading_comments(&mut self, node: NodeId) {
        if let Some(texts) = self.comments.leading.remove(&node) {
            self.push_comment_texts(texts);
        }
    }

    /// Emits one node's foot comment texts (the ROOT's document trailer) as `# text` lines.
    fn write_foot_comments(&mut self, node: NodeId) {
        if let Some(texts) = self.comments.foot.remove(&node) {
            self.push_comment_texts(texts);
        }
    }

    /// Pushes one comment text as a `# text` line. The text is truncated at its first line break — the decoder never
    /// produces one, but a hand-built fact must not be able to smuggle a second statement.
    fn push_comment_texts(&mut self, texts: Vec<String>) {
        for text in texts {
            let line = text.split(['\n', '\r']).next().unwrap_or("");
            self.push(b"# ");
            self.push(line.as_bytes());
            self.push(b"\n");
        }
    }

    /// Pushes one KEY with the grammar's escaping law.
    fn push_key(&mut self, key: &str) -> Result<(), CodecError> {
        match self.grammar {
            Grammar::Properties => {
                for (index, ch) in key.char_indices() {
                    if ch == '\n' {
                        self.push(b"\\n");
                        continue;
                    }
                    if ch == '\r' {
                        self.push(b"\\r");
                        continue;
                    }
                    let byte = ch as u32;
                    if matches!(byte, 0x5c /* \ */ | 0x3d /* = */ | 0x3a /* : */)
                        || is_blank_byte(byte)
                        || (index == 0 && matches!(byte, 0x23 | 0x21)/* # ! */)
                    {
                        self.push(b"\\");
                    }
                    push_char(&mut self.bytes, ch);
                }
                Ok(())
            }
            Grammar::Ini => {
                // Empty key is admitted: the decoder accepts `=value`.
                if key.starts_with([' ', '\t', '\u{000c}', '[', '#', ';'])
                    || key.ends_with([' ', '\t', '\u{000c}'])
                    || key.contains(['\n', '\r', '=', ':'])
                {
                    return Err(unrepresentable(
                        "a key containing a separator, a comment marker, or a leading/trailing blank cannot round-trip",
                    ));
                }
                self.push(key.as_bytes());
                Ok(())
            }
            Grammar::Dotenv => {
                // Empty key is admitted. `:` is a legal key byte (the only separator is `=`). An `export `
                // prefix-shaped key would strip on re-decode.
                if key.starts_with("export ")
                    || key.starts_with([' ', '\t', '\u{000c}', '#', ';'])
                    || key.ends_with([' ', '\t', '\u{000c}'])
                    || key.contains(['\n', '\r', '='])
                {
                    return Err(unrepresentable(
                        "a key containing a separator, a comment marker, an export prefix, or a leading/trailing blank cannot round-trip",
                    ));
                }
                self.push(key.as_bytes());
                Ok(())
            }
        }
    }

    /// Pushes one scalar VALUE with the round-trip preflight and the grammar's escaping law.
    fn push_scalar(&mut self, value: &Value) -> Result<(), CodecError> {
        let text = render_scalar(value)?;
        match self.grammar {
            Grammar::Properties => {
                let mut first = true;
                for ch in text.chars() {
                    match ch {
                        '\\' => self.push(b"\\\\"),
                        '\n' => self.push(b"\\n"),
                        '\r' => self.push(b"\\r"),
                        '\t' => self.push(b"\\t"),
                        '\u{000c}' => self.push(b"\\f"),
                        _ => {
                            // A LEADING blank must be escaped or the re-decode would skip it (the value would lose its
                            // first byte); later blanks are ordinary value bytes.
                            if first && is_blank_char(ch) {
                                self.push(b"\\");
                            }
                            push_char(&mut self.bytes, ch);
                        }
                    }
                    first = false;
                }
                Ok(())
            }
            Grammar::Ini => {
                if text.contains(['\n', '\r'])
                    || text.starts_with([' ', '\t', '\u{000c}'])
                    || text.ends_with([' ', '\t', '\u{000c}'])
                {
                    return Err(unrepresentable(
                        "a value containing a line terminator or a leading/trailing blank cannot round-trip",
                    ));
                }
                self.push(text.as_bytes());
                Ok(())
            }
            Grammar::Dotenv => {
                // Quote when the unquoted grammar cannot carry the value (newline, quote, backslash, leading/trailing
                // blank, or a leading apostrophe — the decoder would read it as opening a single-quoted literal and
                // reject the line).
                if text.contains(['\n', '\r', '\t', '"', '\\'])
                    || text.starts_with(['\'', ' ', '\t', '\u{000c}'])
                    || text.ends_with([' ', '\t', '\u{000c}'])
                {
                    self.push(b"\"");
                    for ch in text.chars() {
                        match ch {
                            '\n' => self.push(b"\\n"),
                            '\r' => self.push(b"\\r"),
                            '\t' => self.push(b"\\t"),
                            '\\' => self.push(b"\\\\"),
                            '"' => self.push(b"\\\""),
                            '$' => self.push(b"\\$"),
                            _ => push_char(&mut self.bytes, ch),
                        }
                    }
                    self.push(b"\"");
                } else {
                    self.push(text.as_bytes());
                }
                Ok(())
            }
        }
    }
}

impl EncoderSession for FlatEncoder {
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
                    return Ok(report());
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

/// The splice for a container the edit lane GREW. Returns an empty insertion set (the floor) for a member the policy
/// cannot place.
fn render_table_append(
    document: &jqf_data::Document<'_>,
    container: NodeId,
    source: &[u8],
    members: &[(&str, &Value)],
    grammar: Grammar,
) -> Result<alloc::vec::Vec<EditInsertion>, CodecError> {
    // The ROOT container is the only container with no recorded span (a section object's span is its `[name]` header
    // line) — but the root carries the whole document's authored span (the trailer-write anchor), so root-ness is
    // decided by identity, never by span absence.
    let root = document.root_handle().local() == container;
    let mut insertions = alloc::vec::Vec::new();
    for (key, value) in members {
        if let Value::Object(section) = value.untagged() {
            // A new SECTION (INI's one legal nesting level): at end of file, preceded by exactly one blank line when
            // the file does not already end in one.
            if grammar != Grammar::Ini {
                // Objects are unrepresentable under properties/dotenv.
                return Ok(alloc::vec::Vec::new());
            }
            let mut text = alloc::vec::Vec::new();
            if source.ends_with(b"\n\n") {
                // Already ends in a blank line.
            } else if source.ends_with(b"\n") {
                text.push(b'\n');
            } else {
                text.extend_from_slice(b"\n\n");
            }
            text.extend_from_slice(&render_section_text(
                grammar,
                key,
                section,
                &file_last_separator(document, source)?,
            )?);
            insertions.push(EditInsertion {
                at: source.len(),
                bytes: text,
                replace: None,
            });
            continue;
        }
        // A new KEY in this container. The anchor is the container's last direct entry (a section member for a section,
        // a root KEY for the root); `None` anchors the top of the file (the empty-root law).
        let (anchor, separator) = entry_anchor(document, container, source, root)?;
        let text = render_member_line(grammar, key, &separator, value)?;
        let at = anchor.map_or(0, |anchor| line_after(source, anchor));
        let mut bytes = alloc::vec::Vec::new();
        if at == source.len() && at > 0 && !source.ends_with(b"\n") {
            bytes.push(b'\n');
        }
        bytes.extend_from_slice(&text);
        insertions.push(EditInsertion {
            at,
            bytes,
            replace: None,
        });
    }
    Ok(insertions)
}

/// The anchor position and separator for a new KEY of `container`. The anchor is the line end of the container's last
/// DIRECT ENTRY — a root KEY (a scalar member — a section is not a root key) for the root, any member for a section —
/// `None` anchoring the top of the file (the empty-root law). An empty SECTION anchors its own header line. The
/// separator is the nearest sibling entry's authored spelling (the copy-the-sibling rule), falling back to the whole
/// file's last entry, then the canonical `=` for an entry-less file.
fn entry_anchor(
    document: &jqf_data::Document<'_>,
    container: NodeId,
    source: &[u8],
    root: bool,
) -> Result<(Option<usize>, alloc::vec::Vec<u8>), CodecError> {
    let handle = document.node_handle(container).map_err(map_data)?;
    let view = document.value_view(handle).map_err(map_data)?;
    let object = view.object().map_err(map_data)?.ok_or_else(data_contract)?;
    let mut last: Option<jqf_source::Span> = None;
    for entry in object.iter() {
        let entry = entry.map_err(map_data)?;
        let value = entry.value();
        if root && value.kind().map_err(map_data)? == ValueKind::Object {
            // A section member is not a root key. Number/Bool/String are.
            continue;
        }
        last = document.node_source_span(value.node()).map_err(map_data)?;
    }
    if let Some(span) = last {
        return Ok((Some(span.end() as usize), sibling_separator(source, span)));
    }
    if !root {
        // An empty SECTION anchors its own header line (the table's own header when it has no direct members).
        if let Some(span) = document.node_source_span(container).map_err(map_data)? {
            return Ok((Some(span.end() as usize), file_last_separator(document, source)?));
        }
    }
    Ok((None, file_last_separator(document, source)?))
}

/// The whole file's LAST ENTRY's authored separator — the last root key, or the last member of the last section — the
/// nearest sibling entry when a container has no direct entry of its own (a new section's members, a root key at the
/// top of a section-only file). The canonical `=` when the file has no entries at all.
fn file_last_separator(document: &jqf_data::Document<'_>, source: &[u8]) -> Result<alloc::vec::Vec<u8>, CodecError> {
    let root = document.root_handle();
    let view = document.value_view(root).map_err(map_data)?;
    let object = view.object().map_err(map_data)?.ok_or_else(data_contract)?;
    let mut last: Option<jqf_source::Span> = None;
    for entry in object.iter() {
        let entry = entry.map_err(map_data)?;
        let value = entry.value();
        let kind = value.kind().map_err(map_data)?;
        if kind == ValueKind::Object {
            // A section's last member is the file's last entry when the section is the last member.
            let section = value.object().map_err(map_data)?.ok_or_else(data_contract)?;
            for member in section.iter() {
                let member = member.map_err(map_data)?;
                last = document.node_source_span(member.value().node()).map_err(map_data)?;
            }
        } else {
            last = document.node_source_span(value.node()).map_err(map_data)?;
        }
    }
    Ok(last.map_or_else(|| b"=".to_vec(), |span| sibling_separator(source, span)))
}

/// The authored key→value separator of one entry, walked BACKWARD from its value span so a key containing a separator
/// (escaped under properties) never misleads the walk: `a = 1` yields ` = `, `a=1` yields `=`, `a b` (a properties
/// whitespace separator) yields ` `. The separator set (`=`, `:`, or blank-only under properties) is shared by all
/// three grammars.
fn sibling_separator(source: &[u8], span: jqf_source::Span) -> alloc::vec::Vec<u8> {
    let end = span.start() as usize;
    let mut p = end;
    while p > 0 && is_blank_byte(u32::from(source[p - 1])) {
        p -= 1;
    }
    if p > 0 && matches!(source[p - 1], b'=' | b':') {
        p -= 1;
        while p > 0 && is_blank_byte(u32::from(source[p - 1])) {
            p -= 1;
        }
    }
    source[p..end].to_vec()
}

/// One rendered `key<sep>value` line for a NEW member, using the encoder's own key/value escaping laws (the same bytes
/// the document encoder would write) and the given separator spelling.
fn render_member_line(
    grammar: Grammar,
    key: &str,
    separator: &[u8],
    value: &Value,
) -> Result<alloc::vec::Vec<u8>, CodecError> {
    let mut encoder = FlatEncoder {
        bytes: alloc::vec::Vec::new(),
        root_done: false,
        grammar,
        workspace: jqf_data::MaterializeWorkspace::new(),
        comments: CommentIndex::default(),
    };
    if grammar == Grammar::Dotenv
        && key == "export"
        && separator.first().is_some_and(|byte| is_blank_byte(u32::from(*byte)))
    {
        return Err(unrepresentable(
            "an export-prefix-shaped key cannot round-trip under a blank-leading separator",
        ));
    }
    encoder.push_key(key)?;
    encoder.push(separator);
    encoder.push_scalar(value)?;
    encoder.push(b"\n");
    Ok(encoder.bytes)
}

/// Renders a whole new INI section: `[name]` header plus its member lines with the given separator spelling (the file's
/// nearest sibling — a new section has no entries of its own to copy from). A member that is itself an object is
/// unrepresentable (the one-section-level law).
fn render_section_text(
    grammar: Grammar,
    name: &str,
    members: &jqf_data::Object,
    separator: &[u8],
) -> Result<alloc::vec::Vec<u8>, CodecError> {
    let mut out = alloc::vec::Vec::new();
    out.push(b'[');
    {
        let mut encoder = FlatEncoder {
            bytes: alloc::vec::Vec::new(),
            root_done: false,
            grammar,
            workspace: jqf_data::MaterializeWorkspace::new(),
            comments: CommentIndex::default(),
        };
        push_section_name(&mut encoder.bytes, name)?;
        out.extend_from_slice(&encoder.bytes);
    }
    out.push(b']');
    out.push(b'\n');
    for member in members {
        if matches!(member.value(), Value::Object(_)) {
            return Err(unrepresentable("objects nest only one level under the ini grammar"));
        }
        out.extend_from_slice(&render_member_line(grammar, member.key(), separator, member.value())?);
    }
    Ok(out)
}

/// Indexes every node's comment texts by position from the document's facts: the `<fmt>.comment@1` leading lists per
/// value node and the `<fmt>.comment_foot@1` document trailer on the root. A document without attached facts (an
/// owned-value build) indexes nothing.
fn build_comment_index(
    document: &jqf_data::Document<'_>,
    grammar: Grammar,
    resources: &mut ResourceContext<'_>,
) -> Result<CommentIndex, CodecError> {
    use jqf_data::{FactPayloadView, ReaderPoll, unbounded_batch_limit};

    let mut index = CommentIndex::default();
    let mut reader = match document.fact_reader(resources) {
        Ok(reader) => reader,
        Err(jqf_data::DataError::CapabilityUnavailable {
            capability: jqf_data::DocumentCapability::AttachedFacts,
        }) => return Ok(index),
        // Resource-class failures keep their cause (a ceiling refusal must read as the ceiling, never as an internal
        // bug); map_data routes them through their own classes and collapses only genuinely impossible local states to
        // the contract violation.
        Err(error) => return Err(map_data(error)),
    };
    let limit = unbounded_batch_limit();
    loop {
        let poll = reader.poll_batch(limit, resources).map_err(map_data)?;
        match poll {
            ReaderPoll::Batch(batch) => {
                for fact in batch.iter() {
                    let jqf_data::LocalOwnerRef::Node(node) = fact.owner() else {
                        continue;
                    };
                    let map = if fact.role().as_str() == crate::options::Grammar::comment_fact(grammar) {
                        &mut index.leading
                    } else if fact.role().as_str() == crate::materialize::comment_foot_kind(grammar) {
                        &mut index.foot
                    } else {
                        continue;
                    };
                    let FactPayloadView::List(texts) = fact.payload() else {
                        continue;
                    };
                    let mut out = Vec::new();
                    for entry in texts.iter() {
                        if let FactPayloadView::Text(text) = entry {
                            out.push(String::from(text));
                        }
                    }
                    map.insert(node, out);
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

/// The offset one past the line containing `anchor`'s newline (or EOF when the line is unterminated) — the position a
/// new statement lands after its anchor entry's line, so a trailing comment on that line survives.
fn line_after(source: &[u8], anchor: usize) -> usize {
    source[anchor..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(source.len(), |position| anchor + position + 1)
}

/// The flat-config preservation evidence: the value is re-encoded semantically, never byte-preserved; ordering is
/// retained; no source is reused.
const fn report() -> PreservationReport {
    PreservationReport::new(
        PreservationOutcome::Omitted,
        PreservationOutcome::Omitted,
        PreservationOutcome::Exact,
        PreservationOutcome::Omitted,
    )
}

/// Renders one scalar to its canonical text under the render vocabulary.
fn render_scalar(value: &Value) -> Result<String, CodecError> {
    let mut out = String::new();
    match value {
        Value::String(text) => out.push_str(text.as_str()),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => push_number(&mut out, number)?,
        Value::Null => {
            return Err(unrepresentable("null has no spelling distinct from the empty value"));
        }
        Value::Bytes(_)
        | Value::LocalDate(_)
        | Value::LocalTime(_)
        | Value::LocalDateTime(_)
        | Value::OffsetDateTime(_)
        | Value::Tagged { .. }
        | Value::Array(_)
        | Value::Object(_) => {
            return Err(unrepresentable("only string and number scalars render"));
        }
    }
    Ok(out)
}

/// Appends one owned number in the family's canonical text law.
fn push_number(out: &mut String, number: &jqf_data::Number) -> Result<(), CodecError> {
    if let Some(machine) = number.as_machine() {
        out.push_str(jqf_data::Integer::from_i64(machine).as_str());
        return Ok(());
    }
    if let Some(integer) = number.as_integer() {
        out.push_str(integer.as_str());
        return Ok(());
    }
    if let Some(decimal) = number.as_decimal() {
        push_decimal_plain(out, decimal.coefficient().as_str(), decimal.scale())?;
        return Ok(());
    }
    match number.as_float() {
        Some(value) => {
            let text = match jqf_data::format_binary64(value.get()) {
                Some(text) => text.as_str().to_owned(),
                None => value.get().to_string(),
            };
            out.push_str(&text);
            Ok(())
        }
        None => Err(unrepresentable("a number carrying no representation")),
    }
}

/// Appends an exact decimal positionally: `coefficient * 10^-scale` with no exponent, preserving the stored coefficient
/// digits. A scale whose magnitude does not fit a modest fill (including `i64::MIN`) is unrepresentable — `-scale` is
/// not taken.
fn push_decimal_plain(out: &mut String, coefficient: &str, scale: i64) -> Result<(), CodecError> {
    const MAX_FILL: usize = 4096;
    let (negative, digits) = match coefficient.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, coefficient),
    };
    if negative {
        out.push('-');
    }
    if scale <= 0 {
        out.push_str(digits);
        let zeros = usize::try_from(scale.unsigned_abs()).ok().filter(|&n| n <= MAX_FILL);
        let Some(zeros) = zeros else {
            return Err(unrepresentable("decimal scale overflow"));
        };
        for _ in 0..zeros {
            out.push('0');
        }
        return Ok(());
    }
    let Some(point) = usize::try_from(scale).ok().filter(|&n| n <= MAX_FILL) else {
        return Err(unrepresentable("decimal scale overflow"));
    };
    if digits.len() > point {
        let split = digits.len() - point;
        out.push_str(&digits[..split]);
        out.push('.');
        out.push_str(&digits[split..]);
    } else {
        out.push_str("0.");
        for _ in 0..(point - digits.len()) {
            out.push('0');
        }
        out.push_str(digits);
    }
    Ok(())
}

/// Writes an INI section name. Unlike [`FlatEncoder::push_key`], `=`, `:`, and a leading `#` / `;` are legal between
/// the brackets; empty, `]`, and line breaks are not.
fn push_section_name(out: &mut Vec<u8>, name: &str) -> Result<(), CodecError> {
    if name.is_empty() || name.contains([']', '\n', '\r']) {
        return Err(unrepresentable("a section name that cannot round-trip"));
    }
    out.extend_from_slice(name.as_bytes());
    Ok(())
}

/// The whole-line cut for one flat-config entry: like the shared `#` line cut, plus INI `;` and properties `!` comment
/// openers.
fn flat_line_cut(source: &[u8], span: jqf_source::Span) -> Option<EditRemoval> {
    let span_start = span.start() as usize;
    let span_end = span.end() as usize;
    if span_end > source.len() || span_start > span_end {
        return None;
    }
    let line_start = |at: usize| {
        source[..at]
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |position| position + 1)
    };
    let is_comment_line = |line: &[u8]| {
        let trimmed = line.trim_ascii_start();
        trimmed.first().is_some_and(|byte| matches!(byte, b'#' | b';' | b'!'))
    };
    let mut start = line_start(span_start);
    while start > 0 {
        let previous = line_start(start - 1);
        let line = &source[previous..start];
        if line.trim_ascii().is_empty() || is_comment_line(line) {
            start = previous;
        } else {
            break;
        }
    }
    let quote = source.get(span_start.wrapping_sub(1)).copied();
    let span_end = if quote.is_some_and(|byte| byte == b'"' || byte == b'\'') && source.get(span_end) == quote.as_ref()
    {
        span_end + 1
    } else {
        span_end
    };
    let tail = source[span_end..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(source.len(), |position| span_end + position);
    if !source[span_end..tail].iter().all(u8::is_ascii_whitespace) {
        return None;
    }
    let end = if tail < source.len() { tail + 1 } else { tail };
    Some(EditRemoval {
        start,
        end,
        replacement: Vec::new(),
    })
}

const fn is_blank_byte(byte: u32) -> bool {
    matches!(byte, 0x20 | 0x09 | 0x0c)
}

const fn is_blank_char(ch: char) -> bool {
    matches!(ch, ' ' | '\t' | '\u{000c}')
}

fn push_char(out: &mut Vec<u8>, ch: char) {
    let mut buffer = [0u8; 4];
    out.extend_from_slice(ch.encode_utf8(&mut buffer).as_bytes());
}

fn unrepresentable(message: &str) -> CodecError {
    // The message-only carrier builds fallibly; on refusal the bare failure survives (an unrepresentable document never
    // gets worse — the XML encoder's law).
    let base = CodecError::new(CodecFailureKind::UnsupportedRepresentation);
    let Some(diagnostic) = jqf_source::Diagnostic::try_new(
        Namespace::new("flat-config").code("unrepresentable"),
        Severity::Error,
        message,
    ) else {
        return base;
    };
    base.with_diagnostic(diagnostic)
}

fn map_data(error: jqf_data::DataError) -> CodecError {
    jqf_codec_core::map_data(error, "flat-config encoder rejected document construction")
}

fn data_contract() -> CodecError {
    jqf_codec_core::data_contract("flat-config authoritative document construction")
}
