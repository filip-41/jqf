//! One-item encoder sessions writing through a bounded byte sink.
//!
//! [`ByteSink::write_all`] retries partial writes and refuses zero or over-reported progress. Sibling:
//! [`crate::project`] for non-native scalars.

use core::any::Any;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use jqf_data::{Document, NodeId, Value};
use jqf_resource::ResourceContext;

use crate::{
    CodecError, CodecFailureKind, CodecRunContext, EncodeItem, PhysicalRouteId, PreservationReport, PreservationRequest,
};

use crate::execution::SessionTerminal;

/// Bounded-buffer byte sink for straight-line encode.
///
/// An encoder writes its output through this sink as it produces it, and the sink owns publication in bounded chunks
/// (the SDK adapter caps each host write at 16 KiB). A write may accept fewer bytes than offered — the caller must
/// retry the remainder — which is the bounded buffer's backpressure surface.
pub trait ByteSink {
    /// Attempts to publish `bytes` against the request's resources, returning the number accepted (possibly less than
    /// the input length; the caller retries the remainder).
    ///
    /// The resources travel per call rather than being held by the sink so a straight-line encode can borrow them
    /// through its [`CodecRunContext`] and the sink side at once.
    ///
    /// # Errors
    ///
    /// Returns a codec error when publication fails (host sink, resource ceiling, or control).
    fn write(&mut self, bytes: &[u8], resources: &mut jqf_resource::ResourceContext<'_>) -> Result<usize, CodecError>;

    /// Publishes any bytes still buffered by the sink after the encoder finished.
    ///
    /// # Errors
    ///
    /// Returns a codec error when publication fails.
    fn flush(&mut self) -> Result<(), CodecError>;

    /// Writes all of `bytes`, retrying partial writes until every byte is accepted. The bounded-buffer backpressure
    /// surface: a sink that accepts less than offered is retried with the remainder.
    ///
    /// # Errors
    ///
    /// Returns a codec error when publication fails, or an internal-contract violation when the sink reports zero
    /// progress or more bytes than offered.
    fn write_all(
        &mut self,
        mut bytes: &[u8],
        resources: &mut jqf_resource::ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        while !bytes.is_empty() {
            let written = self.write(bytes, resources)?;
            if written == 0 {
                return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                    contract: "byte sink made no progress",
                }));
            }
            if written > bytes.len() {
                return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                    contract: "byte sink over-reported",
                }));
            }
            bytes = &bytes[written..];
        }
        Ok(())
    }
}

/// A [`ByteSink`] appending into a caller-owned byte vector.
///
/// Production sink over a caller-owned vector. Accepts every byte immediately. Growth is not charged; the caller owns
/// the vector.
pub struct VecByteSink<'a> {
    target: &'a mut Vec<u8>,
}

impl<'a> VecByteSink<'a> {
    /// Binds the sink to one caller-owned output vector.
    #[must_use]
    pub fn new(target: &'a mut Vec<u8>) -> Self {
        Self { target }
    }
}

impl ByteSink for VecByteSink<'_> {
    fn write(&mut self, bytes: &[u8], _resources: &mut jqf_resource::ResourceContext<'_>) -> Result<usize, CodecError> {
        self.target.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> Result<(), CodecError> {
        Ok(())
    }
}

/// Downstream state for one straight-line encoded item.
pub trait EncoderSession: Any {
    /// Encodes the whole item to completion in one call, publishing bytes through `sink` as they are produced. The
    /// encoder replenishes its own cooperative work budget at its loop heads ([`CodecRunContext::replenish_work`])
    /// instead of yielding an offer.
    fn encode(
        &mut self,
        item: EncodeItem<'_, '_>,
        sink: &mut dyn ByteSink,
        context: &mut CodecRunContext<'_, '_>,
    ) -> Result<PreservationReport, CodecError>;

    /// Downcasts to the concrete encoder state for the ordered-emission reuse hook. The concrete `&mut Self` coercion
    /// records the concrete type id — a trait-object upcast to `dyn Any` would record the erased trait's id instead,
    /// which is what the hand-rolled vtables' runtime downcasts needed an error path to detect.
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

/// Target-bound downstream encoder factory.
pub trait EncoderFactoryImpl: Any {
    /// Stable physical identity of sessions started by this factory.
    fn physical_encoder(&self) -> PhysicalRouteId;

    /// Whether this factory, under the options it was constructed with, emits the dialect's canonical spelling of a
    /// document.
    ///
    /// The source-echo lane consults this instead of reading JSON formatting flags off the request: indent, sort, and
    /// ascii-escape rewrite bytes the echo would publish. The default is `true` — every other format's
    /// identity-encode dialect. JSON overrides.
    fn emits_canonical_form(&self) -> bool {
        true
    }
    /// Starts exactly one item session without materializing located authority.
    fn start<'item, 'source>(
        &self,
        item: EncodeItem<'item, 'source>,
        preservation: PreservationRequest,
        resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedEncoderSession<'item, 'source>, CodecError>;

    /// Reinitializes a recycled session state of this factory's own concrete type for one more ordered item.
    ///
    /// This is the ordered-emission reuse hook behind [`crate::ErasedEncoderFactory::start_reusing`]: a stream of
    /// emissions (an adjacent-value sequence, or one document's fan-out) otherwise allocates and drops a complete
    /// encoder — output staging buffer included — per published item. The reset must leave exactly the state a
    /// fresh [`Self::start`] would have produced for `item`, so a prior item that failed partway through its encode
    /// cannot leak bytes or frames into the next one.
    ///
    /// Returning `Ok(false)` means "cannot recycle for this item"; the caller then starts a fresh session, so declining
    /// is always safe. The default declines, leaving every existing encoder's lifecycle untouched.
    ///
    /// # Errors
    ///
    /// Returns a codec error only when the reset itself fails.
    fn try_restart(
        &self,
        state: &mut crate::RecycledSessionState<'_>,
        item: EncodeItem<'_, '_>,
        preservation: PreservationRequest,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        let _ = (state, item, preservation, resources);
        Ok(false)
    }

    /// Renders one owned value exactly as it would appear in VALUE position of a document — a leaf, never a complete
    /// document. This is the source-preserving patch lanes' seam: an edit that replaces a leaf's authored span
    /// publishes these bytes in its place.
    ///
    /// The leaf is rendered with its NODE CONTEXT — the document, the leaf's own node, its value path, and the
    /// retained source segment — because leaf escaping is position-dependent in the markup formats (an XML attribute
    /// value and a text node escape differently, a Lua string literal's quoting depends on its surroundings). The codec
    /// reads what it needs from `document`/`node` (its own schema and spans) and `source`; the earlier value-only
    /// signature could not tell an attribute leaf from a text leaf, so a position-aware codec could never implement it
    /// correctly.
    ///
    /// `authored` is the RETAINED SOURCE BYTES of the patch site (the span being replaced), passed so a codec can
    /// preserve the site's authored spelling — a YAML plain/single/double-quoted scalar, a TOML literal-vs-basic
    /// string. `None` means the site has no authored span (a non-edit caller, or a value the diff could not name a span
    /// for); the codec then uses its default spelling. The SDK passes bytes, never a parsed style — what a quote
    /// means is a format fact.
    ///
    /// The default DECLINES ([`CodecFailureKind::UnsupportedRepresentation`]): most codecs have no standalone value
    /// grammar (a TOML document must be a table), and the caller then falls back to encoding the value as a complete
    /// document — correct for JSON, where every value is a document.
    ///
    /// # Errors
    ///
    /// Returns [`CodecFailureKind::UnsupportedRepresentation`] to decline; any other error is a genuine failure of the
    /// value itself.
    #[expect(
        clippy::too_many_arguments,
        reason = "the leaf seam carries the node context and the authored span the position-aware codecs render from"
    )]
    fn render_leaf(
        &self,
        document: &Document<'_>,
        node: NodeId,
        path: &[String],
        source: &[u8],
        value: &Value,
        authored: Option<&[u8]>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Vec<u8>, CodecError> {
        let _ = (document, node, path, source, value, authored, resources);
        Err(CodecError::new(CodecFailureKind::UnsupportedRepresentation))
    }

    /// Renders ONE fact write as byte patches against the node's authored span, when this codec carries the role: the
    /// role-carrying multi-patch seam, and the one fact-write hook.
    ///
    /// The codec owns the placement AND the validity check: it reads the node's span and payload from
    /// `document`/`node`, validates `payload` against its own grammar, and returns the exact patches. A write the codec
    /// cannot honor — a tag that would not re-decode, a style its renderer does not emit, an alias naming no anchor,
    /// a comment payload that would change the document value — is a refusal error here with a prose message, never a
    /// silent drop (the encode-or-report-a-loss law). The return is `Ok(Some(patches))` for a write the codec serves
    /// — zero or more [`FactEditPatch`]es, an EMPTY vec for a legitimate no-op write (a deletion over a comment-less
    /// node) — and `Ok(None)` when this codec does not carry the role at all; the caller refuses `None` with its own
    /// message naming the format and the role. The role vocabulary is the 144 comment positions (`comment`,
    /// `comment_inline`, `comment_foot`), the markup attribute role, plus the metadata roles
    /// (`style`/`tag`/`anchor`/`alias`); each codec's impl dispatches on the roles its format carries and declines the
    /// rest. `kind` is the fact kind: the attribute selector for a markup attribute write, empty for comment and
    /// metadata roles.
    ///
    /// # Errors
    ///
    /// Returns a codec error with a diagnostic message when the write cannot be honored.
    #[expect(
        clippy::too_many_arguments,
        reason = "the fact-write seam carries the node, role, kind, and payload the codec validates together"
    )]
    fn render_fact_delta(
        &self,
        document: &Document<'_>,
        node: NodeId,
        source: &[u8],
        role: &str,
        kind: &str,
        payload: &Value,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<alloc::vec::Vec<FactEditPatch>>, CodecError> {
        let _ = (document, node, source, role, kind, payload, resources);
        Ok(None)
    }

    /// Renders the splice for a container the edit lane GREW — the source-edit lane's structural seam, beside
    /// [`Self::render_leaf`]'s leaf seam. A program that adds a member to a table or an item to an array has no
    /// authored span to patch; the format-neutral diff walk asks the codec for the bytes that append the added members
    /// in the format's LOCAL syntax, at a position the codec itself names (its splice policy knows where a new
    /// statement, section, or array item belongs).
    ///
    /// The codec receives the ORIGINAL document (spans, roles, comment facts), the grown container's node, its value
    /// path, and the source segment, plus the added members in their new authored order. It returns zero or more
    /// ordered insertions; the caller splices each at its `at` offset and re-verifies the patched bytes by re-decoding,
    /// so a declined or wrong splice falls back to the whole-document floor rather than corrupting the file.
    ///
    /// The default DECLINES (an empty vec): most codecs have no local spelling for a new container member beyond a
    /// whole-document re-encode, which the caller then performs.
    ///
    /// # Errors
    ///
    /// Returns a codec error only when the members themselves are unrepresentable in the format's local syntax; the
    /// caller maps that to the same failure the whole-document encoder would raise.
    fn render_edit_append(
        &self,
        document: &Document<'_>,
        container: NodeId,
        path: &[String],
        source: &[u8],
        members: EditAppendMembers<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<alloc::vec::Vec<EditInsertion>, CodecError> {
        let _ = (document, container, path, source, members, resources);
        Ok(alloc::vec::Vec::new())
    }

    /// Renders the cuts for a container the edit lane SHRANK — the mirror of [`Self::render_edit_append`]. A program
    /// that deletes a table member or an array item removes authored bytes that no leaf patch addresses: the key, its
    /// value, its punctuation, its line, and the comment lines the format attaches to it. Only the codec knows where a
    /// statement starts and ends, so the format-neutral diff walk asks it for the byte ranges to cut.
    ///
    /// The codec receives the ORIGINAL document, the shrunk container's node, its value path, the source segment, and
    /// the removed members with the original node of each. It returns zero or more removals; the caller applies each as
    /// an empty-replacement patch and re-verifies by re-decoding, so a declined or wrong cut falls back to the
    /// whole-document floor rather than corrupting the file.
    ///
    /// The default DECLINES (an empty vec), exactly as the append seam does.
    ///
    /// # Errors
    ///
    /// Returns a codec error only when the container's own source shape contradicts the document; the caller maps that
    /// to the same failure the whole-document encoder would raise.
    fn render_edit_remove(
        &self,
        document: &Document<'_>,
        container: NodeId,
        path: &[String],
        source: &[u8],
        members: EditRemoveMembers<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<alloc::vec::Vec<EditRemoval>, CodecError> {
        let _ = (document, container, path, source, members, resources);
        Ok(alloc::vec::Vec::new())
    }

    /// Renders the key-rename splices for a container whose members RENAMED, at any byte-length change. A SAME-length
    /// key overwrites the old key's authored span in place, so the member's comments, its value, and every untouched
    /// byte survive verbatim; a DIFFERENT-length key splices the same span at the new length and shifts the following
    /// bytes — the new key goes where the old key was, and the comment-follows-key ruling (the comment belongs to the
    /// entry, never to the line) keeps the entry's comments attached through the splice, since nothing around the key
    /// token moves. The diff walk recognizes the rename and asks the codec for the byte replacements — only the codec
    /// knows where a key token starts and ends in its own grammar, and how the new text renders in its spelling (bare,
    /// quoted).
    ///
    /// Each returned [`EditReplacement`] names the authored key region `[at, at + region_len)` and its replacement
    /// `bytes`. The caller re-verifies by re-decoding, so a wrong replacement falls back to the whole-document floor
    /// rather than corrupting the file.
    ///
    /// The default DECLINES (an empty vec), exactly as the append and remove seams do: a codec that cannot name the key
    /// tokens in place leaves the rename to the whole-document floor.
    ///
    /// # Errors
    ///
    /// Returns a codec error only when the container's own source shape contradicts the document; the caller maps that
    /// to the same failure the whole-document encoder would raise.
    fn render_edit_rename(
        &self,
        document: &Document<'_>,
        container: NodeId,
        path: &[String],
        source: &[u8],
        members: EditRenameMembers<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<alloc::vec::Vec<EditReplacement>, CodecError> {
        let _ = (document, container, path, source, members, resources);
        Ok(alloc::vec::Vec::new())
    }
}

/// The members a grown container gained, in their new authored order. The diff walk splits by container kind: a TABLE
/// grows by (key, value) members, an ARRAY by items.
#[derive(Clone, Copy, Debug)]
pub enum EditAppendMembers<'a> {
    /// A table grew: the added (key, value) members, in the new object's insertion order.
    Table(&'a [(&'a str, &'a Value)]),
    /// An array grew: the added items, in the new array's order.
    Array(&'a [&'a Value]),
}

/// One byte-range splice the edit lane must apply to the source segment: the replacement bytes to insert at `at` (a
/// zero-length span — the caller splices them in at `at`).
///
/// A codec whose local syntax must REWRITE an authored span — a binary length header whose count changed — names
/// that span in `replace`, and the caller then replaces `[replace.0, replace.1)` with `bytes` instead of only growing
/// the segment. `at` must equal `replace.0`; the field exists so the pure-insertion callers keep the at-only shape. The
/// text codecs never set it; the binary splice pattern is its first consumer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EditInsertion {
    /// The byte offset into the source segment at which the splice lands.
    pub at: usize,
    /// The bytes to insert at `at` (or, with `replace`, to write in its place).
    pub bytes: alloc::vec::Vec<u8>,
    /// The authored span to REPLACE with [`Self::bytes`], when the splice rewrites existing bytes rather than only
    /// growing the segment. `None` (the text codecs' shape) is a pure insertion at [`Self::at`].
    pub replace: Option<(usize, usize)>,
}

/// The members a shrunk container lost, each with the node it occupied in the ORIGINAL document — the codec reads
/// that node's retained span to find the authored bytes to cut.
#[derive(Clone, Copy, Debug)]
pub enum EditRemoveMembers<'a> {
    /// A table shrank: the removed (key, original value node) members, in the original document's order.
    Table(&'a [(&'a str, NodeId)]),
    /// An array shrank: the removed (original index, original item node) members, in the original array's order.
    Array(&'a [(usize, NodeId)]),
}

impl EditRemoveMembers<'_> {
    /// The removed members' original document nodes, in order — what a codec whose removal policy is the same for
    /// tables and arrays needs.
    #[must_use]
    pub fn nodes(self) -> alloc::vec::Vec<NodeId> {
        match self {
            Self::Table(members) => members.iter().map(|(_, node)| *node).collect(),
            Self::Array(items) => items.iter().map(|(_, node)| *node).collect(),
        }
    }
}

/// One byte range the edit lane must cut from the source segment.
///
/// A codec whose local syntax must REWRITE the cut span — a binary length header rewritten for a smaller count —
/// fills [`Self::replacement`], and the caller replaces the span with those bytes instead of removing them. The text
/// codecs leave it empty (a pure cut); the binary splice pattern is its first consumer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EditRemoval {
    /// The first byte of the cut, as an offset into the source segment.
    pub start: usize,
    /// One past the last byte of the cut.
    pub end: usize,
    /// The bytes to write in the cut's place, when the removal is a replacement rather than a pure cut. Empty for a
    /// pure cut.
    pub replacement: alloc::vec::Vec<u8>,
}

/// The members a TABLE renamed: the (old key, new key) pairs, in the new object's insertion order. Only tables rename
/// — an array has no keys — so this is the pair list itself rather than an enum split by container kind.
#[derive(Clone, Copy, Debug)]
pub struct EditRenameMembers<'a>(pub &'a [(&'a str, &'a str)]);

/// One key-rename splice the edit lane must apply to the source segment: the authored key region `[at, at +
/// region_len)` is replaced with `bytes`. The codec NAMES the exact region it is replacing (its key token's authored
/// extent) and CONSTRUCTS the replacement bytes for the new key. The two lengths agree only for the SAME-length half,
/// where the caller applies the patch as an in-place overwrite that moves nothing; a DIFFERENT-length rename splices
/// the region shorter or longer and shifts the following bytes, keeping the entry — and its comments, which follow
/// the key by ruling — in place.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EditReplacement {
    /// The byte offset into the source segment at which the authored key region begins.
    pub at: usize,
    /// The byte length of the authored key region being replaced.
    pub region_len: usize,
    /// The replacement bytes, any length (equal to `region_len` only on the same-length half).
    pub bytes: alloc::vec::Vec<u8>,
}

/// One byte patch a codec proposes for a metadata fact write: the node's authored span `[start, end)` becomes
/// `replacement`. An insert-before-value write (`.@anchor`, `.@tag`) is a zero-length span at the value's start; a
/// re-render or alias write (`.@style`, `.@alias`) replaces the whole span. The codec constructs both the placement and
/// the bytes, so the position law stays a format fact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FactEditPatch {
    /// The byte offset into the source segment where the patch begins.
    pub start: usize,
    /// The byte offset into the source segment where the patch ends (equal to `start` for a pure insertion).
    pub end: usize,
    /// The replacement bytes.
    pub replacement: alloc::vec::Vec<u8>,
}

/// The whole-line cut for one member of a LINE-ORIENTED container: from the start of the comment block above the
/// member's first line through the line break that ends its subtree's last line.
///
/// This is byte arithmetic, not policy. WHETHER a container's members occupy whole lines, and whether `#` opens a
/// comment, is each codec's own question; the codecs that answer yes for both share this one spelling of the cut
/// instead of copying it.
///
/// The comment block is the codecs' comment-ownership law read in reverse: the comment and blank lines directly above a
/// member are that member's, so they leave with it. A member whose last line carries anything but whitespace after its
/// subtree — a trailing comment, which the decoders attach to the NEXT member — returns `None`: cutting the line
/// would take a comment that is not this member's, and the caller falls back to the whole-document floor rather than
/// guess.
#[must_use]
pub fn line_statement_cut(source: &[u8], span: jqf_source::Span) -> Option<EditRemoval> {
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
    let mut start = line_start(span_start);
    while start > 0 {
        let previous = line_start(start - 1);
        let line = &source[previous..start];
        if line.trim_ascii().is_empty() || line.trim_ascii_start().starts_with(b"#") {
            start = previous;
        } else {
            break;
        }
    }
    // A string's span is codec-specific: JSON, TOML, and YAML record a QUOTED scalar's span as its INNER content, so
    // the closing quote sits one byte past the span end. The same convention detection the leaf patcher runs — a
    // matching quote pair on both edges — steps the cut over it.
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

/// A caller-held recycled encoder state for a stream of ordered items.
///
/// One instance is carried across a whole publication stream. Each successive
/// [`crate::ErasedEncoderFactory::start_reusing`] reinitializes the retained concrete encoder — its frame stack and
/// its output staging buffer — instead of allocating one per emission, and each completed item hands the state back
/// with [`ErasedEncoderSession::recycle`].
///
/// The retained state stays live for as long as this slot lives, and there is one of it, never one per published item.
#[derive(Default)]
pub struct ReusableEncoderSession {
    state: Option<Box<dyn EncoderSession>>,
}

impl core::fmt::Debug for ReusableEncoderSession {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("ReusableEncoderSession")
            .field("recycled", &self.state.is_some())
            .finish()
    }
}

impl ReusableEncoderSession {
    /// An empty slot; the first item starts the session it then recycles.
    #[must_use]
    pub const fn new() -> Self {
        Self { state: None }
    }

    /// Drops the recycled encoder state, releasing its retained charge before the request ends.
    pub fn release(&mut self) {
        self.state = None;
    }
}

impl crate::ErasedEncoderFactory {
    /// Constructs a checked target-bound downstream encoder factory.
    pub fn try_new_factory<T, F>(
        preservation: PreservationRequest,
        _resources: &ResourceContext<'_>,
        constructor: F,
    ) -> Result<Self, CodecError>
    where
        T: EncoderFactoryImpl,
        F: FnOnce() -> Result<T, CodecError>,
    {
        let mut factory = Self::try_new_with(constructor)?;
        factory.diagnostics_checked = true;
        factory.preservation = Some(preservation);
        Ok(factory)
    }

    /// Renders one owned value as a leaf in value position, when this codec has a standalone value grammar. See
    /// [`EncoderFactoryImpl::render_leaf`] for the decline contract. `authored` carries the retained source bytes of
    /// the patch site (see the trait method's docs).
    ///
    /// # Errors
    ///
    /// Returns [`CodecFailureKind::UnsupportedRepresentation`] when the codec cannot render a bare value; the caller
    /// falls back to the whole-document encoder.
    #[expect(
        clippy::too_many_arguments,
        reason = "the leaf seam carries the node context and the authored span the position-aware codecs render from"
    )]
    pub fn render_leaf(
        &self,
        document: &Document<'_>,
        node: NodeId,
        path: &[String],
        source: &[u8],
        value: &Value,
        authored: Option<&[u8]>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Vec<u8>, CodecError> {
        self.owner
            .render_leaf(document, node, path, source, value, authored, resources)
    }

    /// Renders one fact write as byte patches, when this codec carries the role. See
    /// [`EncoderFactoryImpl::render_fact_delta`] for the decline contract.
    ///
    /// # Errors
    ///
    /// Returns a codec error with a diagnostic message when the write cannot be honored.
    #[expect(clippy::too_many_arguments, reason = "mirrors EncoderFactoryImpl::render_fact_delta")]
    pub fn render_fact_delta(
        &self,
        document: &Document<'_>,
        node: NodeId,
        source: &[u8],
        role: &str,
        kind: &str,
        payload: &Value,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<alloc::vec::Vec<FactEditPatch>>, CodecError> {
        self.owner
            .render_fact_delta(document, node, source, role, kind, payload, resources)
    }

    /// Renders the structural-append splice for a grown container, when this codec has a local syntax for new members.
    /// See [`EncoderFactoryImpl::render_edit_append`] for the decline contract.
    ///
    /// # Errors
    ///
    /// Returns a codec error only when the members are unrepresentable in the format's local syntax.
    pub fn render_edit_append(
        &self,
        document: &Document<'_>,
        container: NodeId,
        path: &[String],
        source: &[u8],
        members: EditAppendMembers<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<alloc::vec::Vec<EditInsertion>, CodecError> {
        let insertions = self
            .owner
            .render_edit_append(document, container, path, source, members, resources)?;
        // Every production splice consumer reads insertions through this wrapper, so this is the one borrow site that
        // sees them all: a rewriting insertion (`replace`) must name its span's START as `at`, or the caller would
        // splice the replacement at an offset other than the authored bytes it believes it is replacing.
        if cfg!(debug_assertions) {
            for insertion in &insertions {
                if let Some((start, _)) = insertion.replace {
                    debug_assert_eq!(
                        start, insertion.at,
                        "a replacing EditInsertion must set at == replace.0"
                    );
                }
            }
        }
        Ok(insertions)
    }

    /// Renders the cuts for a shrunk container, when this codec knows where its statements begin and end. See
    /// [`EncoderFactoryImpl::render_edit_remove`] for the decline contract.
    ///
    /// # Errors
    ///
    /// Returns a codec error only when the container's source shape contradicts the document.
    pub fn render_edit_remove(
        &self,
        document: &Document<'_>,
        container: NodeId,
        path: &[String],
        source: &[u8],
        members: EditRemoveMembers<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<alloc::vec::Vec<EditRemoval>, CodecError> {
        self.owner
            .render_edit_remove(document, container, path, source, members, resources)
    }

    /// Renders the in-place key overwrites for a renamed container, when this codec can name the key tokens in place.
    /// See [`EncoderFactoryImpl::render_edit_rename`] for the decline contract.
    ///
    /// # Errors
    ///
    /// Returns a codec error only when the container's source shape contradicts the document.
    pub fn render_edit_rename(
        &self,
        document: &Document<'_>,
        container: NodeId,
        path: &[String],
        source: &[u8],
        members: EditRenameMembers<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<alloc::vec::Vec<EditReplacement>, CodecError> {
        self.owner
            .render_edit_rename(document, container, path, source, members, resources)
    }

    /// Starts one checked encoder session borrowing the supplied item.
    pub fn start<'item, 'source>(
        &self,
        item: EncodeItem<'item, 'source>,
        preservation: PreservationRequest,
        resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedEncoderSession<'item, 'source>, CodecError> {
        if self.preservation != Some(preservation) || !self.diagnostics_checked {
            return Err(CodecError::new(CodecFailureKind::RequirementMismatch));
        }
        let mut session = self.owner.start(item, preservation, resources)?;
        session.seal_physical_encoder(self.owner.physical_encoder())?;
        Ok(session)
    }

    /// Starts one checked encoder session over the state RECYCLED from the previous ordered item instead of a freshly
    /// allocated one.
    ///
    /// A publication stream otherwise allocates and drops one complete encoder per emission, output staging buffer
    /// included. `reuse` carries that state across items: the concrete encoder is reset in place through
    /// [`EncoderFactoryImpl::try_restart`], and the completed session hands it back with
    /// [`ErasedEncoderSession::recycle`].
    ///
    /// The recycled session is otherwise the fresh session exactly — a cursor positioned at the item's root, no
    /// terminal, no report — so it publishes byte-identical output. A factory that declines to recycle (the default)
    /// simply gets a fresh session.
    ///
    /// # Errors
    ///
    /// Returns the same failures as [`Self::start`]. A restart whose reset itself fails releases the slot's retained
    /// state before the error propagates: a reset that failed partway leaves the concrete encoder neither fresh nor
    /// intact, so the next attempt starts a fresh session instead of reusing half-reset state.
    pub fn start_reusing<'item, 'source>(
        &self,
        item: EncodeItem<'item, 'source>,
        preservation: PreservationRequest,
        resources: &mut ResourceContext<'_>,
        reuse: &mut ReusableEncoderSession,
    ) -> Result<ErasedEncoderSession<'item, 'source>, CodecError> {
        if self.preservation != Some(preservation) || !self.diagnostics_checked {
            return Err(CodecError::new(CodecFailureKind::RequirementMismatch));
        }
        let restarted = match reuse.state.as_mut() {
            Some(state) => self.owner.try_restart(
                &mut crate::RecycledSessionState::new(state.as_any_mut()),
                item,
                preservation,
                resources,
            ),
            None => Ok(false),
        };
        let restarted = match restarted {
            Ok(restarted) => restarted,
            // The borrow of `reuse` ends with the match above, so the slot can be dropped here; see this method's error
            // contract.
            Err(error) => {
                reuse.release();
                return Err(error);
            }
        };
        let recycled = restarted.then(|| reuse.state.take()).flatten();
        let mut session = if let Some(state) = recycled {
            ErasedEncoderSession::try_recycled(item, preservation, state)
        } else {
            reuse.release();
            self.owner.start(item, preservation, resources)?
        };
        session.seal_physical_encoder(self.owner.physical_encoder())?;
        Ok(session)
    }
}

/// Core-owned straight-line encoder session for exactly one borrowed item.
pub struct ErasedEncoderSession<'item, 'source> {
    item: EncodeItem<'item, 'source>,
    state: Box<dyn EncoderSession>,
    terminal: Option<SessionTerminal>,
    terminal_error: Option<CodecError>,
    report: Option<PreservationReport>,
    preservation: PreservationRequest,
    physical_encoder: PhysicalRouteId,
}

impl core::fmt::Debug for ErasedEncoderSession<'_, '_> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("ErasedEncoderSession")
            .field("terminal", &self.terminal)
            .finish_non_exhaustive()
    }
}

impl<'item, 'source> ErasedEncoderSession<'item, 'source> {
    /// Constructs concrete state while retaining only borrowed item authority.
    pub fn try_new<T, F>(
        item: EncodeItem<'item, 'source>,
        preservation: PreservationRequest,
        constructor: F,
    ) -> Result<Self, CodecError>
    where
        T: EncoderSession,
        F: FnOnce() -> Result<T, CodecError>,
    {
        let state: Box<dyn EncoderSession> = Box::new(constructor()?);
        Ok(Self {
            item,
            state,
            terminal: None,
            terminal_error: None,
            report: None,
            preservation,
            physical_encoder: PhysicalRouteId::UNSPECIFIED,
        })
    }

    /// Rebuilds one session around a state the previous ordered item already reset, keeping its retained workspaces and
    /// its concrete projections.
    ///
    /// Everything that identifies ONE item — the item authority, its cursor, the terminal, and the preservation
    /// report — starts over exactly as [`Self::try_new`] starts it.
    fn try_recycled(
        item: EncodeItem<'item, 'source>,
        preservation: PreservationRequest,
        state: Box<dyn EncoderSession>,
    ) -> Self {
        Self {
            item,
            state,
            terminal: None,
            terminal_error: None,
            report: None,
            preservation,
            physical_encoder: PhysicalRouteId::UNSPECIFIED,
        }
    }

    /// Hands this completed session's concrete state back to `reuse` so the next ordered item restarts it instead of
    /// allocating a new encoder.
    ///
    /// The item authority, cursor, and every per-item counter are dropped here; only the state carrier and its concrete
    /// projections survive, and the factory's [`EncoderFactoryImpl::try_restart`] resets that state before the next
    /// item ever observes it.
    pub fn recycle(self, reuse: &mut ReusableEncoderSession) {
        reuse.state = Some(self.state);
    }

    fn seal_physical_encoder(&mut self, identity: PhysicalRouteId) -> Result<(), CodecError> {
        if self.physical_encoder != PhysicalRouteId::UNSPECIFIED || identity == PhysicalRouteId::UNSPECIFIED {
            return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                contract: "invalid physical encoder identity",
            }));
        }
        self.physical_encoder = identity;
        Ok(())
    }

    /// Returns the core-sealed physical encoder identity executed by this session.
    #[must_use]
    pub const fn physical_encoder(&self) -> PhysicalRouteId {
        self.physical_encoder
    }

    /// Encodes the whole item in one straight-line call, publishing bytes through the bounded `sink` as the concrete
    /// encoder produces them. A failed encode is terminal and the bytes already written to the sink stand (the caller's
    /// prefix-keep law).
    pub fn encode(
        &mut self,
        sink: &mut dyn ByteSink,
        context: &mut CodecRunContext<'_, '_>,
    ) -> Result<PreservationReport, CodecError> {
        if let Some(terminal) = self.terminal {
            return match terminal {
                SessionTerminal::Failed(_) => self.retry_failure(),
                SessionTerminal::Complete | SessionTerminal::Aborted => {
                    Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                        contract: "encoder session already terminal",
                    }))
                }
            };
        }
        if let Err(error) = context.resources().check_control() {
            return self.retain_failure(CodecError::from(error));
        }
        let report = match self.state.encode(self.item, sink, context) {
            Ok(report) => report,
            Err(error) => return self.retain_failure(error),
        };
        if self.preservation == PreservationRequest::Report {
            self.report = Some(report);
        }
        self.terminal = Some(SessionTerminal::Complete);
        Ok(report)
    }

    fn retry_failure<T>(&self) -> Result<T, CodecError> {
        Err(self
            .terminal_error
            .as_ref()
            .ok_or_else(|| {
                CodecError::new(CodecFailureKind::InternalContractViolation {
                    contract: "encoder failed without retained error",
                })
            })?
            .clone())
    }

    fn retain_failure<T>(&mut self, error: CodecError) -> Result<T, CodecError> {
        self.terminal = Some(SessionTerminal::Failed(error.kind()));
        self.terminal_error = Some(error);
        self.retry_failure()
    }

    /// Final per-item preservation report.
    ///
    /// Availability is gated TWICE: the session must have completed its encode, and the session must have been started
    /// under [`PreservationRequest::Report`] — only then does [`Self::encode`] retain the report. A completed session
    /// built under [`PreservationRequest::None`] returns `None`, because no report was ever computed for it; a failed
    /// or unstarted session also returns `None`.
    #[must_use]
    pub const fn report(&self) -> Option<&PreservationReport> {
        self.report.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::resources;

    struct OverReport;

    impl ByteSink for OverReport {
        fn write(&mut self, _bytes: &[u8], _resources: &mut ResourceContext<'_>) -> Result<usize, CodecError> {
            Ok(usize::MAX)
        }

        fn flush(&mut self) -> Result<(), CodecError> {
            Ok(())
        }
    }

    #[test]
    fn write_all_rejects_an_over_reported_write() {
        let mut resources = resources();
        let error = OverReport.write_all(b"ab", &mut resources).expect_err("over-report");
        assert!(matches!(
            error.kind(),
            CodecFailureKind::InternalContractViolation { contract } if contract == "byte sink over-reported"
        ));
    }
}
