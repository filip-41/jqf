//! XML 1.0 pull parser with a namespace stack.
//!
//! A borrow-free state machine that consumes the retained source bytes and
//! produces the private [`Tree`]. Per §4.9 it is a secure
//! non-validating processor: the prolog, doctype internal subset, namespace
//! resolution, attributes, mixed content, comments, processing instructions,
//! CDATA, and the five predefined plus internal general entities are handled;
//! external entities are disabled, nesting and replacement are bounded, and
//! character/name/attribute/expanded-name validity is enforced. The encoding
//! declaration is a grammar step of the selected format, never general
//! autodetection.

use alloc::borrow::{Cow, ToOwned};
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use jqf_codec_core::byte_scan::{StopSet, Ws, prefix_len};
use jqf_codec_core::{CodecError, CodecFailureKind, data_contract};
use jqf_resource::{ResourceContext, ResourceError, ResourceLimit};
use jqf_source::{Namespace, Severity};

use crate::value::{ContentEvent, Element, ExpandedName, NameId, NameInterner, Tree};

/// The XML text terminators: `<` and `&` (entity-free text is one scan).
#[derive(Clone, Copy)]
struct Xml;
impl StopSet for Xml {
    const EQ: [u8; 8] = [b'<', b'&', 0, 0, 0, 0, 0, 0];
    const EQ_LEN: u8 = 2;
    const LT: Option<u8> = None;
    const GE: Option<u8> = None;
    const ALL: bool = false;
}

/// The XML attribute-value terminators: `"`, `'`, `<`, `&`. Both quote
/// types are in the set so one specialization serves either quoting; the
/// caller re-admits the quote type that is not the value's own delimiter
/// (a `'` inside a double-quoted value is an ordinary character).
#[derive(Clone, Copy)]
struct XmlAttrValue;
impl StopSet for XmlAttrValue {
    const EQ: [u8; 8] = [b'"', b'\'', b'<', b'&', 0, 0, 0, 0];
    const EQ_LEN: u8 = 4;
    const LT: Option<u8> = None;
    const GE: Option<u8> = None;
    const ALL: bool = false;
}

/// The XML character-data invalid leads: in a valid-UTF-8 run with no `<`,
/// `&`, or quote, the bytes that can begin an XML-invalid character. These
/// are the C0 controls (any byte `< 0x20`; tab/LF/CR are re-admitted by the
/// caller) and the U+FFFE/U+FFFF lead `0xEF`. The 3-byte non-character
/// pattern is verified at the stop byte by the XML adopter. Four-byte
/// plane-end non-characters (U+1FFFE and up) are NOT in the set: the XML 1.0
/// Char production accepts every scalar value in `[#x10000-#x10FFFF]`,
/// exactly as the adopter's `is_char` does.
#[derive(Clone, Copy)]
struct XmlCharInvalid;
impl StopSet for XmlCharInvalid {
    const EQ: [u8; 8] = [0xEF, 0, 0, 0, 0, 0, 0, 0];
    const EQ_LEN: u8 = 1;
    const LT: Option<u8> = Some(0x20);
    const GE: Option<u8> = None;
    const ALL: bool = false;
}

/// Bounds the total replacement text emitted by GENERAL entity resolution,
/// in UTF-8 bytes. Character and predefined references are not counted: a
/// reference of that kind consumes more source bytes than its expansion
/// emits, so it cannot amplify and needs no cap of its own.
const MAX_ENTITY_REPLACEMENT_BYTES: usize = 16 * 1024 * 1024;
/// Bounds the number of distinct internal general entities remembered.
const MAX_ENTITY_COUNT: usize = 4096;
/// Bounds how deeply an entity's replacement text may reference further
/// entities. The in-flight-name walk already rejects a cycle; this also
/// caps a legal-shaped chain, whose length would otherwise scale with the
/// declared-entity count.
const MAX_ENTITY_EXPANSION_DEPTH: usize = 32;

/// The finished parse output.
#[allow(
    clippy::large_enum_variant,
    reason = "the Document arm carries the accounted builder by value on purpose: it is \
              constructed and consumed once per parsed document, and boxing it would add a \
              per-document heap allocation on the decode path"
)]
pub(crate) enum ParseOutput {
    /// The materialized element tree (build retention).
    Tree(Tree),
    /// The count skeleton (measure retention): the document element's direct
    /// children, validated and recorded without building the tree.
    Measure(alloc::vec::Vec<MeasureChild>),
    /// Scoped locate: matched extents / leaves, no whole-document tree.
    Located(crate::locate::LocatedHit),
}

/// One direct child of the document element, recorded by a measure-mode parse.
///
/// The count skeleton's child ledger: every distinct child the tree build
/// would produce (text coalesced), as either a re-parseable element extent or
/// a leaf value. The count consumer answers `length` over the document from
/// exactly this ledger.
#[derive(Debug)]
pub(crate) enum MeasureChild {
    /// A child ELEMENT's source extent `[start, end)` — a standalone XML
    /// document text the span materializer re-parses.
    Element {
        /// The child's first byte (`<`).
        start: usize,
        /// One past the child's last byte (past the end tag's `>` or `/>`).
        end: usize,
    },
    /// A character-data leaf (its decoded text; adjacent runs are coalesced).
    Text(alloc::string::String),
    /// A comment leaf (the comment text).
    Comment(alloc::string::String),
    /// A processing-instruction leaf (its target and data).
    ProcessingInstruction {
        /// The PI target.
        target: alloc::string::String,
        /// The PI data.
        data: alloc::string::String,
    },
}

/// The demand-scoped retention of a parse. The validate-everything-first law
/// is unconditional — every mode scans and validates the whole source with the
/// identical grammar and errors — but only [`Retention::Build`] materializes
/// the [`Tree`]. [`Retention::Measure`] (the count skeleton) builds
/// nothing and records the document element's direct children instead.
#[derive(Debug)]
pub(crate) enum Retention {
    /// Materialize the whole element tree (tests and span re-parse).
    Build,
    /// Validate everything, build nothing, and record the document element's
    /// direct children.
    Measure(alloc::vec::Vec<MeasureChild>),
    /// Validate everything, record only the document element's direct
    /// children (names + extents), then apply the exact path.
    Locate(LocateRetention),
}

/// Locate-mode path and the document element's collected children.
#[derive(Debug)]
pub(crate) struct LocateRetention {
    steps: alloc::vec::Vec<crate::locate::OwnedStep>,
    children: alloc::vec::Vec<crate::locate::LocateChild>,
    root_name: Option<crate::value::ExpandedName>,
    root_attrs: alloc::vec::Vec<crate::value::ExpandedName>,
    root_start: usize,
    root_end: usize,
    last_was_text: bool,
    /// False when this parse is a child span of a deeper path step, so an
    /// own-name miss uses the nested-element hint rather than the document
    /// root's projection-seam hint.
    is_document_root: bool,
}

/// One resumable-parse observation.
#[allow(
    clippy::large_enum_variant,
    reason = "Ready carries the whole parse output by value on purpose: it is produced once \
              per document and consumed immediately by the session that polled it"
)]
pub(crate) enum ParsePoll {
    /// The cooperative entry's work credits are spent; re-poll after the
    /// caller replenishes.
    Pending,
    /// The parse finished.
    Ready(ParseOutput),
}

/// A resolved start tag ready to open.
struct StartTag {
    prefix: NameId,
    local: NameId,
    attributes: Vec<RawAttribute>,
    self_closing: bool,
}

/// One raw attribute: the authored prefix, local name, and the unparsed
/// value, plus the authored span of the QUOTED value bytes.
struct RawAttribute {
    prefix: NameId,
    local: NameId,
    value: String,
    /// The authored `"value"` span: from the opening quote byte through one
    /// past the closing quote byte.
    span: (usize, usize),
}

/// One open element frame.
struct OpenFrame {
    element_index: usize,
    /// The namespace bindings declared by this element's start tag.
    bindings: Vec<(NameId, NameId)>,
    /// The resolved expanded name, for end-tag matching (measure retention
    /// builds no tree to carry it).
    name: ExpandedName,
    /// Whether the last distinct child was text.
    last_was_text: bool,
}

/// The XML parser state. One `parse()` / `poll` drive consumes the whole source;
/// retention is chosen at construction (tree, measure, or locate) and does not
/// change for the drive.
pub(crate) struct XmlParseState {
    position: usize,
    /// Whether we have passed the prolog.
    in_content: bool,
    /// The open element frames.
    frames: Vec<OpenFrame>,
    /// Flat namespace binding stack: appended on open, popped on close.
    namespace_bindings: Vec<(NameId, NameId)>,
    /// The finished element tree being built.
    tree: Tree,
    /// Interned names and namespace URIs for this parse.
    intern: NameInterner,
    /// Whether authored attribute/content spans are recorded (edit lane).
    /// Off on scoped and span-materialize parses.
    record_spans: bool,
    /// Internal general entities (`name -> value`).
    entities: BTreeMap<String, String>,
    /// Total replacement bytes emitted by entity resolution.
    entity_replacement_bytes: usize,
    /// Whether the document element has been opened (for the prolog).
    prolog_comments: Vec<String>,
    /// Whether the document element was ever opened (the second-root and
    /// no-element checks cannot read the tree in measure retention).
    document_element_seen: bool,
    /// The parse retention: [`Retention::Build`] materializes the tree,
    /// [`Retention::Measure`] records the count skeleton.
    retention: Retention,
    /// Whether the poll-based drive finished (re-poll is a contract error).
    done: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Drive {
    Continue,
    Complete,
}

const XML: Namespace = Namespace::new("xml");

fn syntax_impl(message: &'static str) -> CodecError {
    let base = CodecError::new(CodecFailureKind::InvalidInput);
    let Some(diagnostic) = jqf_source::Diagnostic::try_new(XML.code("syntax"), Severity::Error, message) else {
        return base;
    };
    base.with_diagnostic(diagnostic)
}

impl XmlParseState {
    pub(crate) fn try_new(bytes: &[u8]) -> Result<Self, CodecError> {
        if bytes.len() > u32::MAX as usize {
            return Err(CodecError::new(CodecFailureKind::Overflow));
        }
        // One leading UTF-8 BOM is signature metadata, not content: XML
        // admits it before the declaration, and the prolog grammar would
        // otherwise reject the 0xEF lead byte as non-whitespace. Skip exactly
        // one; authored spans stay absolute because every poll re-reads this
        // same full buffer.
        let body = bytes.strip_prefix(UTF8_BOM).unwrap_or(bytes);
        if core::str::from_utf8(body).is_err() {
            // The whole source is validated up front so every subsequent
            // character access is on valid UTF-8 (never a replacement from a
            // mid-multibyte byte).
            return Err(CodecError::new(CodecFailureKind::InvalidInput));
        }
        let mut state = Self::new_prevalidated();
        state.position = bytes.len() - body.len();
        Ok(state)
    }

    /// Constructs a parse over a buffer the caller has already proven is
    /// UTF-8 (a `&str`, or a span the measure parse already validated).
    pub(crate) fn try_new_prevalidated(text: &str) -> Result<Self, CodecError> {
        if text.len() > u32::MAX as usize {
            return Err(CodecError::new(CodecFailureKind::Overflow));
        }
        Ok(Self::new_prevalidated())
    }

    fn new_prevalidated() -> Self {
        let mut intern = NameInterner::new();
        let xml_prefix = intern.intern("xml");
        let xml_ns = intern.intern(crate::XML_NAMESPACE);
        Self {
            position: 0,
            in_content: false,
            frames: Vec::new(),
            namespace_bindings: vec![(xml_prefix, xml_ns)],
            tree: Tree::default(),
            intern,
            record_spans: true,
            entities: BTreeMap::new(),
            entity_replacement_bytes: 0,
            prolog_comments: Vec::new(),
            document_element_seen: false,
            retention: Retention::Build,
            done: false,
        }
    }

    /// Drop authored span vectors: the edit lane does not consume this parse.
    pub(crate) fn without_spans(mut self) -> Self {
        self.record_spans = false;
        self
    }

    /// Constructs a validate-only parse that records the document element's
    /// direct children instead of building the tree (the count skeleton).
    pub(crate) fn try_new_measure(bytes: &[u8]) -> Result<Self, CodecError> {
        let mut state = Self::try_new(bytes)?;
        state.retention = Retention::Measure(alloc::vec::Vec::new());
        Ok(state)
    }

    /// Validate-everything locate parse: records the document element's
    /// direct children and applies `steps` without building the tree.
    pub(crate) fn try_new_locate(
        bytes: &[u8],
        steps: alloc::vec::Vec<crate::locate::OwnedStep>,
    ) -> Result<Self, CodecError> {
        Self::locate_state(bytes, steps, true)
    }

    /// Locate over a child element's source span: own-name misses are nested,
    /// not the document projection seam.
    pub(crate) fn try_new_locate_nested(
        bytes: &[u8],
        steps: alloc::vec::Vec<crate::locate::OwnedStep>,
    ) -> Result<Self, CodecError> {
        Self::locate_state(bytes, steps, false)
    }

    fn locate_state(
        bytes: &[u8],
        steps: alloc::vec::Vec<crate::locate::OwnedStep>,
        is_document_root: bool,
    ) -> Result<Self, CodecError> {
        let mut state = Self::try_new(bytes)?.without_spans();
        state.retention = Retention::Locate(LocateRetention {
            steps,
            children: alloc::vec::Vec::new(),
            root_name: None,
            root_attrs: alloc::vec::Vec::new(),
            root_start: 0,
            root_end: 0,
            last_was_text: false,
            is_document_root,
        });
        Ok(state)
    }

    /// Parses the entire source synchronously (test paths; the sessions poll
    /// cooperatively). The governed element ceiling is not consulted here —
    /// a nesting-limit test drives [`Self::poll`] with bounded resources.
    #[cfg(test)]
    pub(crate) fn parse(mut self, bytes: &[u8]) -> Result<ParseOutput, CodecError> {
        while self.step(bytes, u32::MAX)? == Drive::Continue {}
        self.finalize()
    }

    /// Drives the parse cooperatively: each poll charges the work meter and
    /// returns `Pending` when the entry's credits are spent, so a large
    /// document does not monopolize one session poll. The caller replenishes
    /// credits between polls (the SDK does this between session polls). The
    /// bytes are re-read from the caller's resolved source each poll — the
    /// same authority, never a copy.
    pub(crate) fn poll(&mut self, bytes: &[u8], resources: &mut ResourceContext<'_>) -> Result<ParsePoll, CodecError> {
        if self.done {
            return Err(data_contract("XML parse polled after completion"));
        }
        // The element ceiling is the request's governed nesting limit, read
        // once per poll. Refusing at DECODE time (not at the tree build)
        // is what keeps every retention — build, measure, locate — from
        // accepting a depth no later recursion can process.
        let nesting_limit = resources.limits().max_nesting_depth();
        let mut steps = 0usize;
        loop {
            steps += 1;
            if steps > 1_000_000 {
                return Err(data_contract("XML parser made no progress"));
            }
            if resources.admit_work_transition()? == jqf_resource::WorkAdmission::Pending {
                return Ok(ParsePoll::Pending);
            }
            if self.step(bytes, nesting_limit)? == Drive::Complete {
                break;
            }
        }
        self.done = true;
        Ok(ParsePoll::Ready(self.finalize()?))
    }

    /// The end-of-input well-formedness checks and the parse output.
    fn finalize(&mut self) -> Result<ParseOutput, CodecError> {
        if !self.frames.is_empty() {
            return Err(syntax_impl("unexpected end of input: unclosed element"));
        }
        match &mut self.retention {
            Retention::Measure(children) => {
                if !self.document_element_seen {
                    return Err(syntax_impl("document has no element"));
                }
                Ok(ParseOutput::Measure(core::mem::take(children)))
            }
            Retention::Build => {
                if self.tree.elements.is_empty() {
                    return Err(syntax_impl("document has no element"));
                }
                self.tree.intern = core::mem::take(&mut self.intern);
                Ok(ParseOutput::Tree(core::mem::take(&mut self.tree)))
            }
            Retention::Locate(locate) => {
                if !self.document_element_seen {
                    return Err(syntax_impl("document has no element"));
                }
                let Some(root_name) = locate.root_name else {
                    return Err(syntax_impl("document has no element"));
                };
                if locate.steps.is_empty() {
                    return Ok(ParseOutput::Located(crate::locate::LocatedHit::Element {
                        start: locate.root_start,
                        end: locate.root_end,
                    }));
                }
                let hit = crate::locate::apply_steps(
                    &self.intern,
                    root_name,
                    locate.root_attrs.as_slice(),
                    locate.is_document_root,
                    locate.children.as_slice(),
                    locate.steps.as_slice(),
                    0,
                );
                Ok(ParseOutput::Located(hit))
            }
        }
    }

    fn skips_tree(&self) -> bool {
        !matches!(self.retention, Retention::Build)
    }

    fn record_locate_child(&mut self, child: crate::locate::LocateChild) {
        let Retention::Locate(locate) = &mut self.retention else {
            return;
        };
        if self.frames.len() != 1 {
            return;
        }
        if let crate::locate::LocateChild::Text(text) = &child
            && locate.last_was_text
            && let Some(crate::locate::LocateChild::Text(existing)) = locate.children.last_mut()
        {
            existing.push_str(text);
            return;
        }
        locate.last_was_text = matches!(child, crate::locate::LocateChild::Text(_));
        locate.children.push(child);
    }

    /// Records one direct child of the document element in measure retention.
    fn record_measure_child(&mut self, child: MeasureChild) {
        let Retention::Measure(children) = &mut self.retention else {
            return;
        };
        // A text run continuing the previous text child coalesces into it,
        // exactly as the tree build's distinct-child counter coalesces.
        if let MeasureChild::Text(text) = &child {
            let coalesce = children.last_mut().is_some_and(|last| {
                matches!(last, MeasureChild::Text(_)) && self.frames.last().is_some_and(|frame| frame.last_was_text)
            });
            if coalesce {
                if let Some(MeasureChild::Text(existing)) = children.last_mut() {
                    existing.push_str(text);
                }
                return;
            }
        }
        children.push(child);
    }

    /// The position at which the element child currently opening started, in
    /// measure retention; `None` outside a root child.
    fn measure_child_start(&self) -> Option<usize> {
        self.frames.len().eq(&1).then_some(self.position)
    }

    fn step(&mut self, bytes: &[u8], nesting_limit: u32) -> Result<Drive, CodecError> {
        // Whitespace is significant INSIDE an element: XML element content
        // preserves it, so a text run at the start or end of an element's
        // content (before/after the first/last child) is character data, not
        // trivia. Only whitespace OUTSIDE all open elements — the prolog and
        // the "Misc" after the document element — is skipped.
        if self.frames.is_empty() {
            // Prolog and epilog whitespace is not element content — it is
            // skipped so the Misc* after the document element stays clean.
            self.skip_whitespace(bytes);
        }
        if self.position >= bytes.len() {
            return Ok(Drive::Complete);
        }
        if !self.in_content {
            return self.prolog_step(bytes, nesting_limit);
        }
        if self.frames.is_empty() {
            // The document element has closed: XML 1.0's `prolog element
            // Misc*` permits comments and processing instructions after it.
            // Comments are preserved (appended to the root's content, exactly
            // as prolog comments are prepended); PIs are validated and
            // dropped, matching the prolog's own PI handling. Anything else —
            // a second root, a doctype, an end tag, character data — is an
            // error.
            if bytes[self.position..].starts_with(b"<!--") {
                self.position += 4;
                let text = self.read_comment(bytes)?;
                return self.epilog_comment(text);
            }
            if bytes[self.position..].starts_with(b"<?") {
                let (target, _data) = self.read_pi(bytes)?;
                if target.eq_ignore_ascii_case("xml") {
                    return Err(syntax_impl("reserved processing-instruction target"));
                }
                return Ok(Drive::Continue);
            }
            return Err(syntax_impl("content after the document element"));
        }
        if bytes[self.position] == b'<' {
            self.markup_step(bytes, nesting_limit)?;
        } else {
            self.read_text_into_current(bytes)?;
        }
        Ok(Drive::Continue)
    }

    /// Appends an epilog comment to the document root's content (a comment
    /// after the root is a child of the root, exactly like a prolog comment
    /// prepended to it).
    fn epilog_comment(&mut self, text: String) -> Result<Drive, CodecError> {
        if self.skips_tree() {
            self.record_measure_child(MeasureChild::Comment(text.clone()));
            self.record_locate_child(crate::locate::LocateChild::Comment(text));
            return Ok(Drive::Continue);
        }
        let index = self.tree.root;
        self.tree.elements[index].content.push(ContentEvent::Comment(text));
        Ok(Drive::Continue)
    }

    fn prolog_step(&mut self, bytes: &[u8], nesting_limit: u32) -> Result<Drive, CodecError> {
        // A declaration is `<?xml` followed by whitespace or `?`. The
        // ubiquitous `<?xml-stylesheet …?>` PI starts with the same five
        // bytes and must take the ordinary PI arm, never the declaration.
        if is_xml_declaration_at(bytes, self.position) {
            // "Start of the document" means after at most the BOM, whose
            // full length try_new already advanced past.
            let start = if bytes.starts_with(UTF8_BOM) { UTF8_BOM.len() } else { 0 };
            if self.position != start {
                return Err(syntax_impl("XML declaration must be at the start of the document"));
            }
            self.read_xml_decl(bytes)?;
        } else if bytes[self.position..].starts_with(b"<!--") {
            self.position += 4;
            let text = self.read_comment(bytes)?;
            self.prolog_comments.push(text);
        } else if bytes[self.position..].starts_with(b"<?") {
            // A PI other than the declaration must not be named `xml`.
            let (target, _data) = self.read_pi(bytes)?;
            if target.eq_ignore_ascii_case("xml") {
                return Err(syntax_impl("reserved processing-instruction target"));
            }
        } else if bytes[self.position..].starts_with(b"<!DOCTYPE") {
            self.read_doctype(bytes)?;
        } else if bytes[self.position] == b'<' {
            // The document element.
            self.in_content = true;
            self.markup_step(bytes, nesting_limit)?;
            self.attach_prolog_comments()?;
        } else {
            return Err(syntax_impl("unexpected non-whitespace before the document element"));
        }
        Ok(Drive::Continue)
    }

    fn attach_prolog_comments(&mut self) -> Result<(), CodecError> {
        if self.skips_tree() {
            // The prolog comments are the root's FIRST children; prepend them
            // in order.
            let prolog = core::mem::take(&mut self.prolog_comments);
            if let Retention::Measure(children) = &mut self.retention {
                let mut head = alloc::vec::Vec::new();
                for comment in &prolog {
                    head.push(MeasureChild::Comment(comment.clone()));
                }
                head.append(children);
                *children = head;
            }
            if let Retention::Locate(locate) = &mut self.retention {
                let mut head = alloc::vec::Vec::new();
                for comment in prolog {
                    head.push(crate::locate::LocateChild::Comment(comment));
                }
                head.append(&mut locate.children);
                locate.children = head;
            }
            return Ok(());
        }
        if let Some(root) = self.tree.elements.first_mut() {
            for comment in self.prolog_comments.drain(..).rev() {
                root.content.insert(0, ContentEvent::Comment(comment));
                if self.record_spans {
                    root.content_spans.insert(0, None);
                }
            }
        }
        Ok(())
    }

    fn skip_whitespace(&mut self, bytes: &[u8]) {
        self.position += ws_prefix_len(&bytes[self.position..]);
    }

    fn current_char(&self, bytes: &[u8]) -> char {
        // `try_new` pre-validated the whole source as UTF-8, so the bytes from
        // `position` (always a char boundary) form a valid character. Decode
        // ONLY that character via its lead byte — never re-validate the
        // remaining buffer, which would make a whole-document scan O(n²).
        let byte = bytes[self.position];
        let len = match byte {
            0x00..=0x7f => 1,
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf7 => 4,
            // Unreachable on a pre-validated buffer; the U+FFFD fallback keeps
            // the function total if a byte ever slips through.
            _ => return '\u{FFFD}',
        };
        let end = self.position.saturating_add(len).min(bytes.len());
        core::str::from_utf8(&bytes[self.position..end])
            .ok()
            .and_then(|text| text.chars().next())
            .unwrap_or('\u{FFFD}')
    }

    fn read_until(&mut self, bytes: &[u8], terminator: &[u8]) -> Result<String, CodecError> {
        let start = self.position;
        let window =
            find_terminator(&bytes[start..], terminator).ok_or_else(|| syntax_impl("expected a closing delimiter"))?;
        let text = Self::decode_range(bytes, start, start + window)?;
        self.position = start + window + terminator.len();
        Ok(text)
    }

    fn read_comment(&mut self, bytes: &[u8]) -> Result<String, CodecError> {
        // One scan for both the `-->` terminator and the `--` well-formedness
        // rejection: a `--` that is not the start of `-->` is illegal, and a
        // body that would end with `-` is the same `--` not followed by `>`.
        let start = self.position;
        let haystack = &bytes[start..];
        let mut i = 0;
        while i < haystack.len() {
            let skip = prefix_len::<StopDash>(&haystack[i..]);
            let pos = i + skip;
            if pos >= haystack.len() {
                break;
            }
            if haystack.get(pos + 1) == Some(&b'-') {
                if haystack.get(pos + 2) == Some(&b'>') {
                    // A comment body is `Char`-restricted too.
                    Self::check_char_data(&haystack[..pos])?;
                    let text = Self::decode_range(bytes, start, start + pos)?;
                    self.position = start + pos + 3;
                    return Ok(text);
                }
                return Err(syntax_impl("comment text must not contain '--'"));
            }
            i = pos + 1;
        }
        Err(syntax_impl("expected a closing delimiter"))
    }

    fn read_xml_decl(&mut self, bytes: &[u8]) -> Result<(), CodecError> {
        self.position += 5; // past `<?xml`
        self.skip_whitespace(bytes);
        let inner = self.read_until(bytes, b"?>")?;
        // XML 1.0: version, then optional encoding, then optional
        // standalone — each at most once. `version` is the production
        // `1.` + one or more digits; this processor accepts only `1.0`.
        let mut stage = DeclStage::Version;
        let mut saw_version = false;
        for pair in split_decl_attributes(&inner)? {
            let (key, value) = pair;
            match key.as_str() {
                "version" => {
                    if saw_version || stage != DeclStage::Version {
                        return Err(syntax_impl(
                            "XML declaration attributes must appear in order version, encoding, standalone",
                        ));
                    }
                    if !is_xml10_version(&value) {
                        if value.starts_with("1.")
                            && value.len() > 2
                            && value.as_bytes()[2..].iter().all(u8::is_ascii_digit)
                        {
                            return Err(syntax_impl("unsupported XML version"));
                        }
                        return Err(syntax_impl("invalid XML version"));
                    }
                    saw_version = true;
                    stage = DeclStage::Encoding;
                }
                "encoding" => {
                    if stage != DeclStage::Encoding {
                        return Err(syntax_impl(
                            "XML declaration attributes must appear in order version, encoding, standalone",
                        ));
                    }
                    let normalized = value.to_ascii_lowercase();
                    if normalized != "utf-8" && normalized != "utf8" {
                        return Err(syntax_impl("unsupported encoding"));
                    }
                    stage = DeclStage::Standalone;
                }
                "standalone" => {
                    if stage != DeclStage::Encoding && stage != DeclStage::Standalone {
                        return Err(syntax_impl(
                            "XML declaration attributes must appear in order version, encoding, standalone",
                        ));
                    }
                    if value != "yes" && value != "no" {
                        return Err(syntax_impl("invalid standalone declaration"));
                    }
                    stage = DeclStage::Done;
                }
                _ => return Err(syntax_impl("unknown XML declaration attribute")),
            }
        }
        if !saw_version {
            return Err(syntax_impl("XML declaration is missing version"));
        }
        Ok(())
    }

    fn read_pi(&mut self, bytes: &[u8]) -> Result<(String, String), CodecError> {
        // At `<?`; advance past it.
        self.position += 2;
        let data_start = self.position;
        let inner = self.read_until(bytes, b"?>")?;
        // PI data is `Char*`, so the invalid-character scan applies here too.
        Self::check_char_data(&bytes[data_start..self.position - 2])?;
        let mut split = inner.splitn(2, char::is_whitespace);
        let target = split.next().unwrap_or("").to_string();
        let data = split.next().unwrap_or("").to_string();
        // XML 1.0 §2.6: the target is a `Name`, and Namespaces in XML
        // forbids a colon in it — so `<`, quotes, a leading digit, or any
        // other non-name text before the first whitespace is rejected, not
        // silently taken as the target.
        let mut chars = target.chars();
        let valid_name =
            chars.next().is_some_and(|c| is_name_start(c) && c != ':') && chars.all(|c| is_name_char(c) && c != ':');
        if !valid_name {
            return Err(syntax_impl("invalid processing-instruction target"));
        }
        Ok((target, data))
    }

    fn read_doctype(&mut self, bytes: &[u8]) -> Result<(), CodecError> {
        self.tree.had_doctype = true;
        self.position += 9; // past `<!DOCTYPE`
        self.skip_whitespace(bytes);
        let _name = self.read_name(bytes)?;
        self.skip_whitespace(bytes);
        if bytes[self.position..].starts_with(b"SYSTEM") || bytes[self.position..].starts_with(b"PUBLIC") {
            // The external subset is skipped: no I/O ever happens, and no
            // defaulting/declaration is demanded by this non-validating
            // processor's semantic projection. The walk is token-at-a-time
            // (quoted literals honored) and falls through rather than
            // returning, because an internal subset may FOLLOW the external
            // id (`<!DOCTYPE d SYSTEM "u" […]>`).
            loop {
                self.skip_whitespace(bytes);
                match bytes.get(self.position) {
                    Some(b'>' | b'[') => break,
                    Some(q @ (b'"' | b'\'')) => {
                        self.position += 1;
                        let _ = self.read_until(bytes, &[*q])?;
                    }
                    Some(_) => self.position += 1,
                    None => return Err(syntax_impl("unterminated DOCTYPE")),
                }
            }
        }
        if self.position < bytes.len() && bytes[self.position] == b'[' {
            self.position += 1;
            let subset = self.read_dtd_subset(bytes)?;
            self.parse_internal_subset(&subset)?;
            self.skip_whitespace(bytes);
        }
        if self.position >= bytes.len() || bytes[self.position] != b'>' {
            return Err(syntax_impl("unterminated DOCTYPE"));
        }
        self.position += 1;
        Ok(())
    }

    fn read_dtd_subset(&mut self, bytes: &[u8]) -> Result<String, CodecError> {
        let mut depth = 1usize;
        // A `[`/`]` inside a quoted declaration value (an entity value, an
        // ATTLIST default) is not a subset bracket: the bracket law applies
        // only to unquoted text. A quote toggles only outside other quotes,
        // so `<!ENTITY e "[">` contributes no depth.
        let mut quote: Option<char> = None;
        let start = self.position;
        while self.position < bytes.len() && depth > 0 {
            // Quote state gates the comment arm: inside a quoted declaration
            // value, `<!--` is literal text (e.g. an entity whose value is a
            // comment), never a comment to skip past.
            if quote.is_none() && bytes[self.position..].starts_with(b"<!--") {
                let _ = self.read_until(bytes, b"-->")?;
                continue;
            }
            let c = self.current_char(bytes);
            match quote {
                Some(q) if q == c => quote = None,
                Some(_) => {}
                None => match c {
                    '"' | '\'' => quote = Some(c),
                    '[' => depth += 1,
                    ']' => depth -= 1,
                    _ => {}
                },
            }
            self.position += c.len_utf8();
        }
        if depth != 0 {
            return Err(syntax_impl("unterminated internal subset"));
        }
        let end = self.position - 1; // position lands past the ']'
        Self::decode_range(bytes, start, end)
    }

    fn parse_internal_subset(&mut self, text: &str) -> Result<(), CodecError> {
        // Walk declarations; only general `ENTITY name "value"` is recorded
        // as replacement text. Parameter entities and external declarations
        // are refused (the secure processor performs no substitution).
        let mut rest = text;
        while rest.trim() != "" {
            let trimmed = rest.trim_start();
            if !trimmed.starts_with("<!ENTITY") {
                // Skip any other declaration to its closing `>`.
                let Some(close) = closing_gt(trimmed) else {
                    return Ok(());
                };
                rest = &trimmed[close + 1..];
                continue;
            }
            let body = trimmed["<!ENTITY".len()..].trim_start();
            let (name, value) = extract_entity(body)?;
            if let Some(value) = value {
                if self.entities.len() >= MAX_ENTITY_COUNT {
                    return Err(CodecError::new(CodecFailureKind::InvalidInput));
                }
                if value.len() > MAX_ENTITY_REPLACEMENT_BYTES {
                    return Err(CodecError::new(CodecFailureKind::InvalidInput));
                }
                if !name.is_empty() && !name.starts_with('%') {
                    self.entities.insert(name, value);
                }
            }
            let Some(close) = closing_gt(body) else {
                return Ok(());
            };
            rest = &body[close + 1..];
        }
        Ok(())
    }

    fn read_name(&mut self, bytes: &[u8]) -> Result<String, CodecError> {
        let start = self.position;
        if start >= bytes.len() {
            return Err(syntax_impl("expected a name"));
        }
        // ASCII fast path: one byte table walk then a single slice copy.
        // A byte >= 0x80 falls through to the char loop.
        if bytes[start] < 0x80 {
            if !is_ascii_name_start(bytes[start]) {
                return Err(syntax_impl("expected a name"));
            }
            let mut end = start + 1;
            while end < bytes.len() && bytes[end] < 0x80 && is_ascii_name_char(bytes[end]) {
                end += 1;
            }
            if end == bytes.len() || bytes[end] < 0x80 {
                self.position = end;
                // SAFETY: the range is a proven-UTF-8 ASCII slice.
                return Ok(unsafe { core::str::from_utf8_unchecked(&bytes[start..end]) }.to_owned());
            }
            self.position = start;
        }
        let mut name = String::new();
        let mut first = true;
        while self.position < bytes.len() {
            let c = self.current_char(bytes);
            let ok = if first { is_name_start(c) } else { is_name_char(c) };
            if !ok {
                break;
            }
            name.push(c);
            self.position += c.len_utf8();
            first = false;
        }
        if name.is_empty() {
            return Err(syntax_impl("expected a name"));
        }
        Ok(name)
    }

    fn markup_step(&mut self, bytes: &[u8], nesting_limit: u32) -> Result<(), CodecError> {
        // At '<'.
        if self.position + 1 >= bytes.len() {
            return Err(syntax_impl("unexpected end of input"));
        }
        match bytes[self.position + 1] {
            b'!' => {
                if bytes[self.position + 2..].starts_with(b"--") {
                    // The comment's authored extent: from this `<` through
                    // the `-->` the read consumes (the comment-write seam
                    // replaces comment children by their spans, so a comment
                    // child must carry one).
                    let comment_start = self.position;
                    self.position += 4;
                    let text = self.read_comment(bytes)?;
                    self.append_content(ContentEvent::Comment(text), Some((comment_start, self.position)))?;
                } else if bytes[self.position + 2..].starts_with(b"[CDATA[") {
                    self.position += 9;
                    let cdata_start = self.position;
                    let text = self.read_until(bytes, b"]]>")?;
                    // A CDATA body is `Char`-restricted like any other
                    // character data; `read_until` does not scan for it.
                    Self::check_char_data(&bytes[cdata_start..self.position - 3])?;
                    self.append_text(normalize_line_endings(&text).as_ref(), cdata_start, self.position)?;
                } else {
                    return Err(syntax_impl("unsupported markup declaration in content"));
                }
            }
            b'?' => {
                let (target, data) = self.read_pi(bytes)?;
                if target.eq_ignore_ascii_case("xml") {
                    return Err(syntax_impl("reserved processing-instruction target"));
                }
                self.append_content(ContentEvent::ProcessingInstruction(Box::new((target, data))), None)?;
            }
            b'/' => self.end_tag(bytes)?,
            _ => {
                // In measure retention, a root child's extent starts at this
                // `<`; `open_element` records it via `measure_child_start`.
                let child_start = self.measure_child_start();
                // The authored element extent starts at this `<` (the edit
                // lane's structural splice reads it back through the
                // element's span).
                let element_start = self.position;
                let start = self.start_tag(bytes)?;
                let self_closing = start.self_closing;
                self.open_element(start, element_start, nesting_limit)?;
                if let (Some(start), Retention::Measure(children)) = (child_start, &mut self.retention) {
                    children.push(MeasureChild::Element { start, end: 0 });
                }
                if self_closing {
                    self.pop_frame()?;
                }
            }
        }
        Ok(())
    }

    fn end_tag(&mut self, bytes: &[u8]) -> Result<(), CodecError> {
        self.position += 2; // past `</`
        let name = self.read_name(bytes)?;
        self.skip_whitespace(bytes);
        if self.position >= bytes.len() || bytes[self.position] != b'>' {
            return Err(syntax_impl("malformed end tag"));
        }
        self.position += 1;
        let (prefix, local) = self.split_qname(&name)?;
        let uri = if prefix != NameInterner::EMPTY {
            self.resolve_prefix(prefix)
                .ok_or_else(|| syntax_impl("undeclared prefix in end tag"))?
        } else {
            self.default_namespace().unwrap_or(NameInterner::EMPTY)
        };
        let expanded = ExpandedName { uri, local };
        self.close_last_element_for(expanded)
    }

    fn start_tag(&mut self, bytes: &[u8]) -> Result<StartTag, CodecError> {
        self.position += 1; // past `<`
        let name = self.read_name(bytes)?;
        let (prefix, local) = self.split_qname(&name)?;
        let mut attributes = Vec::new();
        let mut self_closing = false;
        loop {
            // XML requires S between attributes: record the pre-skip
            // position so a SECOND attribute must prove whitespace before it.
            // The `>`/`/>` closers are exempt — they end the tag.
            let before_whitespace = self.position;
            self.skip_whitespace(bytes);
            let saw_separator = self.position != before_whitespace;
            if self.position >= bytes.len() {
                return Err(syntax_impl("unterminated start tag"));
            }
            let c = bytes[self.position];
            if c == b'>' {
                self.position += 1;
                break;
            }
            if c == b'/' {
                if self.position + 1 < bytes.len() && bytes[self.position + 1] == b'>' {
                    self.position += 2;
                    self_closing = true;
                    break;
                }
                return Err(syntax_impl("malformed empty element tag"));
            }
            if !attributes.is_empty() && !saw_separator {
                return Err(syntax_impl("whitespace is required between attributes"));
            }
            attributes.push(self.read_attribute(bytes)?);
        }
        Ok(StartTag {
            prefix,
            local,
            attributes,
            self_closing,
        })
    }

    fn read_attribute(&mut self, bytes: &[u8]) -> Result<RawAttribute, CodecError> {
        let raw_name = self.read_name(bytes)?;
        let (prefix, local) = self.split_qname(&raw_name)?;
        self.skip_whitespace(bytes);
        if self.position >= bytes.len() || bytes[self.position] != b'=' {
            return Err(syntax_impl("expected '=' after attribute name"));
        }
        self.position += 1;
        self.skip_whitespace(bytes);
        if self.position >= bytes.len() {
            return Err(syntax_impl("unterminated attribute value"));
        }
        let quote = bytes[self.position];
        if quote != b'"' && quote != b'\'' {
            return Err(syntax_impl("attribute value must be quoted"));
        }
        // The authored span starts at the opening quote.
        let value_start = self.position;
        self.position += 1;
        let mut value = String::new();
        loop {
            // Wide-scan the next run containing no quote, '<', or '&'. The
            // run is validated and pushed as one chunk; the per-char
            // machinery runs only at the boundary byte, where an entity
            // reference or a markup error can actually start.
            let clean = prefix_len::<XmlAttrValue>(&bytes[self.position..]);
            // §3.3.3 normalization applies to LITERAL text only, so it runs
            // per literal chunk here — never across the entity arm below,
            // where character-referenced whitespace (`&#xA;`) must survive.
            let mut literal = String::new();
            self.push_char_data(bytes, &mut literal, clean)?;
            value.push_str(&normalize_attribute_value(&literal));
            self.position += clean;
            if self.position >= bytes.len() {
                return Err(syntax_impl("unterminated attribute value"));
            }
            match bytes[self.position] {
                b'<' => {
                    return Err(syntax_impl("'<' is not allowed in an attribute value"));
                }
                b'&' => {
                    let (replacement, general_entity) = self.read_character_or_entity(bytes, true)?;
                    if general_entity {
                        self.account_entity(&replacement)?;
                    }
                    value.push_str(&replacement);
                }
                q if q == quote => {
                    self.position += 1;
                    break;
                }
                // The other quote type is an ordinary character of this
                // value; the scan stopped at it only because it shares the
                // attribute-value stop set.
                _ => self.position += 1,
            }
        }
        Ok(RawAttribute {
            prefix,
            local,
            value,
            // The authored span INCLUDES the quotes: `position` is one past
            // the closing quote.
            span: (value_start, self.position),
        })
    }

    fn account_entity(&mut self, replacement: &str) -> Result<(), CodecError> {
        self.entity_replacement_bytes += replacement.len();
        if self.entity_replacement_bytes > MAX_ENTITY_REPLACEMENT_BYTES {
            return Err(CodecError::new(CodecFailureKind::InvalidInput));
        }
        Ok(())
    }

    fn read_text_into_current(&mut self, bytes: &[u8]) -> Result<(), CodecError> {
        // The authored run span starts at the first byte of the run (past the
        // previous `>` / `]]>` / entity boundary) and ends one past its last
        // byte. Interior entity references are part of the run's bytes.
        let run_start = self.position;
        let mut text = String::new();
        loop {
            // Wide-scan the next clean run: no '<' and no '&', so it contains
            // no markup and no entity reference. Its characters are validated
            // and pushed as one chunk; the per-char entity machinery runs only
            // at the boundary byte, where a reference can actually start.
            let clean = prefix_len::<Xml>(&bytes[self.position..]);
            if contains_cdata_end(&bytes[self.position..self.position + clean]) {
                return Err(syntax_impl("sequence ']]>' not allowed in content"));
            }
            // §2.11 EOL normalization applies to LITERAL text only, so it
            // runs per literal chunk here — never across the entity arm below,
            // where a character-referenced CR (`&#xD;`) must survive.
            let mut literal = String::new();
            self.push_char_data(bytes, &mut literal, clean)?;
            text.push_str(normalize_line_endings(&literal).as_ref());
            self.position += clean;
            if self.position >= bytes.len() {
                break;
            }
            match bytes[self.position] {
                b'<' => break,
                b'&' => {
                    let (replacement, general_entity) = self.read_character_or_entity(bytes, false)?;
                    if general_entity {
                        self.account_entity(&replacement)?;
                    }
                    text.push_str(&replacement);
                }
                _ => unreachable!("prefix_len<Xml> stops only at '<' or '&'"),
            }
        }
        self.append_text(&text, run_start, self.position)
    }

    /// Validates the character data in `bytes[position..position + n]` and
    /// appends it to `out` as one chunk. The caller has proven the range
    /// contains no terminator (no `<`, no `&`, and for an attribute value no
    /// quote); the whole source was pre-validated as UTF-8 in `try_new`,
    /// and the range ends at a terminator byte or the source end — both
    /// ASCII — so it is valid UTF-8 whose char boundaries align with the
    /// range ends.
    ///
    /// The characters XML forbids inside such a range are exactly the C0
    /// controls other than tab/LF/CR and U+FFFE/U+FFFF; the
    /// [`XmlCharInvalid`] scan finds the bytes that can begin one, and the
    /// 3-byte non-character pattern is verified here at the stop byte.
    fn push_char_data(&mut self, bytes: &[u8], out: &mut String, n: usize) -> Result<(), CodecError> {
        Self::check_char_data(&bytes[self.position..self.position + n])?;
        // SAFETY: `try_new` validated the whole source as UTF-8, and
        // both range ends are char boundaries (`position` advances by whole
        // characters or entity references; the range end is a terminator or
        // the source end, both ASCII).
        out.push_str(unsafe { core::str::from_utf8_unchecked(&bytes[self.position..self.position + n]) });
        Ok(())
    }

    /// The [`XmlCharInvalid`] well-formedness scan shared by every reader
    /// that collects character data (the XML 1.0 `Char` production): the
    /// characters forbidden in such a range are exactly the C0 controls
    /// other than tab/LF/CR and U+FFFE/U+FFFF, the scan finds the bytes that
    /// can begin one, and the 3-byte non-character pattern is verified at
    /// the stop byte. CDATA bodies, comment bodies, and PI data reach it
    /// through their own readers rather than [`Self::push_char_data`].
    fn check_char_data(range: &[u8]) -> Result<(), CodecError> {
        let mut pos = 0;
        while pos < range.len() {
            let clean = prefix_len::<XmlCharInvalid>(&range[pos..]);
            pos += clean;
            if pos >= range.len() {
                break;
            }
            match range[pos] {
                // Tab, LF, CR: the valid C0 controls the scan re-admits.
                0x09 | 0x0A | 0x0D => pos += 1,
                // U+FFFE / U+FFFF (EF BF BE / EF BF BF), the only
                // non-characters the XML 1.0 Char production rejects.
                0xEF => {
                    if range.get(pos + 1..pos + 3) == Some(&[0xBF, 0xBE])
                        || range.get(pos + 1..pos + 3) == Some(&[0xBF, 0xBF])
                    {
                        return Err(syntax_impl("invalid character in document"));
                    }
                    pos += 3;
                }
                // Any other C0 control is XML-invalid.
                _ => return Err(syntax_impl("invalid character in document")),
            }
        }
        Ok(())
    }

    /// Resolves one `&…;` reference in document content or an attribute
    /// value. Returns the replacement text and whether it came from a
    /// GENERAL entity: only a general expansion is accounted against the
    /// replacement-text cap, because a character or predefined reference
    /// consumes more source bytes than it emits and cannot amplify.
    fn read_character_or_entity(
        &mut self,
        bytes: &[u8],
        in_attribute_value: bool,
    ) -> Result<(String, bool), CodecError> {
        // At `&`.
        self.position += 1;
        if self.position >= bytes.len() {
            return Err(syntax_impl("unterminated entity reference"));
        }
        if bytes[self.position] == b'#' {
            self.position += 1;
            let hex = self.position < bytes.len() && bytes[self.position] == b'x';
            if hex {
                self.position += 1;
            }
            let digits_start = self.position;
            while self.position < bytes.len() {
                let c = bytes[self.position];
                let ok = if hex { c.is_ascii_hexdigit() } else { c.is_ascii_digit() };
                if !ok {
                    break;
                }
                self.position += 1;
            }
            let digits = Self::decode_range(bytes, digits_start, self.position)?;
            if digits.is_empty() {
                return Err(syntax_impl("empty character reference"));
            }
            if self.position >= bytes.len() || bytes[self.position] != b';' {
                return Err(syntax_impl("unterminated character reference"));
            }
            self.position += 1;
            let code = u32::from_str_radix(&digits, if hex { 16 } else { 10 })
                .map_err(|_| syntax_impl("invalid character reference"))?;
            let c = char::from_u32(code)
                .filter(|&c| is_char(c) && c != '\u{0}')
                .ok_or_else(|| syntax_impl("invalid character reference value"))?;
            let mut out = String::new();
            out.push(c);
            return Ok((out, false));
        }
        let name = self.read_name(bytes)?;
        if self.position >= bytes.len() || bytes[self.position] != b';' {
            return Err(syntax_impl("unterminated entity reference"));
        }
        self.position += 1;
        match name.as_str() {
            "amp" => Ok(("&".to_string(), false)),
            "lt" => Ok(("<".to_string(), false)),
            "gt" => Ok((">".to_string(), false)),
            "quot" => Ok(("\"".to_string(), false)),
            "apos" => Ok(("'".to_string(), false)),
            other => Ok((self.expand_general_entity(other, in_attribute_value)?, true)),
        }
    }

    /// Expands a general entity's replacement text on use, recursively
    /// resolving the character, predefined, and general references it
    /// contains. XML 1.0 requires a referenced entity's replacement text to
    /// be scanned for further references, and a direct or indirect
    /// self-reference is a well-formedness error, so the reference names
    /// currently in flight are tracked to reject a cycle — and their count
    /// bounds the expansion depth. The total emitted bytes are accounted by
    /// the caller through [`Self::account_entity`].
    fn expand_general_entity(&mut self, name: &str, in_attribute_value: bool) -> Result<String, CodecError> {
        let mut in_flight = vec![name.to_string()];
        self.expand_replacement_text(name, &mut in_flight, in_attribute_value)
    }

    fn expand_replacement_text(
        &mut self,
        name: &str,
        in_flight: &mut Vec<String>,
        in_attribute_value: bool,
    ) -> Result<String, CodecError> {
        let value = self
            .entities
            .get(name)
            .cloned()
            .ok_or_else(|| syntax_impl("undefined entity reference"))?;
        let mut out = String::new();
        let mut i = 0;
        while i < value.len() {
            if value.as_bytes()[i] == b'&' {
                let (consumed, replacement) = self.expand_one_reference(&value, i, in_flight, in_attribute_value)?;
                out.push_str(&replacement);
                i += consumed;
                continue;
            }
            let ch = value[i..].chars().next().expect("validated char");
            // XML 1.0 §3.3.3 [WFC: No < in Attribute Values]: the
            // replacement text of an entity used in an attribute value must
            // contain neither `<` nor `&` as literal text. A `<`/`&` that
            // arrives through a *named* reference (`&lt;`, `&amp;`, `&e;`)
            // is legal — only literal characters and character references
            // are checked (the `&` half is checked in
            // `expand_one_reference`, where the reference kind is known).
            if in_attribute_value && ch == '<' {
                return Err(syntax_impl(
                    "entity replacement text in an attribute value must not contain '<'",
                ));
            }
            out.push(ch);
            i += ch.len_utf8();
        }
        Ok(out)
    }

    /// Resolves the `&`-reference beginning at `offset` within `value`,
    /// returning the consumed bytes and the replacement text (character
    /// references and predefined entities inline; general entities
    /// recursively, guarded against a self-reference cycle).
    fn expand_one_reference(
        &mut self,
        value: &str,
        offset: usize,
        in_flight: &mut Vec<String>,
        in_attribute_value: bool,
    ) -> Result<(usize, String), CodecError> {
        let rest = &value[offset..];
        if let Some(after_hash) = rest.strip_prefix("&#") {
            let (hex, digits_text) = after_hash
                .strip_prefix('x')
                .map_or((false, after_hash), |hex_digits| (true, hex_digits));
            let mut digits_end = 0;
            while digits_end < digits_text.len() {
                let c = digits_text.as_bytes()[digits_end];
                let ok = if hex { c.is_ascii_hexdigit() } else { c.is_ascii_digit() };
                if !ok {
                    break;
                }
                digits_end += 1;
            }
            if digits_end == 0 || !digits_text[digits_end..].starts_with(';') {
                return Err(syntax_impl("invalid character reference in entity value"));
            }
            let digits = &digits_text[..digits_end];
            let code = u32::from_str_radix(digits, if hex { 16 } else { 10 })
                .map_err(|_| syntax_impl("invalid character reference in entity value"))?;
            let ch = char::from_u32(code)
                .filter(|&c| is_char(c) && c != '\u{0}')
                .ok_or_else(|| syntax_impl("invalid character reference in entity value"))?;
            // [WFC: No < in Attribute Values]: a character reference in an
            // entity's replacement text resolves to literal text for the
            // WFC, so `<` and `&` from a charref are forbidden in an
            // attribute value (named references stay legal).
            if in_attribute_value && (ch == '<' || ch == '&') {
                return Err(syntax_impl(
                    "entity replacement text in an attribute value must not contain '<' or '&'",
                ));
            }
            Ok((2 + digits_end + 1, ch.to_string()))
        } else {
            let name_end = rest[1..].find([';', '&', '<']).map_or(rest.len(), |index| index + 1);
            let entity_name = &rest[1..name_end];
            if entity_name.is_empty() || !rest[name_end..].starts_with(';') {
                return Err(syntax_impl("invalid entity reference in entity value"));
            }
            let consumed = name_end + 1;
            let resolved = match entity_name {
                "amp" => "&".to_string(),
                "lt" => "<".to_string(),
                "gt" => ">".to_string(),
                "quot" => "\"".to_string(),
                "apos" => "'".to_string(),
                other => {
                    if in_flight.iter().any(|s| s == other) {
                        return Err(syntax_impl("recursive entity reference"));
                    }
                    // The in-flight walk IS the expansion depth: one entry
                    // per entity whose replacement text is being scanned.
                    if in_flight.len() >= MAX_ENTITY_EXPANSION_DEPTH {
                        return Err(CodecError::new(CodecFailureKind::InvalidInput));
                    }
                    in_flight.push(other.to_string());
                    let expanded = self.expand_replacement_text(other, in_flight, in_attribute_value)?;
                    in_flight.pop();
                    return Ok((consumed, expanded));
                }
            };
            Ok((consumed, resolved))
        }
    }

    /// Splits one qname at its single colon. A colon in the LOCAL part
    /// (`a:b:c`) is not an `NCName`: XML Namespaces allow exactly one colon,
    /// so the name is a syntax error, never a two-part name with a colon
    /// silently folded into its local part.
    fn split_qname(&mut self, name: &str) -> Result<(NameId, NameId), CodecError> {
        if let Some((prefix, local)) = name.split_once(':') {
            if local.contains(':') {
                return Err(syntax_impl("a name may contain only one colon"));
            }
            Ok((self.intern.intern(prefix), self.intern.intern(local)))
        } else {
            Ok((NameInterner::EMPTY, self.intern.intern(name)))
        }
    }

    fn resolve_prefix(&self, prefix: NameId) -> Option<NameId> {
        self.namespace_bindings
            .iter()
            .rev()
            .find(|(p, _)| *p == prefix)
            .map(|(_, uri)| *uri)
    }

    fn default_namespace(&self) -> Option<NameId> {
        self.namespace_bindings
            .iter()
            .rev()
            .find(|(p, _)| *p == NameInterner::EMPTY)
            .map(|(_, uri)| *uri)
    }

    fn open_element(&mut self, start: StartTag, element_start: usize, nesting_limit: u32) -> Result<(), CodecError> {
        let depth = self.frames.len();
        if depth as u64 >= u64::from(nesting_limit) {
            return Err(CodecError::from(ResourceError::LimitExceeded {
                limit_kind: ResourceLimit::NestingDepth,
                limit: u64::from(nesting_limit),
                current: depth as u64,
                requested_delta: 1,
            }));
        }
        // The whole document is ONE XML document with exactly one element:
        // once the root element has closed, another start tag is a
        // well-formedness error, not a second root. The flag is used in both
        // retentions (measure retention builds no tree to read).
        if self.frames.is_empty() && self.document_element_seen {
            return Err(syntax_impl("a second element after the document element"));
        }
        // Resolve namespace declarations first; they can affect attribute and
        // element-name resolution.
        let xmlns = self.intern.intern("xmlns");
        let xml_prefix = self.intern.intern("xml");
        let mut bindings: Vec<(NameId, NameId)> = Vec::new();
        let mut ordinary: Vec<RawAttribute> = Vec::new();
        let mut seen_ns: Vec<NameId> = Vec::new();
        for attr in start.attributes {
            if attr.prefix == xmlns && attr.local != NameInterner::EMPTY {
                if attr.local == xmlns {
                    return Err(syntax_impl("'xmlns' is a reserved prefix"));
                }
                if attr.local == xml_prefix && attr.value != crate::XML_NAMESPACE {
                    return Err(syntax_impl("reserved 'xml' namespace must keep its URI"));
                }
                if attr.value.is_empty() {
                    return Err(syntax_impl("namespace declarations must not be empty"));
                }
                if seen_ns.contains(&attr.local) {
                    return Err(syntax_impl("duplicate namespace declaration"));
                }
                seen_ns.push(attr.local);
                let uri = self.intern.intern(&attr.value);
                bindings.push((attr.local, uri));
            } else if attr.prefix == NameInterner::EMPTY && attr.local == xmlns {
                if seen_ns.contains(&NameInterner::EMPTY) {
                    return Err(syntax_impl("duplicate namespace declaration"));
                }
                seen_ns.push(NameInterner::EMPTY);
                let uri = self.intern.intern(&attr.value);
                bindings.push((NameInterner::EMPTY, uri));
            } else {
                ordinary.push(attr);
            }
        }
        // Enter the namespace scope BEFORE resolving the element's own name
        // and attributes: per Namespaces in XML, the declarations on an
        // element apply to that element, its attributes, and its descendants.
        self.namespace_bindings.extend_from_slice(&bindings);
        let element_uri = if start.prefix != NameInterner::EMPTY {
            if start.prefix == xmlns {
                return Err(syntax_impl("'xmlns' is a reserved prefix"));
            }
            self.resolve_prefix(start.prefix)
                .ok_or_else(|| syntax_impl("undeclared element prefix"))?
        } else {
            self.default_namespace().unwrap_or(NameInterner::EMPTY)
        };
        let element_expanded = ExpandedName {
            uri: element_uri,
            local: start.local,
        };
        // Resolve attributes and enforce raw-attribute + expanded-name
        // uniqueness. The authored span stays aligned with the ORDINARY
        // attributes only — namespace declarations never reach `attributes`.
        let mut resolved: Vec<(ExpandedName, String)> = Vec::new();
        let mut resolved_spans: Vec<(usize, usize)> = Vec::new();
        let mut seen: Vec<ExpandedName> = Vec::new();
        for attr in ordinary {
            let attr_uri = if attr.prefix != NameInterner::EMPTY {
                self.resolve_prefix(attr.prefix)
                    .ok_or_else(|| syntax_impl("undeclared attribute prefix"))?
            } else {
                NameInterner::EMPTY
            };
            let attr_expanded = ExpandedName {
                uri: attr_uri,
                local: attr.local,
            };
            if seen.contains(&attr_expanded) {
                return Err(syntax_impl("duplicate expanded-name attribute"));
            }
            seen.push(attr_expanded);
            resolved.push((attr_expanded, attr.value));
            resolved_spans.push(attr.span);
        }
        if self.frames.is_empty() {
            self.document_element_seen = true;
        }
        // Record this element as a child of its parent BEFORE the frame is
        // pushed (the parent is still the current frame).
        self.account_child(false);
        // Measure/locate retention builds no tree: the frame below carries
        // the names the count skeleton and the scoped path need.
        if self.skips_tree() {
            if self.frames.is_empty()
                && let Retention::Locate(locate) = &mut self.retention
            {
                locate.root_name = Some(element_expanded);
                locate.root_attrs = resolved.iter().map(|(name, _)| *name).collect();
                locate.root_start = element_start;
            } else if self.frames.len() == 1
                && let Retention::Locate(locate) = &mut self.retention
            {
                locate.last_was_text = false;
                locate.children.push(crate::locate::LocateChild::Element {
                    name: element_expanded,
                    start: element_start,
                    end: 0,
                });
            }
            self.frames.push(OpenFrame {
                element_index: usize::MAX,
                bindings,
                name: element_expanded,
                last_was_text: false,
            });
            return Ok(());
        }
        let element_index = self.tree.elements.len();
        self.tree.elements.push(Element {
            name: element_expanded,
            attributes: resolved,
            content: Vec::new(),
            // The authored extent is finalized in `pop_frame` (end tag or
            // self-closing `/>` observed); `start` is fixed here.
            start: element_start,
            end: 0,
            attribute_spans: if self.record_spans { resolved_spans } else { Vec::new() },
            content_spans: Vec::new(),
        });
        if element_index == 0 {
            self.tree.root = 0;
        }
        // Link into the parent's content. The `content_spans` alignment is
        // kept by pushing a placeholder, exactly as [`Self::append_content`]
        // does for its non-text events — an element child shifts the text
        // spans that follow it, and a missing placeholder would bind a
        // later text's span to the wrong node.
        if !self.frames.is_empty() {
            let parent = self.frames.last().expect("parent").element_index;
            self.tree.elements[parent]
                .content
                .push(ContentEvent::Element(element_index));
            if self.record_spans {
                self.tree.elements[parent].content_spans.push(None);
            }
        }
        self.frames.push(OpenFrame {
            element_index,
            bindings,
            name: element_expanded,
            last_was_text: false,
        });
        Ok(())
    }

    fn close_last_element_for(&mut self, expanded: ExpandedName) -> Result<(), CodecError> {
        let frame = self
            .frames
            .last()
            .ok_or_else(|| syntax_impl("end tag with no open element"))?;
        if frame.name != expanded {
            return Err(syntax_impl("mismatched end tag"));
        }
        self.pop_frame()
    }

    fn pop_frame(&mut self) -> Result<(), CodecError> {
        let frame = self.frames.pop().ok_or_else(|| syntax_impl("no open element"))?;
        for _ in frame.bindings {
            self.namespace_bindings.pop();
        }
        // A popped root child completes its recorded extent: `position` is
        // one past the end tag's `>` (or past a self-closing `/>`).
        let end = self.position;
        if matches!(self.retention, Retention::Measure(_))
            && let Some(MeasureChild::Element { end: slot, .. }) = self.retention_last_element_child_mut()
        {
            *slot = end;
        }
        if let Retention::Locate(locate) = &mut self.retention {
            if self.frames.is_empty() {
                locate.root_end = end;
            } else if self.frames.len() == 1
                && let Some(crate::locate::LocateChild::Element { end: slot, .. }) = locate.children.last_mut()
            {
                *slot = end;
            }
        }
        // Build retention: complete the element's authored extent on the tree
        // node itself (`usize::MAX` is the measure-mode placeholder).
        if frame.element_index != usize::MAX {
            self.tree.elements[frame.element_index].end = end;
        }
        Ok(())
    }

    /// The last recorded measure child, when it is the element child whose
    /// extent is still being recorded (`end == 0`).
    fn retention_last_element_child_mut(&mut self) -> Option<&mut MeasureChild> {
        let Retention::Measure(children) = &mut self.retention else {
            return None;
        };
        match children.last_mut() {
            Some(MeasureChild::Element { end: 0, .. }) => children.last_mut(),
            _ => None,
        }
    }

    fn append_text(&mut self, text: &str, start: usize, end: usize) -> Result<(), CodecError> {
        if text.is_empty() {
            return Ok(());
        }
        if self.skips_tree() {
            // The measure/locate records the DOCUMENT element's direct
            // children only: a text run inside a direct child element is the
            // CHILD's content, not the root's.
            if self.frames.len() == 1 {
                self.record_measure_child(MeasureChild::Text(text.to_owned()));
                self.record_locate_child(crate::locate::LocateChild::Text(text.to_owned()));
            }
            return Ok(());
        }
        let index = self
            .frames
            .last()
            .ok_or_else(|| syntax_impl("character data outside an element"))?
            .element_index;
        self.account_child(true);
        let element = &mut self.tree.elements[index];
        if let Some(ContentEvent::Text(existing)) = element.content.last_mut() {
            existing.push_str(text);
            // A CDATA section sits between the merged runs: the node's
            // authored span extends past the CDATA markup, whose bytes are
            // part of the authored text extent.
            if let Some(Some(span)) = element.content_spans.last_mut() {
                span.1 = end;
            }
        } else {
            element.content.push(ContentEvent::Text(text.to_owned()));
            if self.record_spans {
                element.content_spans.push(Some((start, end)));
            }
        }
        Ok(())
    }

    /// `span` is the authored extent of a comment child (`<!--` through
    /// `-->`), bound so the edit lane's comment write can replace comment
    /// children by their spans; every other content event passes `None` (a
    /// PI has no span, and the element children bind their own extents).
    fn append_content(&mut self, event: ContentEvent, span: Option<(usize, usize)>) -> Result<(), CodecError> {
        if self.skips_tree() {
            // The measure/locate records the DOCUMENT element's direct
            // children only (see [`Self::append_text`]).
            if self.frames.len() == 1 {
                match event {
                    ContentEvent::Comment(text) => {
                        self.record_measure_child(MeasureChild::Comment(text.clone()));
                        self.record_locate_child(crate::locate::LocateChild::Comment(text));
                    }
                    ContentEvent::ProcessingInstruction(payload) => {
                        let (target, data) = *payload;
                        self.record_measure_child(MeasureChild::ProcessingInstruction {
                            target: target.clone(),
                            data: data.clone(),
                        });
                        self.record_locate_child(crate::locate::LocateChild::ProcessingInstruction { target, data });
                    }
                    ContentEvent::Element(_) | ContentEvent::Text(_) => {}
                }
            }
            return Ok(());
        }
        let index = self
            .frames
            .last()
            .ok_or_else(|| syntax_impl("markup outside an element"))?
            .element_index;
        self.account_child(false);
        self.tree.elements[index].content.push(event);
        // Non-text events carry no authored text span unless the caller
        // bound one (a comment child's extent); the alignment with
        // [`Element::content_spans`] is kept by pushing a placeholder.
        if self.record_spans {
            self.tree.elements[index].content_spans.push(span);
        }
        Ok(())
    }

    /// Records one content child of the current (parent) element, coalescing
    /// adjacent text runs into one child.
    ///
    /// The parent is `self.frames.last()`; for a text/comment/PI child this
    /// is the element being appended to, and for an element child it is the
    /// parent, called before the child's frame is pushed.
    fn account_child(&mut self, is_text: bool) {
        if let Some(parent) = self.frames.last_mut()
            && !(is_text && parent.last_was_text)
        {
            parent.last_was_text = is_text;
        }
    }
    fn decode_range(bytes: &[u8], start: usize, end: usize) -> Result<String, CodecError> {
        // SAFETY: `try_new` validated the whole source as UTF-8. `start` and
        // `end` land on ASCII terminator bytes (`-`, `]`, `?`, `>`) or on
        // character boundaries the parser already advanced by whole
        // characters, so both ends are char boundaries of that proven
        // buffer. Same containment as `push_char_data`.
        Ok(unsafe { core::str::from_utf8_unchecked(&bytes[start..end]) }.to_owned())
    }
}

/// Whether `bytes[position..]` is an XML declaration, not a PI whose target
/// merely starts with `xml` (`<?xml-stylesheet …?>`).
fn is_xml_declaration_at(bytes: &[u8], position: usize) -> bool {
    let rest = &bytes[position..];
    rest.starts_with(b"<?xml") && matches!(rest.get(5), Some(&b' ' | &b'\t' | &b'\n' | &b'\r' | &b'?'))
}

/// XML 1.0 `VersionNum` that this processor accepts: exactly `1.0`.
fn is_xml10_version(value: &str) -> bool {
    value == "1.0"
}

/// The UTF-8 byte-order mark: signature metadata before the document, never
/// content (one is skipped by `try_new`).
const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";

/// The XML declaration's required attribute order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeclStage {
    Version,
    Encoding,
    Standalone,
    Done,
}

/// XML 1.0 §2.11: `#xD#xA` and a lone `#xD` become `#xA`.
///
/// CR-free input (the common case) is returned borrowed after one stop-set
/// probe; a hit allocates once and copies at byte level.
fn normalize_line_endings(text: &str) -> Cow<'_, str> {
    if prefix_len::<StopCr>(text.as_bytes()) == text.len() {
        return Cow::Borrowed(text);
    }
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        let skip = prefix_len::<StopCr>(&bytes[i..]);
        if skip > 0 {
            // SAFETY: `i..i+skip` is a CR-free substring of a valid UTF-8
            // buffer; CR is ASCII, so the range ends on a char boundary.
            out.push_str(unsafe { core::str::from_utf8_unchecked(&bytes[i..i + skip]) });
            i += skip;
            if i >= bytes.len() {
                break;
            }
        }
        debug_assert_eq!(bytes[i], b'\r');
        out.push('\n');
        i += 1;
        if i < bytes.len() && bytes[i] == b'\n' {
            i += 1;
        }
    }
    Cow::Owned(out)
}

/// XML 1.0 §3.3.3: after line-ending normalization, a literal TAB or LF
/// in an attribute value becomes a space.
fn normalize_attribute_value(text: &str) -> String {
    if prefix_len::<StopAttrNorm>(text.as_bytes()) == text.len() {
        return text.to_owned();
    }
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        let skip = prefix_len::<StopAttrNorm>(&bytes[i..]);
        if skip > 0 {
            // SAFETY: same as `normalize_line_endings`: the skip is a run of
            // bytes other than CR/TAB/LF, so both ends are ASCII-adjacent
            // char boundaries of a proven UTF-8 buffer.
            out.push_str(unsafe { core::str::from_utf8_unchecked(&bytes[i..i + skip]) });
            i += skip;
            if i >= bytes.len() {
                break;
            }
        }
        match bytes[i] {
            b'\r' => {
                out.push(' ');
                i += 1;
                if i < bytes.len() && bytes[i] == b'\n' {
                    i += 1;
                }
            }
            b'\t' | b'\n' => {
                out.push(' ');
                i += 1;
            }
            _ => unreachable!("StopAttrNorm stops only at CR, TAB, or LF"),
        }
    }
    out
}

/// `>` — the `]]>` lookbehind anchor and the DOCTYPE `>` terminator.
#[derive(Clone, Copy)]
struct StopGt;
impl StopSet for StopGt {
    const EQ: [u8; 8] = [b'>', 0, 0, 0, 0, 0, 0, 0];
    const EQ_LEN: u8 = 1;
    const LT: Option<u8> = None;
    const GE: Option<u8> = None;
    const ALL: bool = false;
}

/// `-` — comment terminator and `--` well-formedness scan.
#[derive(Clone, Copy)]
struct StopDash;
impl StopSet for StopDash {
    const EQ: [u8; 8] = [b'-', 0, 0, 0, 0, 0, 0, 0];
    const EQ_LEN: u8 = 1;
    const LT: Option<u8> = None;
    const GE: Option<u8> = None;
    const ALL: bool = false;
}

/// `?` — PI / XML-declaration terminator.
#[derive(Clone, Copy)]
struct StopQ;
impl StopSet for StopQ {
    const EQ: [u8; 8] = [b'?', 0, 0, 0, 0, 0, 0, 0];
    const EQ_LEN: u8 = 1;
    const LT: Option<u8> = None;
    const GE: Option<u8> = None;
    const ALL: bool = false;
}

/// `]` — CDATA terminator first byte.
#[derive(Clone, Copy)]
struct StopBracket;
impl StopSet for StopBracket {
    const EQ: [u8; 8] = [b']', 0, 0, 0, 0, 0, 0, 0];
    const EQ_LEN: u8 = 1;
    const LT: Option<u8> = None;
    const GE: Option<u8> = None;
    const ALL: bool = false;
}

/// CR — line-ending normalization probe.
#[derive(Clone, Copy)]
struct StopCr;
impl StopSet for StopCr {
    const EQ: [u8; 8] = [b'\r', 0, 0, 0, 0, 0, 0, 0];
    const EQ_LEN: u8 = 1;
    const LT: Option<u8> = None;
    const GE: Option<u8> = None;
    const ALL: bool = false;
}

/// CR, TAB, LF — attribute-value whitespace normalization probe.
#[derive(Clone, Copy)]
struct StopAttrNorm;
impl StopSet for StopAttrNorm {
    const EQ: [u8; 8] = [b'\r', b'\t', b'\n', 0, 0, 0, 0, 0];
    const EQ_LEN: u8 = 3;
    const LT: Option<u8> = None;
    const GE: Option<u8> = None;
    const ALL: bool = false;
}

/// Literal `]]>` inside one clean character-data run: scan for `>` and
/// confirm the two-byte lookbehind. Per-run scope matches the previous
/// `windows(3)` walk (a marker that straddles an entity boundary is
/// undetected either way).
fn contains_cdata_end(bytes: &[u8]) -> bool {
    let mut i = 0;
    while i < bytes.len() {
        let skip = prefix_len::<StopGt>(&bytes[i..]);
        let pos = i + skip;
        if pos >= bytes.len() {
            return false;
        }
        if pos >= 2 && bytes[pos - 2] == b']' && bytes[pos - 1] == b']' {
            return true;
        }
        i = pos + 1;
    }
    false
}

/// First-byte stop-set scan for a terminator, then confirm the remaining
/// bytes at each hit. Replaces the scalar `windows()` walk.
fn find_terminator(haystack: &[u8], terminator: &[u8]) -> Option<usize> {
    let (first, rest) = terminator.split_first()?;
    let mut i = 0;
    while i < haystack.len() {
        let skip = match *first {
            b'>' => prefix_len::<StopGt>(&haystack[i..]),
            b'-' => prefix_len::<StopDash>(&haystack[i..]),
            b'?' => prefix_len::<StopQ>(&haystack[i..]),
            b']' => prefix_len::<StopBracket>(&haystack[i..]),
            other => haystack[i..]
                .iter()
                .position(|&b| b == other)
                .unwrap_or(haystack[i..].len()),
        };
        let pos = i + skip;
        if pos >= haystack.len() {
            return None;
        }
        let after = pos + 1;
        if haystack.get(after..after + rest.len()) == Some(rest) {
            return Some(pos);
        }
        i = pos + 1;
    }
    None
}

/// Longest prefix of `bytes` that is XML `S` whitespace (space, tab, LF, CR).
fn ws_prefix_len(bytes: &[u8]) -> usize {
    let mut n = 0;
    while n < 8 {
        match bytes.get(n) {
            Some(&b' ' | b'\t' | b'\n' | b'\r') => n += 1,
            _ => return n,
        }
    }
    8 + prefix_len::<Ws>(&bytes[8..])
}

fn split_decl_attributes(inner: &str) -> Result<Vec<(String, String)>, CodecError> {
    // `version="1.0" encoding="UTF-8" standalone="yes"`
    let mut out = Vec::new();
    let mut rest = inner.trim();
    while !rest.is_empty() {
        let eq = rest.find('=').ok_or_else(|| syntax_impl("malformed XML declaration"))?;
        let key = rest[..eq].trim().to_string();
        let after = rest[eq + 1..].trim_start();
        let quote = after
            .chars()
            .next()
            .filter(|c| *c == '"' || *c == '\'')
            .ok_or_else(|| syntax_impl("malformed XML declaration"))?;
        let close = after[1..]
            .find(quote)
            .map(|i| i + 1)
            .ok_or_else(|| syntax_impl("malformed XML declaration"))?;
        let value = after[1..close].to_string();
        out.push((key, value));
        rest = after[close + 1..].trim();
    }
    Ok(out)
}

/// Byte index of the next `>` that is not inside a quoted value.
fn closing_gt(text: &str) -> Option<usize> {
    let mut quoting = false;
    let mut quote = '\0';
    for (index, ch) in text.char_indices() {
        if quoting {
            if ch == quote {
                quoting = false;
            }
        } else if ch == '"' || ch == '\'' {
            quoting = true;
            quote = ch;
        } else if ch == '>' {
            return Some(index);
        }
    }
    None
}

fn extract_entity(decl_rest: &str) -> Result<(String, Option<String>), CodecError> {
    // `name "value"` with an optional preceding `%` (parameter entity).
    let trimmed = decl_rest.trim_start();
    let mut tokens: Vec<(String, bool)> = Vec::new();
    let mut current = String::new();
    let mut quoting = false;
    let mut quote = '\0';
    for ch in trimmed.chars() {
        if quoting {
            if ch == quote {
                quoting = false;
                tokens.push((core::mem::take(&mut current), true));
            } else {
                current.push(ch);
            }
        } else if ch == '"' || ch == '\'' {
            quoting = true;
            quote = ch;
        } else if ch.is_whitespace() {
            if !current.is_empty() {
                tokens.push((core::mem::take(&mut current), false));
            }
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        tokens.push((current, false));
    }
    let mut idx = 0;
    if tokens.get(idx).map(|(s, _)| s.as_str()) == Some("%") {
        idx += 1;
    }
    let Some((name, _)) = tokens.get(idx).cloned() else {
        return Ok((String::new(), None));
    };
    if name.starts_with('%') {
        return Ok((name, None));
    }
    // An external entity declaration (`<!ENTITY name SYSTEM "uri">` or
    // `PUBLIC`) has no quoted replacement text: the next token is the
    // external-id keyword, never a value. External entities are DISABLED (the
    // secure processor performs no substitution), so the declaration is not
    // registered — a reference to it is then unbound and fails where it
    // stands. A quoted value that happens to spell `SYSTEM` or `PUBLIC` is
    // still replacement text.
    match tokens.get(idx + 1) {
        Some((value, false)) if value == "SYSTEM" || value == "PUBLIC" => Ok((name, None)),
        Some((value, _)) => Ok((name, Some(value.clone()))),
        None => Ok((name, None)),
    }
}

fn is_ascii_name_start(b: u8) -> bool {
    b == b':' || b == b'_' || b.is_ascii_alphabetic()
}

fn is_ascii_name_char(b: u8) -> bool {
    is_ascii_name_start(b) || b == b'-' || b == b'.' || b.is_ascii_digit()
}

pub(crate) fn is_name_start(c: char) -> bool {
    c == ':'
        || c == '_'
        || c.is_ascii_alphabetic()
        || ('\u{C0}'..='\u{D6}').contains(&c)
        || ('\u{D8}'..='\u{F6}').contains(&c)
        || ('\u{F8}'..='\u{2FF}').contains(&c)
        || ('\u{370}'..='\u{37D}').contains(&c)
        || ('\u{37F}'..='\u{1FFF}').contains(&c)
        || ('\u{200C}'..='\u{200D}').contains(&c)
        || ('\u{2070}'..='\u{218F}').contains(&c)
        || ('\u{2C00}'..='\u{2FEF}').contains(&c)
        || ('\u{3001}'..='\u{D7FF}').contains(&c)
        || ('\u{F900}'..='\u{FDCF}').contains(&c)
        || ('\u{FDF0}'..='\u{FFFD}').contains(&c)
        || ('\u{10000}'..='\u{EFFFF}').contains(&c)
}

pub(crate) fn is_name_char(c: char) -> bool {
    is_name_start(c)
        || c == '-'
        || c == '.'
        || c.is_ascii_digit()
        || c == '\u{B7}'
        || ('\u{300}'..='\u{36F}').contains(&c)
        || ('\u{203F}'..='\u{2040}').contains(&c)
}

fn is_char(c: char) -> bool {
    c == '\u{9}'
        || c == '\u{A}'
        || c == '\u{D}'
        || ('\u{20}'..='\u{D7FF}').contains(&c)
        || ('\u{E000}'..='\u{FFFD}').contains(&c)
        || ('\u{10000}'..='\u{10FFFF}').contains(&c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(input: &str) -> Tree {
        match XmlParseState::try_new(input.as_bytes())
            .expect("state")
            .parse(input.as_bytes())
            .expect("parse")
        {
            ParseOutput::Tree(tree) => tree,
            _ => panic!("build parse returned a non-tree"),
        }
    }

    #[test]
    fn empty_is_rejected() {
        let result = XmlParseState::try_new(b"").expect("state").parse(b"");
        assert!(result.is_err());
    }

    #[test]
    fn simple_document() {
        let tree = parse_ok("<a><b>x</b><b>y</b><c/></a>");
        assert_eq!(tree.elements.len(), 4);
        assert_eq!(tree.local_of(tree.elements[tree.root].name), "a");
        let root_content = &tree.elements[0].content;
        assert_eq!(root_content.len(), 3);
        let ContentEvent::Element(b0) = root_content[0] else {
            panic!("expected element child");
        };
        assert_eq!(tree.local_of(tree.elements[b0].name), "b");
    }

    #[test]
    fn text_and_content() {
        let tree = parse_ok("<a>hello<b>world</b></a>");
        let content = &tree.elements[0].content;
        assert!(matches!(content[0], ContentEvent::Text(ref t) if t == "hello"));
        assert!(matches!(content[1], ContentEvent::Element(_)));
    }

    #[test]
    fn self_closing_does_not_expect_end() {
        let tree = parse_ok("<a><b/><c></c></a>");
        assert_eq!(tree.elements.len(), 3);
        let content = &tree.elements[0].content;
        assert!(matches!(content[0], ContentEvent::Element(_)));
        assert!(matches!(content[1], ContentEvent::Element(_)));
    }

    #[test]
    fn entities_resolve() {
        let tree = parse_ok("<a>&lt;&amp;&gt;</a>");
        let ContentEvent::Text(text) = &tree.elements[0].content[0] else {
            panic!("expected text");
        };
        assert_eq!(text, "<&>");
    }

    #[test]
    fn mismatched_end_is_rejected() {
        let result = XmlParseState::try_new(b"<a></b>").expect("state").parse(b"<a></b>");
        assert!(result.is_err());
    }

    #[test]
    fn cdata_is_text() {
        let tree = parse_ok("<a><![CDATA[x<y>]]></a>");
        let ContentEvent::Text(text) = &tree.elements[0].content[0] else {
            panic!("expected text");
        };
        assert_eq!(text, "x<y>");
    }

    #[test]
    fn namespace_resolution() {
        let tree =
            parse_ok("<root xmlns='http://e/' xmlns:p='http://p/'><p:child xmlns:c='http://c/' c:a='v'/></root>");
        assert_eq!(tree.uri_of(tree.elements[0].name), "http://e/");
        let ContentEvent::Element(child) = tree.elements[0].content[0] else {
            panic!("expected child");
        };
        assert_eq!(tree.uri_of(tree.elements[child].name), "http://p/");
        assert_eq!(tree.local_of(tree.elements[child].name), "child");
        assert_eq!(tree.uri_of(tree.elements[child].attributes[0].0), "http://c/");
        assert_eq!(tree.elements[child].attributes[0].1, "v");
    }

    #[test]
    fn internal_entity() {
        let tree = parse_ok("<!DOCTYPE a [<!ENTITY x \"yo\">]><a>&x;</a>");
        let ContentEvent::Text(text) = &tree.elements[0].content[0] else {
            panic!("expected text");
        };
        assert_eq!(text, "yo");
    }

    #[test]
    fn dtd_subset_scans_quote_state_before_comments() {
        // An entity VALUE may contain `<!--`: inside a quoted declaration
        // value those bytes are literal text, never a comment to skip past.
        // Scanning comments before quote state eats the rest of the subset.
        let tree = parse_ok("<!DOCTYPE a [<!ENTITY c \"<!--\"><!-- real --><!ENTITY x \"yo\">]><a>&x;</a>");
        let ContentEvent::Text(text) = &tree.elements[0].content[0] else {
            panic!("expected text");
        };
        assert_eq!(text, "yo");
    }

    #[test]
    fn external_id_doctype_still_parses_internal_subset() {
        // `<!DOCTYPE name SYSTEM|PUBLIC … [ … ]>`: the external id is
        // skipped (no I/O), but an internal subset FOLLOWING it still
        // registers its entities.
        for head in [
            "<!DOCTYPE a SYSTEM \"outer.dtd\" ",
            "<!DOCTYPE a PUBLIC \"pub-id\" \"outer.dtd\" ",
            // A `>` inside a quoted SystemLiteral must not end the walk.
            "<!DOCTYPE a SYSTEM \"out>er.dtd\" ",
        ] {
            let source = alloc::format!("{head}[<!ENTITY x \"yo\">]><a>&x;</a>");
            let tree = parse_ok(&source);
            let ContentEvent::Text(text) = &tree.elements[0].content[0] else {
                panic!("expected text");
            };
            assert_eq!(text, "yo", "entity lost after external id {head:?}");
        }
    }

    #[test]
    fn external_entity_declaration_is_not_registered() {
        // An external entity declaration (`SYSTEM`/`PUBLIC`) is DISABLED: it
        // must not register the keyword as replacement text (the pre-fix
        // behavior silently expanded `&ext;` to the literal text `SYSTEM`). A
        // reference to it is therefore unbound and fails where it stands,
        // matching the unbound-entity doctrine.
        let result = XmlParseState::try_new(b"<!DOCTYPE a [<!ENTITY ext SYSTEM \"http://x/y\">]><a>&ext;</a>")
            .expect("state")
            .parse(b"<!DOCTYPE a [<!ENTITY ext SYSTEM \"http://x/y\">]><a>&ext;</a>");
        assert!(result.is_err(), "an external entity reference must fail");
        // The same for PUBLIC.
        let result = XmlParseState::try_new(b"<!DOCTYPE a [<!ENTITY ext PUBLIC \"id\" \"uri\">]><a>&ext;</a>")
            .expect("state")
            .parse(b"<!DOCTYPE a [<!ENTITY ext PUBLIC \"id\" \"uri\">]><a>&ext;</a>");
        assert!(result.is_err(), "a PUBLIC external entity reference must fail");
        // A declaration that is never referenced stays inert.
        let tree = parse_ok("<!DOCTYPE a [<!ENTITY ext SYSTEM \"http://x/y\">]><a>v</a>");
        let ContentEvent::Text(text) = &tree.elements[0].content[0] else {
            panic!("expected text");
        };
        assert_eq!(text, "v");
    }

    #[test]
    fn entity_values_expand_recursively_on_use() {
        let tree = parse_ok("<!DOCTYPE a [<!ENTITY a \"1\"><!ENTITY b \"&a;2\">]><a>&b;</a>");
        let ContentEvent::Text(text) = &tree.elements[0].content[0] else {
            panic!("expected text");
        };
        assert_eq!(text, "12");
    }

    #[test]
    fn recursive_entity_reference_rejected() {
        let result = XmlParseState::try_new(b"<!DOCTYPE a [<!ENTITY a \"&a;\">]><a>&a;</a>")
            .expect("state")
            .parse(b"<!DOCTYPE a [<!ENTITY a \"&a;\">]><a>&a;</a>");
        assert!(result.is_err());
    }

    #[test]
    fn attribute_charref_whitespace_survives_normalization() {
        // XML 1.0 §3.3.3: whitespace normalization applies to LITERAL
        // attribute text only — character-referenced TAB/LF/CR is not
        // literal and must survive.
        let tree = parse_ok("<a x=\"p&#xA;q\"/>");
        assert_eq!(tree.elements[0].attributes[0].1, "p\nq");
        let tree = parse_ok("<a x=\"p&#x9;q\"/>");
        assert_eq!(tree.elements[0].attributes[0].1, "p\tq");
        // The literal twin of the same byte still becomes a space.
        let tree = parse_ok("<a x=\"p\nq\"/>");
        assert_eq!(tree.elements[0].attributes[0].1, "p q");
    }

    #[test]
    fn leading_bom_is_signature_not_content() {
        // One leading UTF-8 BOM precedes the declaration; two is content.
        let tree = parse_ok("\u{FEFF}<a>x</a>");
        assert_eq!(tree.local_of(tree.elements[tree.root].name), "a");
        let tree = parse_ok("\u{FEFF}<?xml version=\"1.0\"?><a/>");
        assert_eq!(tree.elements.len(), 1);
        assert!(
            XmlParseState::try_new("\u{FEFF}\u{FEFF}<a/>".as_bytes())
                .expect("state")
                .parse("\u{FEFF}\u{FEFF}<a/>".as_bytes())
                .is_err()
        );
    }

    #[test]
    fn cdata_body_rejects_invalid_characters() {
        // The Char production holds inside CDATA too; tab/LF/CR stay valid.
        let result = XmlParseState::try_new(b"<a><![CDATA[o\x01k]]></a>")
            .expect("state")
            .parse(b"<a><![CDATA[o\x01k]]></a>");
        assert!(result.is_err());
        let tree = parse_ok("<a><![CDATA[a\tb]]></a>");
        let ContentEvent::Text(text) = &tree.elements[0].content[0] else {
            panic!("expected text");
        };
        assert_eq!(text, "a\tb");
    }

    #[test]
    fn comment_and_pi_bodies_reject_invalid_characters() {
        for bad in [
            &b"<a><!-- \x01 --></a>"[..],
            &b"<a><?tgt \x01?></a>"[..],
            &b"<a><![CDATA[x\x1B]]></a>"[..],
        ] {
            let result = XmlParseState::try_new(bad).expect("state").parse(bad);
            assert!(result.is_err(), "invalid Char must reject {bad:?}");
        }
    }

    #[test]
    fn second_element_after_root_rejected() {
        let result = XmlParseState::try_new(b"<a/><b/>").expect("state").parse(b"<a/><b/>");
        assert!(result.is_err());
    }

    #[test]
    fn trailing_content_after_root_rejected() {
        // Character data after the document element is an error (XML 1.0's
        // `Misc` admits only comments, processing instructions, and
        // whitespace); an epilog COMMENT is legal and parses.
        let result = XmlParseState::try_new(b"<a/>text").expect("state").parse(b"<a/>text");
        assert!(result.is_err());
        let result = XmlParseState::try_new(b"<a/><!--c-->")
            .expect("state")
            .parse(b"<a/><!--c-->");
        assert!(result.is_ok(), "an epilog comment is legal Misc");
    }

    #[test]
    fn duplicate_expanded_attribute_rejected() {
        let result = XmlParseState::try_new(b"<a xmlns:p='http://p/' p:x='1' xmlns:q='http://p/' q:x='2'/>")
            .expect("state")
            .parse(b"<a xmlns:p='http://p/' p:x='1' xmlns:q='http://p/' q:x='2'/>");
        assert!(result.is_err());
    }

    /// Drives a parse through the cooperative poll with explicit limits.
    fn poll_parse(input: &[u8], limits: jqf_resource::ResourceLimits) -> Result<ParseOutput, CodecError> {
        let mut resources = jqf_resource::ResourceContext::new(
            jqf_resource::RequestAccount::try_new(limits).expect("account"),
            &jqf_resource::ContinueControl,
            jqf_resource::WorkMeter::try_new_v1(4096).expect("work"),
        )
        .expect("resources");
        let mut state = XmlParseState::try_new(input)?;
        loop {
            match state.poll(input, &mut resources)? {
                ParsePoll::Pending => {
                    resources.try_begin_next_cooperative_entry(1).expect("resume");
                }
                ParsePoll::Ready(output) => return Ok(output),
            }
        }
    }

    #[test]
    fn deep_nesting_bounded_at_the_governed_ceiling() {
        // The element ceiling is the request's governed nesting limit, not
        // a parser-local constant. Decode refuses one level past it with
        // the same resource error the document build raises, so no
        // retention accepts a depth no later recursion can process.
        let at = 64usize;
        let input = format!("{}{}", "<a>".repeat(at), "</a>".repeat(at));
        let limits = jqf_resource::ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, 64);
        poll_parse(input.as_bytes(), limits).expect("depth at the ceiling parses");
        let over = format!("{}{}", "<a>".repeat(at + 1), "</a>".repeat(at + 1));
        let Err(err) = poll_parse(over.as_bytes(), limits) else {
            panic!("past the ceiling must reject");
        };
        let CodecFailureKind::Resource(jqf_resource::ResourceError::LimitExceeded {
            limit_kind: jqf_resource::ResourceLimit::NestingDepth,
            limit: 64,
            current: 64,
            requested_delta: 1,
        }) = err.kind()
        else {
            panic!("expected the nesting-limit resource error, got {err:?}");
        };
    }

    #[test]
    fn deep_entity_chain_is_bounded_by_the_expansion_depth() {
        // A legal-shaped chain (no cycle) longer than the expansion bound
        // is refused; a chain inside the bound still resolves. The bound is
        // on NESTING, independent of how many entities are declared.
        let build = |count: usize| {
            use core::fmt::Write as _;
            let mut dtd = String::from("<!DOCTYPE a [");
            for i in 0..count {
                if i + 1 < count {
                    write!(dtd, r#"<!ENTITY e{i} "&e{};">"#, i + 1).expect("write");
                } else {
                    write!(dtd, r#"<!ENTITY e{i} "x">"#).expect("write");
                }
            }
            dtd.push_str("]><a>&e0;</a>");
            dtd
        };
        let inside = parse_ok(&build(31));
        let ContentEvent::Text(text) = &inside.elements[0].content[0] else {
            panic!("expected text");
        };
        assert_eq!(text, "x");
        assert!(
            XmlParseState::try_new(build(33).as_bytes())
                .expect("state")
                .parse(build(33).as_bytes())
                .is_err()
        );
    }

    #[test]
    fn character_references_are_not_accounted_against_the_entity_cap() {
        // Only general-entity expansions are charged against the
        // replacement-text cap: a character or predefined reference emits
        // fewer bytes than its source spelling, so a heavily escaped
        // document cannot amplify.
        let input = "<!DOCTYPE a [<!ENTITY g \"yo\">]><a>&#65;&amp;&g;&#x42;</a>";
        let mut state = XmlParseState::try_new(input.as_bytes()).expect("state");
        loop {
            if state.step(input.as_bytes(), u32::MAX).expect("step") == Drive::Complete {
                break;
            }
        }
        assert_eq!(state.entity_replacement_bytes, 2);
    }

    #[test]
    fn pi_target_must_be_a_name_without_a_colon() {
        // XML 1.0 §2.6: the PI target is a Name; Namespaces in XML adds the
        // no-colon rule. Non-name targets were previously accepted up to
        // the first whitespace byte.
        for doc in [
            "<a><?<b x?></a>",
            "<a><?1x?></a>",
            "<a><?-x?></a>",
            "<a><?\"q\"?></a>",
            "<a><?x:y?></a>",
            "<?1x?><a/>",
        ] {
            parse_err(doc);
        }
        parse_ok("<a><?pi data?><b/></a>");
        parse_ok("<?pi?><a/>");
        parse_ok("<a><?π?></a>");
    }

    #[test]
    fn attribute_entity_wfc_builtin_lt_keeps_working() {
        // A predefined `&lt;` directly in an attribute value is legal: the
        // WFC applies to the replacement text of a referenced ENTITY, never
        // to a builtin or a character reference in the source.
        let tree = parse_ok("<a x=\"&lt;\"/>");
        assert_eq!(tree.elements[0].attributes[0].1, "<");
        let tree = parse_ok("<a x=\"&#60;\"/>");
        assert_eq!(tree.elements[0].attributes[0].1, "<");
    }

    #[test]
    fn attribute_entity_replacement_with_lt_rejected() {
        // XML 1.0 §3.3.3 [WFC: No < in Attribute Values]: the replacement
        // text of an entity referenced in an attribute value must contain no
        // `<`.
        for dtd in [
            "<!DOCTYPE a [<!ENTITY e \"<evil\">]><a x=\"&e;\"/>",
            "<!DOCTYPE a [<!ENTITY e \"&#60;\">]><a x=\"&e;\"/>",
            "<!DOCTYPE a [<!ENTITY e \"a&#60;b\">]><a x=\"&e;\"/>",
        ] {
            let result = XmlParseState::try_new(dtd.as_bytes())
                .expect("state")
                .parse(dtd.as_bytes());
            assert!(result.is_err(), "must reject: {dtd}");
        }
    }

    #[test]
    fn attribute_entity_replacement_with_bare_amp_rejected() {
        // A character reference resolving to `&` leaves a bare `&` in the
        // entity's replacement text, which the attribute grammar forbids.
        let result = XmlParseState::try_new(b"<!DOCTYPE a [<!ENTITY e \"&#38;\">]><a x=\"&e;\"/>")
            .expect("state")
            .parse(b"<!DOCTYPE a [<!ENTITY e \"&#38;\">]><a x=\"&e;\"/>");
        assert!(result.is_err());
    }

    #[test]
    fn attribute_entity_named_reference_to_lt_is_legal() {
        // The replacement text `&lt;` contains no `<`; the reference is
        // resolved during attribute-value processing, where `<` is fine.
        let tree = parse_ok("<!DOCTYPE a [<!ENTITY e \"&lt;\">]><a x=\"&e;\"/>");
        assert_eq!(tree.elements[0].attributes[0].1, "<");
        // Indirectly referred entities obey the same law.
        let tree = parse_ok("<!DOCTYPE a [<!ENTITY e \"&lt;x\"><!ENTITY f \"&e;\">]><a x=\"&f;\"/>");
        assert_eq!(tree.elements[0].attributes[0].1, "<x");
    }

    #[test]
    fn attribute_entity_replacement_legal_text_unaffected() {
        let tree = parse_ok("<!DOCTYPE a [<!ENTITY e \"hi\">]><a x=\"&e;\"/>");
        assert_eq!(tree.elements[0].attributes[0].1, "hi");
        // Content context is untouched: entity markup-ish text still expands
        // to text without the attribute-value WFC.
        let tree = parse_ok("<!DOCTYPE a [<!ENTITY e \"&lt;b&gt;\">]><a>&e;</a>");
        let ContentEvent::Text(text) = &tree.elements[0].content[0] else {
            panic!("expected text");
        };
        assert_eq!(text, "<b>");
    }

    #[test]
    fn dtd_subset_entity_value_brackets_are_quoted() {
        // A `[` inside a quoted entity value is not a subset bracket
        // (quote-aware depth scan).
        let tree = parse_ok("<!DOCTYPE r [ <!ENTITY e \"[\"> ]><r/>");
        assert_eq!(tree.local_of(tree.elements[tree.root].name), "r");
        let tree = parse_ok("<!DOCTYPE r [<!ENTITY e '['>]><r/>");
        assert_eq!(tree.local_of(tree.elements[tree.root].name), "r");
        // A quote of the other kind inside a quoted value stays inert.
        let tree = parse_ok("<!DOCTYPE r [<!ENTITY e 'a\"b'>]><r/>");
        assert_eq!(tree.local_of(tree.elements[tree.root].name), "r");
    }

    #[test]
    fn prolog_comments_do_not_shift_content_spans() {
        let input = "<!--a--><!--b--><r>x<y/>z</r>";
        let tree = parse_ok(input);
        let root = &tree.elements[tree.root];
        assert_eq!(root.content.len(), root.content_spans.len());
        assert_eq!(root.content_spans[0], None);
        assert_eq!(root.content_spans[1], None);
        let (start, end) = root.content_spans[2].expect("text x keeps its span");
        assert_eq!(&input[start..end], "x");
        assert_eq!(root.content_spans[3], None);
        let (start, end) = root.content_spans[4].expect("text z keeps its span");
        assert_eq!(&input[start..end], "z");
    }

    #[test]
    fn character_reference_cr_survives_in_content() {
        let tree = parse_ok("<a>&#13;</a>");
        let ContentEvent::Text(text) = &tree.elements[0].content[0] else {
            panic!("expected text");
        };
        assert_eq!(text, "\r");
        let tree = parse_ok("<a>a\rb</a>");
        let ContentEvent::Text(text) = &tree.elements[0].content[0] else {
            panic!("expected text");
        };
        assert_eq!(text, "a\nb");
    }

    #[test]
    fn entity_value_gt_does_not_invent_a_declaration() {
        let tree = parse_ok("<!DOCTYPE a [<!ENTITY a \"x><!ENTITY e 'evil'\"><!ENTITY c \"w\">]><a>&a;</a>");
        let ContentEvent::Text(text) = &tree.elements[0].content[0] else {
            panic!("expected text");
        };
        assert_eq!(text, "x><!ENTITY e 'evil'");
        parse_err("<!DOCTYPE a [<!ENTITY a \"x><!ENTITY e 'evil'\"><!ENTITY c \"w\">]><a>&e;</a>");
        let tree = parse_ok("<!DOCTYPE a [<!ENTITY a \"x><!ENTITY e 'evil'\"><!ENTITY c \"w\">]><a>&c;</a>");
        let ContentEvent::Text(text) = &tree.elements[0].content[0] else {
            panic!("expected text");
        };
        assert_eq!(text, "w");
    }

    #[test]
    fn quoted_system_is_replacement_text() {
        let tree = parse_ok("<!DOCTYPE a [<!ENTITY sys \"SYSTEM\">]><a>&sys;</a>");
        let ContentEvent::Text(text) = &tree.elements[0].content[0] else {
            panic!("expected text");
        };
        assert_eq!(text, "SYSTEM");
    }

    #[test]
    fn dtd_subset_genuinely_unterminated_still_errors() {
        for doc in [
            "<!DOCTYPE r [",
            "<!DOCTYPE r [ <!ENTITY e \"[\">",
            "<!DOCTYPE r [<!ENTITY e \"[\">]<!DOCTYPE q",
        ] {
            let result = XmlParseState::try_new(doc.as_bytes())
                .expect("state")
                .parse(doc.as_bytes());
            assert!(result.is_err(), "must reject unterminated subset: {doc}");
        }
    }

    fn parse_err(input: &str) {
        let result = XmlParseState::try_new(input.as_bytes())
            .expect("state")
            .parse(input.as_bytes());
        assert!(result.is_err(), "must reject: {input}");
    }

    #[test]
    fn xml_stylesheet_pi_in_the_prolog_is_not_a_declaration() {
        let tree = parse_ok(r#"<?xml-stylesheet type="text/xsl" href="s.xsl"?><a/>"#);
        assert_eq!(tree.local_of(tree.elements[tree.root].name), "a");
    }

    #[test]
    fn literal_cdata_end_marker_in_content_is_rejected() {
        parse_err("<a>]]></a>");
        parse_err("<a>x]]>y</a>");
    }

    #[test]
    fn escaped_cdata_end_marker_in_content_is_accepted() {
        let tree = parse_ok("<a>]]&gt;</a>");
        let ContentEvent::Text(text) = &tree.elements[0].content[0] else {
            panic!("expected text");
        };
        assert_eq!(text, "]]>");
    }

    #[test]
    fn duplicate_namespace_declarations_are_rejected() {
        parse_err(r#"<a xmlns:p="u1" xmlns:p="u2"/>"#);
        parse_err(r#"<a xmlns="u1" xmlns="u2"/>"#);
    }

    #[test]
    fn distinct_namespace_declarations_are_accepted() {
        let tree = parse_ok(r#"<a xmlns:p="u1" xmlns:q="u2" xmlns="u3"/>"#);
        assert_eq!(tree.local_of(tree.elements[tree.root].name), "a");
    }

    #[test]
    fn xml_declaration_requires_version_first_and_exactly_1_0() {
        parse_ok(r#"<?xml version="1.0"?><a/>"#);
        parse_ok(r#"<?xml version="1.0" encoding="utf-8"?><a/>"#);
        parse_ok(r#"<?xml version="1.0" encoding="utf-8" standalone="yes"?><a/>"#);
        parse_err(r#"<?xml encoding="utf-8" version="1.0"?><a/>"#);
        parse_err(r#"<?xml standalone="yes" version="1.0"?><a/>"#);
        parse_err(r#"<?xml version="1.0" version="1.0"?><a/>"#);
        parse_err(r#"<?xml version="1."?><a/>"#);
        parse_err(r#"<?xml version="1.1"?><a/>"#);
        parse_err(" <?xml version=\"1.0\"?><a/>");
    }

    #[test]
    fn attribute_value_whitespace_is_normalized_to_space() {
        let tree = parse_ok("<a x=\"p\tq\"/>");
        assert_eq!(tree.elements[0].attributes[0].1, "p q");
        let tree = parse_ok("<a x=\"p\r\nq\"/>");
        assert_eq!(tree.elements[0].attributes[0].1, "p q");
    }

    #[test]
    fn character_data_line_endings_are_normalized_to_lf() {
        let tree = parse_ok("<a>x\r\ny</a>");
        let ContentEvent::Text(text) = &tree.elements[0].content[0] else {
            panic!("expected text");
        };
        assert_eq!(text, "x\ny");
        let tree = parse_ok("<a>x\ry</a>");
        let ContentEvent::Text(text) = &tree.elements[0].content[0] else {
            panic!("expected text");
        };
        assert_eq!(text, "x\ny");
        // CR-free input is an identity (the borrowed path).
        let tree = parse_ok("<a>x\ny</a>");
        let ContentEvent::Text(text) = &tree.elements[0].content[0] else {
            panic!("expected text");
        };
        assert_eq!(text, "x\ny");
    }

    #[test]
    fn comment_double_hyphen_is_rejected() {
        parse_err("<a><!-- -- --></a>");
        parse_err("<a><!-- ---></a>");
        let tree = parse_ok("<a><!-- ok --></a>");
        let ContentEvent::Comment(text) = &tree.elements[0].content[0] else {
            panic!("expected comment");
        };
        assert_eq!(text, " ok ");
    }
}

#[cfg(test)]
mod measure_tests {
    use super::*;

    fn measure_children(input: &str) -> alloc::vec::Vec<MeasureChild> {
        let state = XmlParseState::try_new_measure(input.as_bytes()).expect("measure state");
        let output = state.parse(input.as_bytes()).expect("parse");
        match output {
            ParseOutput::Measure(children) => children,
            _ => panic!("expected measure"),
        }
    }

    #[test]
    fn measure_records_element_children_with_extents() {
        let children = measure_children(r#"<catalog><item id="0"/><item id="1"/></catalog>"#);
        assert_eq!(children.len(), 2);
        let input = r#"<catalog><item id="0"/><item id="1"/></catalog>"#;
        match &children[0] {
            MeasureChild::Element { start, end } => {
                assert_eq!(*start, 9); // at the first '<item'
                assert_eq!(&input[*start..*end], r#"<item id="0"/>"#);
            }
            other => panic!("expected element, got {other:?}"),
        }
    }

    #[test]
    fn measure_coalesces_adjacent_text_and_keeps_leaf_kinds() {
        let children = measure_children(r"<catalog>a <item/> b<!--c--><?pi d?></catalog>");
        // a text child (coalesced whitespace+text), one element, one text, one
        // comment, one PI.
        assert_eq!(children.len(), 5);
        assert!(matches!(&children[0], MeasureChild::Text(t) if t == "a "));
        assert!(matches!(children[1], MeasureChild::Element { .. }));
        assert!(matches!(&children[2], MeasureChild::Text(t) if t == " b"));
        assert!(matches!(&children[3], MeasureChild::Comment(t) if t == "c"));
        assert!(matches!(
            &children[4],
            MeasureChild::ProcessingInstruction { target, data } if target == "pi" && data == "d"
        ));
    }
}

/// The alignment oracle for the stop sets this module owns: the wide kernel
/// must agree with each set's scalar predicate at every alignment and
/// length, so a wrong kernel is a test failure here.
#[cfg(test)]
mod stop_set_oracle_tests {
    use super::{StopSet, Xml, XmlAttrValue, XmlCharInvalid, prefix_len};
    use alloc::vec;
    use alloc::vec::Vec;

    fn check_alignment<S: StopSet>(bytes: &[u8]) {
        for start in 0..=bytes.len().min(3) {
            for end in start..=bytes.len().min(start + 48) {
                let slice = &bytes[start..end];
                assert_eq!(
                    prefix_len::<S>(slice),
                    slice.iter().take_while(|b| !S::stop(**b)).count(),
                    "{} mismatch at {start}..{end} of {bytes:?}",
                    core::any::type_name::<S>(),
                );
            }
        }
    }

    #[test]
    fn stop_sets_agree_with_their_scalar_predicates_at_every_alignment() {
        let mut corpus: Vec<Vec<u8>> = vec![
            Vec::new(),
            b"a".to_vec(),
            b"\"".to_vec(),
            b"<tag>&amp;</tag>".to_vec(),
            b"\x00\x1f\x7f\xef\xf0".to_vec(),
        ];
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mix = |state: &mut u64| {
            *state ^= *state << 13;
            *state ^= *state >> 7;
            *state ^= *state << 17;
            *state
        };
        for len in 0..48 {
            let mut bytes = Vec::with_capacity(len);
            for _ in 0..len {
                let r = mix(&mut state);
                bytes.push(match r % 4 {
                    0 => b"<&'\"\x00\x1f\xef"[((r >> 8) % 7) as usize],
                    _ => ((r >> 16) & 0xFF) as u8,
                });
            }
            corpus.push(bytes);
        }
        for bytes in &corpus {
            check_alignment::<Xml>(bytes);
            check_alignment::<XmlAttrValue>(bytes);
            check_alignment::<XmlCharInvalid>(bytes);
        }
    }
}
