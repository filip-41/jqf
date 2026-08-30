//! The YAML event parser: tokens to graph events.
//!
//! A faithful port of the reference parser's explicit state machine: a state stack replaces recursion, so
//! document/collection nesting of any depth parses iteratively. The parser emits one graph event per state-machine
//! step; the session drives it against the scanner's token stream.
//!
//! The grammar is the reference's:
//!
//! ```text
//! stream            ::= STREAM-START implicit_document? explicit_document* STREAM-END
//! implicit_document ::= block_node DOCUMENT-END*
//! explicit_document ::= DIRECTIVE* DOCUMENT-START block_node? DOCUMENT-END*
//! ```
//!
//! The `?`/`:` key/value tokens, block collections, flow collections, and indentless sequences all follow the reference
//! productions. Events carry the anchor/tag properties of their node (the parser resolves tag handles against the
//! document's `%TAG` directives here, so the graph stores exact resolved tag text).

use alloc::borrow::ToOwned;
use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;

use jqf_codec_core::{
    AccessInput, AccessOutcome, AccessResult, AccessSession, CodecError, CodecFailureKind, CodecRunContext,
    DocumentProduct,
};
use jqf_data::{
    AccountedDocumentBuilder, AccountedDocumentFinalizer, DocumentFinalizationPoll, DocumentSourceBindingPoll,
    DocumentSourceBindingStage,
};
use jqf_resource::ResourceContext;
use jqf_source::{ResolvedSource, Span};

use crate::anchor;
use crate::document::data_contract;
use crate::error;
use crate::graph::{NodeId, NodeKind, TextRef, YamlGraph, YamlNode};
use crate::key::{KeyEquality, Verdict};
use crate::scan::{ScalarStyle, ScanPoll, ScannedScalar, Scanner, Token, TokenKind};
use jqf_data::NodeId as DocumentNodeId;

/// Core-string text of a merge key, after alias follow. Integer/float/bool/null keys stay out of the string set so `1`
/// and `01` still run full equality.
fn merge_core_string_text<'a>(
    graph: &'a YamlGraph,
    key: NodeId,
    source: ResolvedSource<'a>,
    dialect: crate::provider::DialectKind,
) -> Result<Option<&'a str>, CodecError> {
    let id = graph.follow_alias(key, source)?;
    let Some(YamlNode::Scalar { text, .. }) = graph.node_opt(id, source) else {
        return Ok(None);
    };
    match crate::schema::resolve_scalar(graph, id, dialect, source)? {
        crate::schema::ResolvedScalar::Core {
            category: crate::schema::ScalarCategory::String,
            ..
        }
        | crate::schema::ResolvedScalar::Tagged {
            payload: crate::schema::ScalarCategory::String,
            ..
        } => Ok(Some(text)),
        _ => Ok(None),
    }
}

/// One parser state (the reference's `yaml_parser_state_t`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    StreamStart,
    ImplicitDocumentStart,
    DocumentContent,
    DocumentEnd,
    BlockNode,
    BlockSequenceFirstEntry,
    BlockSequenceEntry,
    IndentlessSequenceEntry,
    BlockMappingFirstKey,
    BlockMappingKey,
    BlockMappingValue,
    FlowSequenceFirstEntry,
    FlowSequenceEntry,
    FlowSequenceEntryMappingKey,
    FlowSequenceEntryMappingValue,
    FlowSequenceEntryMappingEnd,
    FlowMappingFirstKey,
    FlowMappingKey,
    FlowMappingValue,
    FlowMappingEmptyValue,
    End,
}

/// The YAML whole-document access session: one source, one document.
#[allow(
    clippy::struct_excessive_bools,
    reason = "each flag is one session phase fact; grouping them into a bitmask would obscure the state machine"
)]
pub(crate) struct YamlParseState {
    /// The source-backed decoded text and its byte map.
    decoded: crate::scan::DecodedSource,
    /// The resumable scanner.
    scanner: crate::scan::Scanner,
    /// The event parser building the graph.
    parser: YamlParser,
    /// The current phase.
    phase: Phase,
    /// The dialect (schema).
    dialect: crate::provider::DialectKind,
    /// The builder's root node.
    root: Option<DocumentNodeId>,
    /// The in-flight builder once the parse completed.
    builder: Option<AccountedDocumentBuilder<'static>>,
    /// The in-flight cooperative finalizer.
    finalizer: Option<AccountedDocumentFinalizer<'static>>,
    /// The completed document product.
    product: Option<DocumentProduct<'static>>,
    /// The in-flight cooperative source seal, when the parse committed bound-source spans (the YAML analog of the
    /// JSON/TOML seal step, which is what lets the edit lane diff against the source).
    binding_stage: Option<DocumentSourceBindingStage>,
    /// Whether the parse bound a source seal (and so must finalize and publish through the source-backed arms).
    bound: bool,
    /// Whether the current document's build admitted any bound-source span, deciding whether the source must be sealed
    /// and bound before the document can finish.
    spans_committed: bool,
    /// Whether the parse completed and the product is published.
    published: bool,
    /// Whether the current document was already consumed.
    document_done: bool,
    /// Whether the whole source was scanned to its end.
    stream_done: bool,
    /// Whether the current document's start event was seen. SESSION STATE, not a loop local: the parse budget yields
    /// Pending mid-document, and a reset flag would silently fall through to the null-document build.
    document_started: bool,
    /// Whether the last document ended at an IMPLICIT boundary (the next document's `---` was already consumed by the
    /// parser), so the next poll's reset must keep the parser positioned for that document's content rather than
    /// expecting a fresh document start.
    implicit_boundary: bool,
    /// Whether any REAL (non-empty) document was built in this session. A trailing `StreamEnd` after real documents is
    /// the stream's true end (zero more items), whereas a genuinely empty stream builds the null document (§4.8: "an
    /// empty or comment-only stream emits zero items; an explicit or bare empty document contains one empty plain
    /// scalar").
    saw_document: bool,
    /// The kept-subtree prune hint: which mapping members the requesting program provably reads. `None` keeps
    /// everything.
    prune: Option<crate::document::PruneLookup>,
    /// Builder coverage the document walk honours: skip attaching comments/anchors/style unless facts are demanded.
    coverage: jqf_data::BuilderCoverage,
    /// See [`crate::document::demanded_intrinsic`].
    want_tags: bool,
}

enum Phase {
    /// The grammar parse and document build are pending.
    Parse,
    /// Sealing the source segment the committed spans name.
    Seal,
    /// Finalizing the built document.
    Finalize,
    /// The product is ready to publish.
    Publish,
}

impl YamlParseState {
    pub(crate) fn try_new(
        source: ResolvedSource<'_>,
        dialect: crate::provider::DialectKind,
        prune: Option<crate::document::PruneLookup>,
        coverage: jqf_data::BuilderCoverage,
        want_tags: bool,
        resources: &ResourceContext<'_>,
    ) -> Result<Self, CodecError> {
        let decoded = crate::scan::DecodedSource::try_new(source, resources)?;
        let start = decoded.start_offset();
        let scanner = crate::scan::Scanner::try_new(start);
        let parser = YamlParser::try_new(dialect)?;
        Ok(Self {
            decoded,
            scanner,
            parser,
            phase: Phase::Parse,
            dialect,
            root: None,
            builder: None,
            binding_stage: None,
            bound: false,
            spans_committed: false,
            finalizer: None,
            product: None,
            published: false,
            document_done: false,
            stream_done: false,
            document_started: false,
            implicit_boundary: false,
            saw_document: false,
            prune,
            coverage,
            want_tags,
        })
    }

    /// Reinitializes this state for one more adjacent document, leaving exactly the state a fresh [`Self::try_new`]
    /// over the next document's source would have produced.
    ///
    /// The scanner and parser are rebuilt from scratch — a completed or failed document leaves no token, graph, or flag
    /// behind for the next one, including the three the multi-yield path resets by hand (`document_started`,
    /// `implicit_boundary`, `saw_document`). The retained [`crate::scan::DecodedSource`] descriptor is kept only in the
    /// plain UTF-8 no-BOM shape, where every narrowed value view of the source detects identically; any other shape
    /// DECLINES so the caller constructs a fresh session (declining is always safe).
    pub(crate) fn try_reset(&mut self, dialect: crate::provider::DialectKind) -> Result<bool, CodecError> {
        if !self.decoded.is_reusable() {
            return Ok(false);
        }
        self.scanner = crate::scan::Scanner::try_new(self.decoded.start_offset());
        self.parser = YamlParser::try_new(dialect)?;
        self.phase = Phase::Parse;
        self.dialect = dialect;
        self.root = None;
        self.builder = None;
        self.binding_stage = None;
        self.bound = false;
        self.spans_committed = false;
        self.finalizer = None;
        self.product = None;
        self.published = false;
        self.document_done = false;
        self.stream_done = false;
        self.document_started = false;
        self.implicit_boundary = false;
        self.saw_document = false;
        Ok(true)
    }
}

impl YamlParseState {
    #[allow(
        clippy::needless_pass_by_value,
        reason = "mirrors the by-value AccessSession::decode signature it forwards to"
    )]
    fn decode_once<'source>(
        &mut self,
        input: AccessInput<'_, 'source>,
        context: &mut CodecRunContext<'_, '_>,
    ) -> Result<AccessResult<'source>, CodecError> {
        let source = match input {
            AccessInput::Source(source) => source,
            AccessInput::Document(_) => {
                return Err(CodecError::new(CodecFailureKind::ProviderRouteMismatch));
            }
        };
        if self.published && self.stream_done {
            return Err(data_contract());
        }
        if self.published {
            // A document was published and the stream continues: reset the parser for the NEXT document (the scanner
            // keeps its position) and start another parse. An implicit boundary keeps the parser in the next document's
            // content; an explicit one starts fresh.
            self.parser.reset_after_document()?;
            self.phase = Phase::Parse;
            self.published = false;
            self.bound = false;
            self.spans_committed = false;
            self.document_done = false;
            self.document_started = self.implicit_boundary;
            self.implicit_boundary = false;
        }
        loop {
            match self.phase {
                Phase::Parse => {
                    let text = self.decoded.text(source);
                    // Drive the scanner and parser until a document completes or the budget yields. `document_started`
                    // is SESSION STATE (persisting across the Pending yields), never a loop local.
                    let mut polls = 0usize;
                    loop {
                        polls += 1;
                        // Force a control observation at the same cadence the poll-era forced yield ran: replenishing
                        // the cooperative budget observes cancellation and the ambient memory latch, so a straight-line
                        // parse cannot hide an infinite loop behind an unchecked budget.
                        if polls >= 64 {
                            context.replenish_work()?;
                            polls = 0;
                        }
                        if self.document_done {
                            break;
                        }
                        let text = self.decoded.text(source);
                        match self.parser.poll(text, &mut self.scanner, source, context.resources())? {
                            ParserPoll::Pending => context.replenish_work()?,
                            ParserPoll::Event(event) => {
                                match event {
                                    GraphEvent::StreamEnd => {
                                        self.stream_done = true;
                                        self.document_done = true;
                                    }
                                    GraphEvent::DocumentStart => {
                                        // A document start after a previous document began is an IMPLICIT boundary: the
                                        // previous document's content is complete (its nodes are already in the graph,
                                        // since the start event fires before the new document's nodes are added). Build
                                        // the previous document now; the reset keeps the parser positioned for the new
                                        // document's content.
                                        if self.document_started {
                                            self.document_done = true;
                                            self.implicit_boundary = true;
                                        } else {
                                            self.document_started = true;
                                        }
                                    }
                                    GraphEvent::DocumentEnd => {
                                        // One document's content is complete; build it now and, if the stream
                                        // continues, the next poll resets and parses the following document.
                                        self.document_done = true;
                                    }
                                    GraphEvent::Node => {}
                                }
                            }
                        }
                    }
                    let _ = text;
                    // One document's graph is complete: build the semantic document from it. A stream with a document
                    // but no explicit end still produced a root. An explicit empty document (`---` with only
                    // comments/whitespace) has a start but no root node and is a NULL document; only a TRUE trailing
                    // StreamEnd with no document start is the stream's end.
                    if self.document_started && self.parser.root().is_some() {
                        self.saw_document = true;
                        let root = self.parser.root().expect("root present");
                        let graph = self.parser.take_graph();
                        let (builder, root, spans_committed) = crate::document::build_document(
                            &graph,
                            root,
                            source,
                            self.dialect,
                            &self.decoded,
                            self.prune.as_ref(),
                            self.coverage,
                            self.want_tags,
                            context.resources(),
                        )?;
                        self.builder = Some(builder);
                        self.root = Some(root);
                        self.spans_committed = spans_committed;
                        if spans_committed {
                            // The document segment is the source the committed spans address: the WHOLE source on the
                            // single-document route, or exactly this document's consumed extent on the adjacent-value
                            // route (a multi-document stream's earlier documents must not be sealed into a later
                            // document's segment — the YAML analog of the JSON seal). The seal must cover every span
                            // the build admitted before the document may finish.
                            let sealed = self.sealed_source(source)?;
                            self.binding_stage =
                                Some(DocumentSourceBindingStage::new(sealed).map_err(crate::document::map_data)?);
                            self.phase = Phase::Seal;
                        } else {
                            self.phase = Phase::Finalize;
                        }
                    } else if self.document_started {
                        // An explicit empty document: a null scalar under core (§4.8). Counts as a real document even
                        // after earlier documents.
                        self.saw_document = true;
                        let (builder, root) = crate::scoped_build::build_null_document(context.resources())?;
                        self.builder = Some(builder);
                        self.root = Some(root);
                        self.phase = Phase::Finalize;
                    } else if self.saw_document {
                        // A trailing StreamEnd after real documents is the stream's true end: zero more items. A
                        // straight-line decode produces no outcome for it (the erased session is single-outcome; the
                        // sequence drive reopens per document) — a bare direct driver asks again at its own risk,
                        // exactly as a second decode of a completed session errors.
                        self.stream_done = true;
                        self.published = false;
                        return Err(data_contract());
                    } else {
                        // A no-document stream: nothing was ever started — a blank-only input (`\n`) or a comment-only
                        // one (`# c`), plus `...` with nothing after it. The reference emits ZERO documents for both,
                        // and the unit-stream view says "an empty or comment-only stream emits zero items" — but the
                        // sequence drive REQUIRES every poll at a real offset to yield an outcome (a session that
                        // completes without one is a contract violation), so once the session is OPENED it must
                        // publish. RECORDED NARROWING: a blank-only source is skipped by the drive's separator scan
                        // before the session opens, so it emits zero items; a comment-only source reaches this arm and
                        // publishes ONE null document (the bare empty document, a null scalar under core — the same
                        // value the explicit `---` yields). The Publish arm reports the WHOLE source as the consumed
                        // offset — never the parser's default 0, which would trip the forward-progress guard.
                        let (builder, root) = crate::scoped_build::build_null_document(context.resources())?;
                        self.builder = Some(builder);
                        self.root = Some(root);
                        self.stream_done = true;
                        self.phase = Phase::Finalize;
                    }
                }
                Phase::Seal => {
                    let sealed = self.sealed_source(source)?;
                    let stage = self.binding_stage.as_mut().ok_or_else(data_contract)?;
                    // SAFETY: codec-core retains one immutable source authority
                    // for the complete access session and passes that exact authority each poll; the stage was
                    // constructed over the same segment and re-verifies identity on every call.
                    match unsafe { stage.poll(sealed, context.resources()) }.map_err(crate::document::map_data)? {
                        DocumentSourceBindingPoll::Pending => {
                            context.replenish_work()?;
                        }
                        DocumentSourceBindingPoll::Ready(binding) => {
                            self.binding_stage = None;
                            self.builder
                                .as_mut()
                                .ok_or_else(data_contract)?
                                .bind_source(binding)
                                .map_err(crate::document::map_data)?;
                            self.bound = true;
                            self.phase = Phase::Finalize;
                        }
                    }
                }
                Phase::Finalize => {
                    // Begin the finalizer once; a Pending keeps it across polls.
                    if self.finalizer.is_none() {
                        let root = self.root.take().ok_or_else(data_contract)?;
                        let builder = self.builder.take().ok_or_else(data_contract)?;
                        self.finalizer = Some(
                            builder
                                .begin_finish(root, context.resources())
                                .map_err(crate::document::map_data)?,
                        );
                    }
                    let sealed = if self.bound {
                        Some(self.sealed_source(source)?)
                    } else {
                        None
                    };
                    let finalizer = self.finalizer.as_mut().ok_or_else(data_contract)?;
                    let poll = if let Some(sealed) = sealed {
                        // SAFETY: the codec-core access session owns and
                        // supplies the same immutable ResolvedSource authority for every parser poll, and `sealed` is
                        // the exact segment the binding was taken over.
                        unsafe { finalizer.poll_with_source(sealed, context.resources()) }
                            .map_err(crate::document::map_data)?
                    } else {
                        finalizer.poll(context.resources()).map_err(crate::document::map_data)?
                    };
                    let DocumentFinalizationPoll::Ready(document) = poll else {
                        context.replenish_work()?;
                        continue;
                    };
                    self.finalizer = None;
                    self.product = Some(DocumentProduct::try_new(document, context.resources())?);
                    self.phase = Phase::Publish;
                }
                Phase::Publish => {
                    let product = self.product.take().ok_or_else(data_contract)?;
                    // SAFETY: codec-core owns this exact immutable
                    // ResolvedSource for the whole access session; the binding was taken over this same segment.
                    let product = if self.bound {
                        let sealed = self.sealed_source(source)?;
                        unsafe { product.attach_borrowed_source_from_access_session(sealed, context.resources()) }?
                    } else {
                        product
                    };
                    self.published = true;
                    let outcome = AccessOutcome::FullDocument(product);
                    // A `---` unit is ONE document (§4.8): every decode publishes exactly the current document and
                    // reports where IT ended (in original source bytes), so the SDK's reopen-at-offset sequence drive
                    // can open the next unit fresh — one SDK item per document, in source order, exactly like JSON's
                    // adjacent-value mode. A stream with NO document (comment-only, or `...` alone) publishes the bare
                    // empty document and consumes the whole source.
                    let source_len = source.bytes().len();
                    // An empty/comment-only stream (stream done with no document started) never set a parser boundary —
                    // the whole source is one consumed span. Reporting the parser's default 0 would trip the sequence
                    // drive's forward-progress guard ("opened without reporting a consumed offset"), so account for the
                    // full source.
                    let boundary = if self.stream_done && !self.saw_document {
                        source_len
                    } else {
                        self.parser.document_boundary
                    };
                    let consumed = u64::try_from(self.decoded.original(boundary, source_len)).unwrap_or(u64::MAX);
                    return Ok(AccessResult::from_outcome_with_consumed_offset(outcome, consumed));
                }
            }
        }
    }
}

impl AccessSession for YamlParseState {
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
    fn decode<'source>(
        &mut self,
        input: AccessInput<'_, 'source>,
        context: &mut CodecRunContext<'_, '_>,
    ) -> Result<AccessResult<'source>, CodecError> {
        // The session owns the decoded/original byte map, so this boundary is where parse-phase diagnostics become
        // SOURCE coordinates (identity for UTF-8, where no map exists).
        let source_len = match &input {
            AccessInput::Source(source) => source.bytes().len(),
            AccessInput::Document(_) => 0,
        };
        self.decode_once(input, context)
            .map_err(|error| self.decoded.translate_error(error, source_len))
    }
}

impl YamlParseState {
    /// Whether the whole stream has been consumed (a trailing `StreamEnd` seen). A multi-document stream's last
    /// document sets this before its outcome; a direct driver keeps decoding until it reads true (the SDK's
    /// reopen-at-offset sequence drive never needs it — each decode is a fresh session).
    #[must_use]
    pub(crate) const fn stream_done(&self) -> bool {
        self.stream_done
    }

    /// The exact source segment this session's document names: everything its content occupies, and nothing after it.
    ///
    /// Every route names exactly the current document's consumed extent (the parser's document boundary — the `...`
    /// marker's end or the next document's start), so a multi-document stream's later documents are not sealed into an
    /// earlier document's segment. Narrowing preserves the source identity and base offset, so a span, a diagnostic,
    /// and the seal all keep naming the same absolute positions.
    fn sealed_source<'s>(&self, source: ResolvedSource<'s>) -> Result<ResolvedSource<'s>, CodecError> {
        let len = self.parser.document_boundary;
        let bytes = source.bytes().get(..len).ok_or_else(data_contract)?;
        Ok(ResolvedSource::new(
            source.source(),
            source.label(),
            bytes,
            source.base_offset(),
        ))
    }
}

/// Pushes under the parser's out-of-memory contract (the buffered entry lists use plain `Vec` + `try_reserve`,
/// mirroring `attach_node`).
fn try_push<T>(list: &mut Vec<T>, value: T) -> Result<(), CodecError> {
    list.try_reserve(1)
        .map_err(|_| CodecError::new(CodecFailureKind::AllocationFailure))?;
    list.push(value);
    Ok(())
}

/// One graph event emitted by the parser.
#[derive(Clone, Debug)]
pub(crate) enum GraphEvent {
    DocumentStart,
    DocumentEnd,
    Node,
    StreamEnd,
}

/// The tag-directive table for the current document.
#[derive(Clone, Debug)]
struct TagDirective {
    handle: String,
    prefix: String,
}

/// The resumable event parser.
pub(crate) struct YamlParser {
    /// The explicit state stack.
    states: Vec<State>,
    /// The current state.
    state: State,
    /// The queued tokens (the parser peeks one ahead).
    tokens: Vec<Token>,
    /// The current document's tag directives.
    tag_directives: Vec<TagDirective>,
    /// The graph under construction.
    graph: YamlGraph,
    /// The dialect (schema): the parser resolves YAML 1.1 merge keys at mapping close under CORE only (see
    /// [`Self::expand_merge_keys`]).
    dialect: crate::provider::DialectKind,
    /// The current node stack: the innermost open collection.
    open: Vec<OpenNode>,
    /// The document root node, once the first document's node completes.
    root: Option<NodeId>,
    /// Whether a BARE (implicit, no `---`) document is allowed at the current document start. Per the stream grammar, a
    /// bare document is allowed only as the FIRST document or after an explicit `...` suffix; after an implicit
    /// boundary the next document must be explicit (`---`/directives). This is what makes `word1 # comment\n word2` an
    /// error while `--- a\n...\nkey: value` is two documents.
    implicit_document_allowed: bool,
    /// The decoded-text offset where the current (or just-finished) document's consumed span ends: the explicit `...`
    /// marker's end, or — for an implicit boundary — the start of whatever token follows (the next document's
    /// `---`/directive, or the stream end). This is what a multi-document, adjacent-value drive reports as
    /// `consumed_offset` so the SDK can reopen at the next document (see `YamlParseState::poll`'s `Phase::Publish`).
    document_boundary: usize,
}

/// One open collection in the node stack.
#[derive(Debug)]
enum OpenNode {
    /// A sequence whose items are buffered locally and flushed into the graph's flat arena when the collection CLOSES —
    /// closure order is source order for each collection's own range (a nested collection's items land in the arena
    /// before its parent's, so appending at close keeps every range contiguous).
    Sequence { node: NodeId, items: Vec<NodeId> },
    /// Mapping with the in-progress key (None while expecting a key).
    Mapping {
        node: NodeId,
        key: Option<NodeId>,
        entries: Vec<(NodeId, NodeId)>,
    },
    /// A flow mapping started from a `?` inside a flow sequence.
    FlowSequenceMapping {
        node: NodeId,
        key: Option<NodeId>,
        entries: Vec<(NodeId, NodeId)>,
    },
}

impl YamlParser {
    /// Creates an empty parser (the graph is allocated now).
    pub(crate) fn try_new(dialect: crate::provider::DialectKind) -> Result<Self, CodecError> {
        let mut states = Vec::new();
        states
            .try_reserve_exact(16)
            .map_err(jqf_resource::ResourceError::from)?;
        Ok(Self {
            states,
            state: State::StreamStart,
            tokens: Vec::new(),
            tag_directives: Vec::new(),
            graph: YamlGraph::try_new()?,
            dialect,
            open: Vec::new(),
            root: None,
            implicit_document_allowed: true,
            document_boundary: 0,
        })
    }

    /// Takes the completed document's graph. The session (`YamlParseState`) calls `reset_after_document` right after
    /// this to give the parser a fresh graph for the next document in the stream; the placeholder here is only ever
    /// live in that narrow window.
    pub(crate) fn take_graph(&mut self) -> YamlGraph {
        let mut empty = YamlGraph::try_new_placeholder();
        core::mem::swap(&mut self.graph, &mut empty);
        empty
    }

    /// Prepares the parser for the NEXT document in the stream: clears the completed document's graph, node stack,
    /// root, and directive state. The STATE MACHINE and token queue are untouched — at an implicit boundary the parser
    /// has already consumed the next `---` and is positioned in `DocumentContent` for the following document, while at
    /// an explicit `...` boundary it is at `DocumentStart`. The scanner keeps its position in both cases.
    pub(crate) fn reset_after_document(&mut self) -> Result<(), CodecError> {
        self.tag_directives.clear();
        self.open.clear();
        self.root = None;
        self.graph = YamlGraph::try_new()?;
        Ok(())
    }

    /// The document root node.
    #[must_use]
    pub(crate) fn root(&self) -> Option<NodeId> {
        self.root
    }

    /// The decoded-text offset where the current (or just-finished) document's consumed span ends. See the field's own
    /// doc comment for the exact contract; other sessions that drive this parser directly (the scoped/located route)
    /// read it to report their own `consumed_offset` under the adjacent-value contract.
    #[must_use]
    pub(crate) fn document_boundary(&self) -> usize {
        self.document_boundary
    }

    /// One poll: consume one token and run the state machine until an event is produced (or the budget yields).
    pub(crate) fn poll(
        &mut self,
        text: &[u8],
        scanner: &mut Scanner,
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<ParserPoll, CodecError> {
        if resources.admit_work_transition()? == jqf_resource::WorkAdmission::Pending {
            return Ok(ParserPoll::Pending);
        }
        let mut steps = 0usize;
        loop {
            steps += 1;
            if steps > 1_000_000 {
                return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                    contract: "YAML parser state machine made no progress",
                }));
            }
            // The reference's PEEK_TOKEN fetches more tokens lazily: a state function can consume TWO tokens in one
            // step (an indicator plus the token after it), so the queue is refilled to at least two tokens — or one,
            // when the scanner is done — before every step.
            while self.tokens.len() < 2 {
                match scanner.poll(text, source, resources)? {
                    ScanPoll::Token(token) => {
                        self.tokens.push(token);
                        self.drain_comments(scanner);
                    }
                    ScanPoll::Done => break,
                }
            }
            // Run one state-machine step. A step may legitimately consume nothing visible (a collection-start node
            // hands the start token to its first-entry state); the steps cap above bounds the loop.
            let event = self.run_state(scanner, text, source, resources)?;
            if let Some(event) = event {
                return Ok(ParserPoll::Event(event));
            }
        }
    }

    /// Runs one state-machine step, returning the event it produced (if any). A step that emits nothing continues the
    /// loop. Peek the next token, fetching from the scanner when the queue is empty (the reference's `PEEK_TOKEN`). A
    /// state step may consume several tokens, so the queue is refilled lazily on every access.
    fn peek_token(
        &mut self,
        scanner: &mut Scanner,
        text: &[u8],
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<Token>, CodecError> {
        while self.tokens.is_empty() {
            match scanner.poll(text, source, resources)? {
                ScanPoll::Token(token) => {
                    self.tokens.push(token);
                    self.drain_comments(scanner);
                }
                ScanPoll::Done => break,
            }
        }
        Ok(self.tokens.first().cloned())
    }

    /// Moves the scanner's skipped trivia comment spans into the graph, in source order, so the whole-document walker
    /// can attach them as leading-comment facts to the node each one precedes.
    fn drain_comments(&mut self, scanner: &mut Scanner) {
        for span in scanner.take_pending_comments() {
            self.graph.add_comment(span);
        }
    }

    /// Take the head token (the caller must have peeked first).
    fn head_token(&mut self) -> Token {
        self.tokens.remove(0)
    }

    fn run_state(
        &mut self,
        scanner: &mut Scanner,
        text: &[u8],
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<GraphEvent>, CodecError> {
        self.run_state_inner(scanner, text, source, resources)
    }

    fn run_state_inner(
        &mut self,
        scanner: &mut Scanner,
        text: &[u8],
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<GraphEvent>, CodecError> {
        let Some(token) = self.peek_token(scanner, text, source, resources)? else {
            return Ok(None);
        };
        match self.state {
            State::StreamStart => match token.kind {
                TokenKind::StreamStart => {
                    self.tokens.remove(0);
                    self.state = State::ImplicitDocumentStart;
                    Ok(None)
                }
                _ => Err(error::invalid(
                    source,
                    token.start,
                    "stream",
                    "did not find expected <stream-start>",
                )),
            },
            State::ImplicitDocumentStart => self.parse_document_start(scanner, text, source, resources),
            State::DocumentContent => self.parse_document_content(scanner, text, source, resources),
            State::DocumentEnd => self.parse_document_end(scanner, text, source, resources),
            // The state field is NOT popped here: `parse_node` pops the caller's continuation for a terminal node or
            // sets the next state for a collection start.
            State::BlockNode => self.parse_node(scanner, text, source, resources, true, false),
            State::BlockSequenceFirstEntry => self.parse_block_sequence_entry(scanner, text, source, resources, true),
            State::BlockSequenceEntry => self.parse_block_sequence_entry(scanner, text, source, resources, false),
            State::IndentlessSequenceEntry => self.parse_indentless_sequence_entry(scanner, text, source, resources),
            State::BlockMappingFirstKey => self.parse_block_mapping_key(scanner, text, source, resources, true),
            State::BlockMappingKey => self.parse_block_mapping_key(scanner, text, source, resources, false),
            State::BlockMappingValue => self.parse_block_mapping_value(scanner, text, source, resources),
            State::FlowSequenceFirstEntry => self.parse_flow_sequence_entry(scanner, text, source, resources, true),
            State::FlowSequenceEntry => self.parse_flow_sequence_entry(scanner, text, source, resources, false),
            State::FlowSequenceEntryMappingKey => {
                self.parse_flow_sequence_entry_mapping_key(scanner, text, source, resources)
            }
            State::FlowSequenceEntryMappingValue => {
                self.parse_flow_sequence_entry_mapping_value(scanner, text, source, resources)
            }
            State::FlowSequenceEntryMappingEnd => {
                self.parse_flow_sequence_entry_mapping_end(scanner, text, source, resources)
            }
            State::FlowMappingFirstKey => self.parse_flow_mapping_key(scanner, text, source, resources, true),
            State::FlowMappingKey => self.parse_flow_mapping_key(scanner, text, source, resources, false),
            State::FlowMappingValue => self.parse_flow_mapping_value(scanner, text, source, resources, false),
            State::FlowMappingEmptyValue => self.parse_flow_mapping_value(scanner, text, source, resources, true),
            State::End => Ok(Some(GraphEvent::StreamEnd)),
        }
    }

    fn pop_state(&mut self) -> State {
        self.states.pop().unwrap_or(State::End)
    }

    fn push_state(&mut self, state: State) {
        self.states.push(state);
    }

    fn peek_kind(&self) -> Option<&TokenKind> {
        self.tokens.first().map(|t| &t.kind)
    }

    /// `parse_document_start` (implicit and explicit arms).
    fn parse_document_start(
        &mut self,
        scanner: &mut Scanner,
        text: &[u8],
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<GraphEvent>, CodecError> {
        let implicit = self.state == State::ImplicitDocumentStart;
        // Skip extra document-end indicators after an explicit document.
        if !implicit {
            while matches!(self.peek_kind(), Some(TokenKind::DocumentEnd)) {
                self.head_token();
            }
        }
        let token = self
            .peek_token(scanner, text, source, resources)?
            .ok_or_else(data_contract)?;
        // An implicit document: any token except directives/markers/stream-end.
        if implicit
            && !matches!(
                token.kind,
                TokenKind::VersionDirective { .. }
                    | TokenKind::TagDirective { .. }
                    | TokenKind::ReservedDirective
                    | TokenKind::DocumentStart
                    | TokenKind::DocumentEnd
                    | TokenKind::StreamEnd
            )
        {
            if !self.implicit_document_allowed {
                // A bare document after an implicit boundary (no `...`) is invalid: the next document must be explicit.
                // This is the corpus's `word1 # comment\nword2` fail reading.
                return Err(error::invalid(
                    source,
                    token.start,
                    "document",
                    "did not find expected <document start>",
                ));
            }
            // The first document is bare; the next must be explicit.
            self.implicit_document_allowed = false;
            self.process_directives();
            self.push_state(State::DocumentEnd);
            self.state = State::BlockNode;
            // Defensive: if a document boundary is ever recognized by a fresh `DocumentStart` rather than a
            // `parse_document_end` call (the ordinary path — see that function), the natural cut point for whatever
            // came before is exactly where this new document begins.
            self.document_boundary = token.start;
            return Ok(Some(GraphEvent::DocumentStart));
        }
        if matches!(token.kind, TokenKind::DocumentEnd) {
            // A `...` at a document start with nothing after it is an empty stream (the corpus's `...` and `#
            // comment\n...` reading); a `...` followed by a real document is a boundary, so consume it and check what
            // follows.
            self.head_token();
            let token = self
                .peek_token(scanner, text, source, resources)?
                .ok_or_else(data_contract)?;
            if matches!(token.kind, TokenKind::StreamEnd) {
                self.state = State::End;
                return Ok(Some(GraphEvent::StreamEnd));
            }
            // Another document follows: re-enter through the poll loop so a run of `...` markers cannot grow the Rust
            // stack with the input.
            self.state = State::ImplicitDocumentStart;
            return Ok(None);
        }
        if !matches!(token.kind, TokenKind::StreamEnd) {
            let start = token.start;
            self.collect_directives(scanner, text, source, resources)?;
            let token = self
                .peek_token(scanner, text, source, resources)?
                .ok_or_else(data_contract)?;
            if !matches!(token.kind, TokenKind::DocumentStart) {
                return Err(error::invalid(
                    source,
                    token.start,
                    "document",
                    "did not find expected <document start>",
                ));
            }
            self.head_token();
            self.push_state(State::DocumentEnd);
            self.state = State::DocumentContent;
            // Defensive, same rationale as the implicit-bare-document arm above: `start` is the boundary a fresh
            // `DocumentStart` here would imply for whatever preceded it.
            self.document_boundary = start;
            return Ok(Some(GraphEvent::DocumentStart));
        }
        // Stream end.
        self.state = State::End;
        Ok(Some(GraphEvent::StreamEnd))
    }

    fn parse_document_content(
        &mut self,
        scanner: &mut Scanner,
        text: &[u8],
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<GraphEvent>, CodecError> {
        let token = self
            .peek_token(scanner, text, source, resources)?
            .ok_or_else(data_contract)?;
        if matches!(
            token.kind,
            TokenKind::VersionDirective { .. } | TokenKind::TagDirective { .. }
        ) {
            // A directive appearing after document content requires a `...` (explicit document-end marker) first (the
            // corpus's fail reading for a directive without a footer).
            return Err(error::invalid(
                source,
                token.start,
                "document",
                "did not find expected <document end>",
            ));
        }
        if matches!(token.kind, TokenKind::DocumentEnd | TokenKind::StreamEnd) {
            self.state = self.pop_state();
            self.process_empty_scalar(source)
        } else if matches!(token.kind, TokenKind::DocumentStart) {
            // An explicit document start follows content: finish the current document's content (an empty tail) and let
            // the boundary state handle the marker.
            self.state = self.pop_state();
            self.process_empty_scalar(source)
        } else {
            self.parse_node(scanner, text, source, resources, true, false)
        }
    }

    fn parse_document_end(
        &mut self,
        scanner: &mut Scanner,
        text: &[u8],
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<GraphEvent>, CodecError> {
        let token = self
            .peek_token(scanner, text, source, resources)?
            .ok_or_else(data_contract)?;
        let explicit = matches!(token.kind, TokenKind::DocumentEnd);
        // The consumed span for THIS document: the explicit `...` marker's own end, or — for an implicit boundary — the
        // start of whatever follows (the next document's `---`/directive, or the stream end; `token` is not consumed on
        // that path, so its `start` is exactly where the next document's material begins).
        self.document_boundary = if explicit { token.end } else { token.start };
        // A directive appearing after document content without an explicit `...` footer is an error (the corpus's fail
        // reading). The directive can only begin a NEW document after a real document-end marker.
        if matches!(
            token.kind,
            TokenKind::VersionDirective { .. } | TokenKind::TagDirective { .. }
        ) && !explicit
        {
            return Err(error::invalid(
                source,
                token.start,
                "document",
                "did not find expected <document end>",
            ));
        }
        if explicit {
            self.head_token();
        }
        // The document's tag directives are cleared at the boundary.
        self.tag_directives.clear();
        // A following document may be explicit (`---`/directives) OR bare (implicit content after a `...`), so the next
        // document starts in the IMPLICIT state (the corpus's bare-document-after-marker reading). A bare document is
        // allowed only after an explicit `...` suffix; after an implicit boundary the next must be explicit.
        self.implicit_document_allowed = explicit;
        self.state = State::ImplicitDocumentStart;
        Ok(Some(GraphEvent::DocumentEnd))
    }

    /// The node production (anchor/tag properties, alias, scalar, collections, indentless sequences).
    fn parse_node(
        &mut self,
        scanner: &mut Scanner,
        text: &[u8],
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
        block: bool,
        indentless_sequence: bool,
    ) -> Result<Option<GraphEvent>, CodecError> {
        let token = self
            .peek_token(scanner, text, source, resources)?
            .ok_or_else(data_contract)?;
        if let TokenKind::Alias(name) = &token.kind {
            let name_id = self.graph.intern_name(name)?;
            let target = anchor::resolve(&self.graph, name_id, source, token.start)?;
            self.state = self.pop_state();
            let span = Span::try_from_usize(token.start, token.end)
                .map_err(|_| CodecError::new(CodecFailureKind::Overflow))?;
            let node = self.graph.add_alias(target, span)?;
            self.attach_node(node)?;
            self.head_token();
            return Ok(Some(GraphEvent::Node));
        }
        let start = token.start;
        // Properties: ANCHOR TAG? or TAG ANCHOR?. Every peek is LAZY (the queue refills from the scanner), because a
        // property can be the last token the scanner has handed out so far.
        let mut anchor: Option<String> = None;
        let mut tag: Option<String> = None;
        let mut token = self
            .peek_token(scanner, text, source, resources)?
            .ok_or_else(data_contract)?;
        if let TokenKind::Anchor(name) = &token.kind {
            anchor = Some(name.clone());
            self.head_token();
            token = self
                .peek_token(scanner, text, source, resources)?
                .ok_or_else(data_contract)?;
            if let TokenKind::Tag { handle, suffix } = &token.kind {
                tag = Some(self.resolve_tag(handle, suffix, source)?);
                self.head_token();
                token = self
                    .peek_token(scanner, text, source, resources)?
                    .ok_or_else(data_contract)?;
            }
        } else if let TokenKind::Tag { handle, suffix } = &token.kind {
            tag = Some(self.resolve_tag(handle, suffix, source)?);
            self.head_token();
            token = self
                .peek_token(scanner, text, source, resources)?
                .ok_or_else(data_contract)?;
            if let TokenKind::Anchor(name) = &token.kind {
                anchor = Some(name.clone());
                self.head_token();
                token = self
                    .peek_token(scanner, text, source, resources)?
                    .ok_or_else(data_contract)?;
            }
        }
        // Indentless sequence?
        if indentless_sequence && matches!(token.kind, TokenKind::BlockEntry) {
            self.new_sequence(start, token.end, anchor, tag)?;
            self.state = State::IndentlessSequenceEntry;
            return Ok(Some(GraphEvent::Node));
        }
        match &token.kind {
            TokenKind::Scalar { value, style } => {
                let end = token.end;
                let node = self.new_scalar(value.clone(), *style, start, end, anchor, tag, source)?;
                self.state = self.pop_state();
                self.head_token();
                self.attach_node(node)?;
                Ok(Some(GraphEvent::Node))
            }
            // The reference's `parse_node` does NOT consume a collection-start token: the first-entry state function
            // takes it (each first-entry arm below calls `take_token` on its start token). Consuming it here too would
            // shift the whole token stream.
            TokenKind::FlowSequenceStart => {
                let end = token.end;
                self.new_sequence(start, end, anchor, tag)?;
                self.state = State::FlowSequenceFirstEntry;
                Ok(Some(GraphEvent::Node))
            }
            TokenKind::FlowMappingStart => {
                let end = token.end;
                self.new_mapping(start, end, anchor, tag)?;
                self.state = State::FlowMappingFirstKey;
                Ok(Some(GraphEvent::Node))
            }
            TokenKind::BlockSequenceStart if block => {
                let end = token.end;
                self.new_sequence(start, end, anchor, tag)?;
                self.state = State::BlockSequenceFirstEntry;
                Ok(Some(GraphEvent::Node))
            }
            TokenKind::BlockMappingStart if block => {
                let end = token.end;
                self.new_mapping(start, end, anchor, tag)?;
                self.state = State::BlockMappingFirstKey;
                Ok(Some(GraphEvent::Node))
            }
            // An empty-key block mapping: a `:` value indicator with no preceding simple key (the corpus's `: a`
            // reading — the empty scalar is the key). The scanner did not emit a BLOCK-MAPPING-START token, so start
            // the mapping here and let the key state (non-first, which does not consume a start token) handle the VALUE
            // as the empty key.
            TokenKind::Value if block => {
                let end = token.end;
                self.new_mapping(start, end, anchor, tag)?;
                self.state = State::BlockMappingKey;
                Ok(Some(GraphEvent::Node))
            }
            _ if anchor.is_some() || tag.is_some() => {
                // A node with properties but no content: an empty scalar.
                let node = self.new_scalar(
                    ScannedScalar::Owned(String::new()),
                    ScalarStyle::Plain,
                    start,
                    token.start,
                    anchor,
                    tag,
                    source,
                )?;
                self.state = self.pop_state();
                self.attach_node(node)?;
                Ok(Some(GraphEvent::Node))
            }
            _ => Err(error::invalid(
                source,
                token.start,
                "node",
                if block {
                    "while parsing a block node: did not find expected node content"
                } else {
                    "while parsing a flow node: did not find expected node content"
                },
            )),
        }
    }

    /// Resolves a tag handle+suffix pair against the document's directives (the reference's tag resolution in
    /// `parse_node`).
    fn resolve_tag(&mut self, handle: &str, suffix: &str, source: ResolvedSource<'_>) -> Result<String, CodecError> {
        if handle.is_empty() {
            // `!<...>` verbatim or the bare `!` (suffix "!").
            return Ok(suffix.to_owned());
        }
        for directive in &self.tag_directives {
            if directive.handle == handle {
                let mut tag = String::with_capacity(directive.prefix.len() + suffix.len());
                tag.push_str(&directive.prefix);
                tag.push_str(suffix);
                return Ok(tag);
            }
        }
        Err(error::invalid(
            source,
            self.tokens.first().map_or(0, |t| t.start),
            "tag",
            "found undefined tag handle",
        ))
    }

    /// Creates a sequence node and opens it on the node stack.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "tag/anchor arrive owned from the token stream; only their text is read for interning"
    )]
    fn new_sequence(
        &mut self,
        start: usize,
        end: usize,
        anchor: Option<String>,
        tag: Option<String>,
    ) -> Result<NodeId, CodecError> {
        let span = Span::try_from_usize(start, end).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?;
        let tag_id = tag.as_deref().map(|text| self.graph.intern_name(text)).transpose()?;
        let anchor_id = anchor.as_deref().map(|text| self.graph.intern_name(text)).transpose()?;
        let node = self.graph.add_sequence(tag_id, anchor_id, span)?;
        if let Some(name) = anchor {
            let name_id = self.graph.intern_name(&name)?;
            self.graph.bind_anchor(name_id, node);
        }
        self.open.push(OpenNode::Sequence {
            node,
            items: Vec::new(),
        });
        Ok(node)
    }

    /// Creates a mapping node and opens it on the node stack.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "tag/anchor arrive owned from the token stream; only their text is read for interning"
    )]
    fn new_mapping(
        &mut self,
        start: usize,
        end: usize,
        anchor: Option<String>,
        tag: Option<String>,
    ) -> Result<NodeId, CodecError> {
        let span = Span::try_from_usize(start, end).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?;
        let tag_id = tag.as_deref().map(|text| self.graph.intern_name(text)).transpose()?;
        let anchor_id = anchor.as_deref().map(|text| self.graph.intern_name(text)).transpose()?;
        let node = self.graph.add_mapping(tag_id, anchor_id, span)?;
        if let Some(name) = anchor {
            let name_id = self.graph.intern_name(&name)?;
            self.graph.bind_anchor(name_id, node);
        }
        self.open.push(OpenNode::Mapping {
            node,
            key: None,
            entries: Vec::new(),
        });
        Ok(node)
    }

    /// Creates a scalar node.
    #[allow(
        clippy::too_many_arguments,
        clippy::needless_pass_by_value,
        reason = "tag/anchor arrive owned from the token stream; only their text is read for interning"
    )]
    fn new_scalar(
        &mut self,
        value: ScannedScalar,
        style: ScalarStyle,
        start: usize,
        end: usize,
        anchor: Option<String>,
        tag: Option<String>,
        source: ResolvedSource<'_>,
    ) -> Result<NodeId, CodecError> {
        let span = Span::try_from_usize(start, end).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?;
        let text = match value {
            // The scanner guarantees the decoded text IS these exact source bytes (a fold-free plain scalar's content
            // span, or an escape-free quoted scalar's inner span): name the span, no arena copy.
            ScannedScalar::Span(inner) => TextRef::Span(inner),
            ScannedScalar::Owned(value) => {
                if style == ScalarStyle::Plain {
                    // The byte compare is the belt-and-braces gate for the owned path: a plain scalar whose decoded
                    // text still IS the source bytes (a fold whose breaks happen to preserve them) names the span
                    // instead of the arena, exactly as before the scanner returned spans.
                    let span_bytes = source.bytes().get(span.start() as usize..span.end() as usize);
                    if span_bytes == Some(value.as_bytes()) {
                        TextRef::Span(span)
                    } else {
                        TextRef::Owned(self.graph.store_text(&value))
                    }
                } else {
                    TextRef::Owned(self.graph.store_text(&value))
                }
            }
        };
        let _ = source;
        let tag_id = tag.as_deref().map(|text| self.graph.intern_name(text)).transpose()?;
        let anchor_id = anchor.as_deref().map(|text| self.graph.intern_name(text)).transpose()?;
        let node = self.graph.add_scalar(text, style as u8, tag_id, anchor_id, span)?;
        if let Some(name) = anchor {
            let name_id = self.graph.intern_name(&name)?;
            self.graph.bind_anchor(name_id, node);
        }
        Ok(node)
    }

    /// Attaches a completed node to the innermost open collection, or records it as the document root.
    fn attach_node(&mut self, node: NodeId) -> Result<(), CodecError> {
        let Some(open) = self.open.last_mut() else {
            self.root = Some(node);
            self.graph.set_root(node);
            return Ok(());
        };
        match open {
            OpenNode::Sequence { items, .. } => {
                items
                    .try_reserve(1)
                    .map_err(|_| CodecError::new(CodecFailureKind::AllocationFailure))?;
                items.push(node);
            }
            OpenNode::Mapping { key, entries, .. } | OpenNode::FlowSequenceMapping { key, entries, .. } => match key {
                None => *key = Some(node),
                Some(key_node) => {
                    let key_node = *key_node;
                    entries
                        .try_reserve(1)
                        .map_err(|_| CodecError::new(CodecFailureKind::AllocationFailure))?;
                    entries.push((key_node, node));
                    *key = None;
                }
            },
        }
        Ok(())
    }

    /// Closes the innermost open collection, attaching it to its parent. A closing mapping resolves its YAML 1.1 merge
    /// key here, BEFORE its entries flush to the graph, so every route (whole, scoped, streams) reads merged mappings
    /// from the same arena.
    fn close_collection(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        let Some(open) = self.open.pop() else {
            return Ok(());
        };
        let node = match open {
            OpenNode::Sequence { node, items } => {
                self.graph.close_sequence(node, &items)?;
                node
            }
            OpenNode::Mapping { node, entries, .. } | OpenNode::FlowSequenceMapping { node, entries, .. } => {
                let (entries, merged_into) = match self.expand_merge_keys(node, &entries, source, resources)? {
                    Some((expanded, provenance)) => (expanded, provenance),
                    None => (entries, Vec::new()),
                };
                for (value, host) in merged_into {
                    self.graph.record_merge_host(value, host);
                }
                self.graph.close_mapping(node, &entries)?;
                node
            }
        };
        self.attach_node(node)
    }

    /// Resolves a closing mapping's YAML 1.1 merge key (`<<`, `tag:yaml.org,2002:merge` — yaml.org/type/merge.html)
    /// under the CORE schema. The merge value must be a mapping or a sequence of mappings; its entries splice into the
    /// host at the `<<` entry's position, where the host's own keys always override merged entries, EARLIER sequence
    /// items override later ones, and the `<<` key itself never survives. Chained merges cost nothing extra: a
    /// merged-in mapping closed before its host, so its own entries are already resolved. The failsafe and JSON schemas
    /// keep `<<` as a literal key, consistent with their refusal of the other 1.1-isms (`Null`, `~`, radix integers,
    /// ...).
    ///
    /// Returns `None` when the mapping carries no merge key, so the common case pays one scan and no allocation. When a
    /// merge resolves, returns the expanded entries AND the merge provenance: one `(merged VALUE node, host mapping
    /// node)` pair per admitted merged entry, so the document build can mark each merge-inherited member and the edit
    /// lane can splice an override into the host instead of patching the anchor.
    #[allow(
        clippy::type_complexity,
        reason = "one return carries the expanded entries and the merge provenance ledger beside them"
    )]
    fn expand_merge_keys(
        &self,
        host: NodeId,
        entries: &[(NodeId, NodeId)],
        source: ResolvedSource<'_>,
        resources: &ResourceContext<'_>,
    ) -> Result<Option<(Vec<(NodeId, NodeId)>, Vec<(NodeId, NodeId)>)>, CodecError> {
        if self.dialect != crate::provider::DialectKind::Core {
            return Ok(None);
        }
        let Some(merge_at) = entries.iter().position(|(key, _)| self.is_merge_key(*key, source)) else {
            return Ok(None);
        };
        // At most one merge key per mapping — the duplicate-key rejection's merge arm (under the 1.1 merge type a
        // second `<<` is a duplicate of the first, exactly like any other repeated key).
        if let Some((second, _)) = entries[merge_at + 1..]
            .iter()
            .find(|(key, _)| self.is_merge_key(*key, source))
        {
            return Err(self.merge_error(*second, "duplicate merge key ('<<') in one mapping", source));
        }
        let (merge_key, merge_value) = entries[merge_at];
        // The mappings to merge, in override order (earlier wins).
        let value = self.resolve_alias_target(merge_value);
        let mut merge_sources: Vec<NodeId> = Vec::new();
        match self.graph.node_opt(value, source) {
            Some(YamlNode::Mapping { .. }) => {
                self.check_merge_source_closed(value, host, merge_key, source)?;
                try_push(&mut merge_sources, value)?;
            }
            Some(YamlNode::Sequence { items, .. }) => {
                for item in items {
                    let item = self.resolve_alias_target(*item);
                    if self.graph.node_kind(item) != Some(NodeKind::Mapping) {
                        return Err(self.merge_error(item, "merge key ('<<') sequence item is not a mapping", source));
                    }
                    self.check_merge_source_closed(item, host, merge_key, source)?;
                    try_push(&mut merge_sources, item)?;
                }
            }
            _ => {
                return Err(self.merge_error(
                    value,
                    "merge key ('<<') value is not a mapping or a sequence of mappings",
                    source,
                ));
            }
        }
        // Host keys always win; earlier merge sources beat later ones. A merged candidate is dropped when the host
        // (minus the `<<` entry itself) or an already-accepted merged entry holds an equal key under
        // yaml.key-equivalence@1 — the SAME law the duplicate-key validation later applies to the expanded entries, so
        // a legal merge can never manufacture a duplicate-key rejection.
        let mut equality = KeyEquality::try_new(&self.graph, source, self.dialect)?;
        let mut merged: Vec<(NodeId, NodeId)> = Vec::new();
        // One provenance pair per admitted entry: the merged VALUE node and the host mapping it landed in.
        let mut merged_into: Vec<(NodeId, NodeId)> = Vec::new();
        let mut host_strings = BTreeSet::new();
        let mut merged_strings = BTreeSet::new();
        for (index, (host_key, _)) in entries.iter().enumerate() {
            if index == merge_at {
                continue;
            }
            if let Some(text) = merge_core_string_text(&self.graph, *host_key, source, self.dialect)? {
                host_strings.insert(text.to_owned());
            }
        }
        for mapping in &merge_sources {
            let Some(YamlNode::Mapping {
                entries: candidates, ..
            }) = self.graph.node_opt(*mapping, source)
            else {
                return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                    contract: "YAML merge source is not a closed mapping",
                }));
            };
            for candidate in candidates {
                // A merged candidate key must be a scalar: this loop is the only place keys of arbitrary kind are
                // compared, so the scalar law is enforced HERE instead of inside the comparator's container arms (see
                // [`crate::key::require_scalar_merge_key`]).
                crate::key::require_scalar_merge_key(&self.graph, candidate.0, source)?;
                let mut overridden = false;
                if let Some(text) = merge_core_string_text(&self.graph, candidate.0, source, self.dialect)? {
                    overridden = host_strings.contains(text) || merged_strings.contains(text);
                } else {
                    for (index, (host_key, _)) in entries.iter().enumerate() {
                        if index != merge_at && equality.equals(*host_key, candidate.0, resources)? == Verdict::Equal {
                            overridden = true;
                            break;
                        }
                    }
                    if !overridden {
                        for (merged_key, _) in &merged {
                            if equality.equals(*merged_key, candidate.0, resources)? == Verdict::Equal {
                                overridden = true;
                                break;
                            }
                        }
                    }
                }
                if !overridden {
                    if let Some(text) = merge_core_string_text(&self.graph, candidate.0, source, self.dialect)? {
                        merged_strings.insert(text.to_owned());
                    }
                    try_push(&mut merged, *candidate)?;
                    try_push(&mut merged_into, (candidate.1, host))?;
                }
            }
        }
        // Splice: host entries keep their order; the merged entries stand where the `<<` entry stood.
        let mut expanded: Vec<(NodeId, NodeId)> = Vec::new();
        for (index, entry) in entries.iter().enumerate() {
            if index == merge_at {
                for entry in &merged {
                    try_push(&mut expanded, *entry)?;
                }
            } else {
                try_push(&mut expanded, *entry)?;
            }
        }
        Ok(Some((expanded, merged_into)))
    }

    /// Whether a mapping key node is the YAML 1.1 merge key: a plain untagged `<<` scalar, or any scalar carrying the
    /// explicit merge tag. A quoted `"<<"` or an explicit `!!str <<` stays a literal key.
    fn is_merge_key(&self, node: NodeId, source: ResolvedSource<'_>) -> bool {
        match self.graph.node_opt(node, source) {
            Some(YamlNode::Scalar { text, style, tag, .. }) => match tag {
                None => style == crate::graph::ScalarStyle::Plain && text == "<<",
                Some(tag) => tag == crate::schema::TAG_MERGE,
            },
            _ => false,
        }
    }

    /// Follows an alias to its anchored target (targets are never aliases — anchors bind only content nodes — so one
    /// step suffices; the loop is the safe form and terminates because a target precedes its alias).
    fn resolve_alias_target(&self, mut node: NodeId) -> NodeId {
        while self.graph.node_kind(node) == Some(NodeKind::Alias) {
            node = self.graph.alias_target(node);
        }
        node
    }

    /// Rejects a merge source that is the closing mapping itself or a still-open ancestor (`a: &a {<<: *a}`): its
    /// entries have not been flushed to the graph yet, so merging would silently read nothing.
    fn check_merge_source_closed(
        &self,
        target: NodeId,
        host: NodeId,
        merge_key: NodeId,
        source: ResolvedSource<'_>,
    ) -> Result<(), CodecError> {
        let open = target == host
            || self.open.iter().any(|open| match open {
                OpenNode::Sequence { node, .. }
                | OpenNode::Mapping { node, .. }
                | OpenNode::FlowSequenceMapping { node, .. } => *node == target,
            });
        if open {
            return Err(self.merge_error(
                merge_key,
                "merge key ('<<') references its own enclosing mapping",
                source,
            ));
        }
        Ok(())
    }

    /// A structured merge-key diagnostic at a node's source span.
    fn merge_error(&self, node: NodeId, message: &'static str, source: ResolvedSource<'_>) -> CodecError {
        let span = self.graph.node_span(node);
        error::invalid_range(source, span.start() as usize, span.end() as usize, "merge-key", message)
    }

    /// The empty-scalar production (a `KEY`/`VALUE`/`BLOCK-ENTRY` with no content).
    fn process_empty_scalar(&mut self, source: ResolvedSource<'_>) -> Result<Option<GraphEvent>, CodecError> {
        let offset = self.tokens.first().map_or(0, |t| t.start);
        let node = self.new_scalar(
            ScannedScalar::Owned(String::new()),
            ScalarStyle::Plain,
            offset,
            offset,
            None,
            None,
            source,
        )?;
        self.attach_node(node)?;
        Ok(Some(GraphEvent::Node))
    }

    // --- block collections -------------------------------------------------

    fn parse_block_sequence_entry(
        &mut self,
        scanner: &mut Scanner,
        text: &[u8],
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
        first: bool,
    ) -> Result<Option<GraphEvent>, CodecError> {
        if first {
            let token = self
                .peek_token(scanner, text, source, resources)?
                .ok_or_else(data_contract)?;
            self.head_token(); // BLOCK-SEQUENCE-START
            let _ = token;
        }
        let token = self
            .peek_token(scanner, text, source, resources)?
            .ok_or_else(data_contract)?;
        if matches!(token.kind, TokenKind::BlockEntry) {
            let mark = token.end;
            self.head_token();
            let token = self
                .peek_token(scanner, text, source, resources)?
                .ok_or_else(data_contract)?;
            if !matches!(token.kind, TokenKind::BlockEntry | TokenKind::BlockEnd) {
                self.push_state(State::BlockSequenceEntry);
                return self.parse_node(scanner, text, source, resources, true, false);
            }
            self.state = State::BlockSequenceEntry;
            return self.process_empty_scalar_at(source, mark);
        }
        if matches!(token.kind, TokenKind::BlockEnd) {
            self.state = self.pop_state();
            self.head_token();
            self.close_collection(source, resources)?;
            return Ok(None);
        }
        Err(error::invalid(
            source,
            token.start,
            "block-sequence",
            "did not find expected '-' indicator",
        ))
    }

    fn parse_indentless_sequence_entry(
        &mut self,
        scanner: &mut Scanner,
        text: &[u8],
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<GraphEvent>, CodecError> {
        let token = self
            .peek_token(scanner, text, source, resources)?
            .ok_or_else(data_contract)?;
        if matches!(token.kind, TokenKind::BlockEntry) {
            let mark = token.end;
            self.head_token();
            let token = self
                .peek_token(scanner, text, source, resources)?
                .ok_or_else(data_contract)?;
            if !matches!(
                token.kind,
                TokenKind::BlockEntry | TokenKind::Key | TokenKind::Value | TokenKind::BlockEnd
            ) {
                self.push_state(State::IndentlessSequenceEntry);
                return self.parse_node(scanner, text, source, resources, true, false);
            }
            self.state = State::IndentlessSequenceEntry;
            return self.process_empty_scalar_at(source, mark);
        }
        self.state = self.pop_state();
        self.close_collection(source, resources)?;
        Ok(None)
    }

    fn parse_block_mapping_key(
        &mut self,
        scanner: &mut Scanner,
        text: &[u8],
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
        first: bool,
    ) -> Result<Option<GraphEvent>, CodecError> {
        if first {
            let token = self
                .peek_token(scanner, text, source, resources)?
                .ok_or_else(data_contract)?;
            self.head_token(); // BLOCK-MAPPING-START
            let _ = token;
        }
        let token = self
            .peek_token(scanner, text, source, resources)?
            .ok_or_else(data_contract)?;
        if matches!(token.kind, TokenKind::Key) {
            let mark = token.end;
            self.head_token();
            let token = self
                .peek_token(scanner, text, source, resources)?
                .ok_or_else(data_contract)?;
            if !matches!(token.kind, TokenKind::Key | TokenKind::Value | TokenKind::BlockEnd) {
                self.push_state(State::BlockMappingValue);
                return self.parse_node(scanner, text, source, resources, true, true);
            }
            self.state = State::BlockMappingValue;
            return self.process_empty_scalar_at(source, mark);
        }
        // An implicit empty key: `: value` at the start of a block mapping has no saved simple key, so the scanner
        // emits only a VALUE token. Per the corpus's `: a` reading that is the empty key, then the value follows.
        if matches!(token.kind, TokenKind::Value) {
            let mark = token.start;
            self.head_token();
            self.state = State::BlockMappingValue;
            return self.process_empty_scalar_at(source, mark);
        }
        if matches!(token.kind, TokenKind::BlockEnd) {
            self.state = self.pop_state();
            self.head_token();
            self.close_collection(source, resources)?;
            return Ok(None);
        }
        Err(error::invalid(
            source,
            token.start,
            "block-mapping",
            "did not find expected key",
        ))
    }

    fn parse_block_mapping_value(
        &mut self,
        scanner: &mut Scanner,
        text: &[u8],
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<GraphEvent>, CodecError> {
        let token = self
            .peek_token(scanner, text, source, resources)?
            .ok_or_else(data_contract)?;
        if matches!(token.kind, TokenKind::Value) {
            let mark = token.end;
            self.head_token();
            let token = self
                .peek_token(scanner, text, source, resources)?
                .ok_or_else(data_contract)?;
            if !matches!(token.kind, TokenKind::Key | TokenKind::Value | TokenKind::BlockEnd) {
                self.push_state(State::BlockMappingKey);
                return self.parse_node(scanner, text, source, resources, true, true);
            }
            self.state = State::BlockMappingKey;
            return self.process_empty_scalar_at(source, mark);
        }
        self.state = State::BlockMappingKey;
        let offset = token.start;
        self.process_empty_scalar_at(source, offset)
    }

    // --- flow collections --------------------------------------------------

    fn parse_flow_sequence_entry(
        &mut self,
        scanner: &mut Scanner,
        text: &[u8],
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
        first: bool,
    ) -> Result<Option<GraphEvent>, CodecError> {
        if first {
            let token = self
                .peek_token(scanner, text, source, resources)?
                .ok_or_else(data_contract)?;
            self.head_token(); // FLOW-SEQUENCE-START
            let _ = token;
        }
        let token = self
            .peek_token(scanner, text, source, resources)?
            .ok_or_else(data_contract)?;
        if !matches!(token.kind, TokenKind::FlowSequenceEnd) {
            if !first {
                if matches!(token.kind, TokenKind::FlowEntry) {
                    self.head_token();
                } else {
                    return Err(error::invalid(
                        source,
                        token.start,
                        "flow-sequence",
                        "did not find expected ',' or ']'",
                    ));
                }
                let token = self
                    .peek_token(scanner, text, source, resources)?
                    .ok_or_else(data_contract)?;
                if matches!(token.kind, TokenKind::FlowSequenceEnd) {
                    self.state = self.pop_state();
                    self.head_token();
                    self.close_collection(source, resources)?;
                    return Ok(None);
                }
            }
            let token = self
                .peek_token(scanner, text, source, resources)?
                .ok_or_else(data_contract)?;
            if matches!(token.kind, TokenKind::Key) {
                // A flow sequence entry can be an implicit mapping `? key`.
                let start = token.start;
                let end = token.end;
                self.head_token();
                let node = self.graph.add_mapping(
                    None,
                    None,
                    Span::try_from_usize(start, end).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?,
                )?;
                self.open.push(OpenNode::FlowSequenceMapping {
                    node,
                    key: None,
                    entries: Vec::new(),
                });
                self.state = State::FlowSequenceEntryMappingKey;
                return Ok(Some(GraphEvent::Node));
            }
            // A flow sequence entry can be a single-pair implicit mapping with an EMPTY key: `[ : x ]` (the corpus's
            // reading) — the VALUE token opens the mapping whose empty scalar is the key and whose following node is
            // the value (no second VALUE token).
            if matches!(token.kind, TokenKind::Value) {
                let start = token.start;
                let end = token.end;
                self.head_token();
                let node = self.graph.add_mapping(
                    None,
                    None,
                    Span::try_from_usize(start, end).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?,
                )?;
                self.open.push(OpenNode::FlowSequenceMapping {
                    node,
                    key: None,
                    entries: Vec::new(),
                });
                self.push_state(State::FlowSequenceEntryMappingEnd);
                let _ = self.process_empty_scalar_at(source, start)?;
                return self.parse_node(scanner, text, source, resources, false, false);
            }
            if !matches!(token.kind, TokenKind::FlowSequenceEnd) {
                self.push_state(State::FlowSequenceEntry);
                return self.parse_node(scanner, text, source, resources, false, false);
            }
        }
        self.state = self.pop_state();
        self.head_token();
        self.close_collection(source, resources)?;
        Ok(None)
    }

    fn parse_flow_sequence_entry_mapping_key(
        &mut self,
        scanner: &mut Scanner,
        text: &[u8],
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<GraphEvent>, CodecError> {
        let token = self
            .peek_token(scanner, text, source, resources)?
            .ok_or_else(data_contract)?;
        if !matches!(
            token.kind,
            TokenKind::Value | TokenKind::FlowEntry | TokenKind::FlowSequenceEnd
        ) {
            self.push_state(State::FlowSequenceEntryMappingValue);
            return self.parse_node(scanner, text, source, resources, false, false);
        }
        if matches!(token.kind, TokenKind::FlowSequenceEnd) {
            let mark = token.start;
            self.state = State::FlowSequenceEntryMappingValue;
            return self.process_empty_scalar_at(source, mark);
        }
        let mark = token.end;
        self.head_token();
        self.state = State::FlowSequenceEntryMappingValue;
        self.process_empty_scalar_at(source, mark)
    }

    fn parse_flow_sequence_entry_mapping_value(
        &mut self,
        scanner: &mut Scanner,
        text: &[u8],
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<GraphEvent>, CodecError> {
        let token = self
            .peek_token(scanner, text, source, resources)?
            .ok_or_else(data_contract)?;
        if matches!(token.kind, TokenKind::Value) {
            self.head_token();
            let token = self
                .peek_token(scanner, text, source, resources)?
                .ok_or_else(data_contract)?;
            if !matches!(token.kind, TokenKind::FlowEntry | TokenKind::FlowSequenceEnd) {
                self.push_state(State::FlowSequenceEntryMappingEnd);
                return self.parse_node(scanner, text, source, resources, false, false);
            }
        }
        self.state = State::FlowSequenceEntryMappingEnd;
        let offset = token.start;
        self.process_empty_scalar_at(source, offset)
    }

    fn parse_flow_sequence_entry_mapping_end(
        &mut self,
        _scanner: &mut Scanner,
        _text: &[u8],
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<GraphEvent>, CodecError> {
        self.state = State::FlowSequenceEntry;
        self.close_collection(source, resources)?;
        Ok(None)
    }

    fn parse_flow_mapping_key(
        &mut self,
        scanner: &mut Scanner,
        text: &[u8],
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
        first: bool,
    ) -> Result<Option<GraphEvent>, CodecError> {
        if first {
            let token = self
                .peek_token(scanner, text, source, resources)?
                .ok_or_else(data_contract)?;
            self.head_token(); // FLOW-MAPPING-START
            let _ = token;
        }
        let token = self
            .peek_token(scanner, text, source, resources)?
            .ok_or_else(data_contract)?;
        if !matches!(token.kind, TokenKind::FlowMappingEnd) {
            if !first {
                if matches!(token.kind, TokenKind::FlowEntry) {
                    self.head_token();
                } else {
                    return Err(error::invalid(
                        source,
                        token.start,
                        "flow-mapping",
                        "did not find expected ',' or '}'",
                    ));
                }
                let token = self
                    .peek_token(scanner, text, source, resources)?
                    .ok_or_else(data_contract)?;
                if matches!(token.kind, TokenKind::FlowMappingEnd) {
                    self.state = self.pop_state();
                    self.head_token();
                    self.close_collection(source, resources)?;
                    return Ok(None);
                }
            }
            let token = self
                .peek_token(scanner, text, source, resources)?
                .ok_or_else(data_contract)?;
            if matches!(token.kind, TokenKind::Key) {
                let mark = token.end;
                self.head_token();
                let token = self
                    .peek_token(scanner, text, source, resources)?
                    .ok_or_else(data_contract)?;
                if !matches!(
                    token.kind,
                    TokenKind::Value | TokenKind::FlowEntry | TokenKind::FlowMappingEnd
                ) {
                    self.push_state(State::FlowMappingValue);
                    return self.parse_node(scanner, text, source, resources, false, false);
                }
                self.state = State::FlowMappingValue;
                return self.process_empty_scalar_at(source, mark);
            }
            // An implicit empty key: a `:` value indicator as a mapping entry (the corpus's `: bar` reading) — the
            // empty scalar is the key, and the following node is its value (no second VALUE token).
            if matches!(token.kind, TokenKind::Value) {
                let mark = token.start;
                self.head_token();
                self.push_state(State::FlowMappingKey);
                let _ = self.process_empty_scalar_at(source, mark)?;
                return self.parse_node(scanner, text, source, resources, false, false);
            }
            if !matches!(token.kind, TokenKind::FlowMappingEnd) {
                self.push_state(State::FlowMappingEmptyValue);
                return self.parse_node(scanner, text, source, resources, false, false);
            }
        }
        self.state = self.pop_state();
        self.head_token();
        self.close_collection(source, resources)?;
        Ok(None)
    }

    #[allow(
        unused_variables,
        reason = "the `empty` flag mirrors the reference's arm split; both arms now transition identically"
    )]
    fn parse_flow_mapping_value(
        &mut self,
        scanner: &mut Scanner,
        text: &[u8],
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
        empty: bool,
    ) -> Result<Option<GraphEvent>, CodecError> {
        let token = self
            .peek_token(scanner, text, source, resources)?
            .ok_or_else(data_contract)?;
        if matches!(token.kind, TokenKind::Value) {
            let mark = token.end;
            self.head_token();
            let token = self
                .peek_token(scanner, text, source, resources)?
                .ok_or_else(data_contract)?;
            if !matches!(token.kind, TokenKind::FlowEntry | TokenKind::FlowMappingEnd) {
                self.push_state(State::FlowMappingKey);
                return self.parse_node(scanner, text, source, resources, false, false);
            }
            self.state = State::FlowMappingKey;
            return self.process_empty_scalar_at(source, mark);
        }
        // The reference's `empty` arm transitions to the next-key state unconditionally and emits the empty scalar
        // WITHOUT consuming the current token (`,` or `}`); the next FLOW_MAPPING_KEY run consumes it. Leaving the
        // state here would re-enter this function on the same token forever.
        self.state = State::FlowMappingKey;
        let offset = token.start;
        self.process_empty_scalar_at(source, offset)
    }

    /// The empty-scalar production at a specific offset.
    fn process_empty_scalar_at(
        &mut self,
        source: ResolvedSource<'_>,

        offset: usize,
    ) -> Result<Option<GraphEvent>, CodecError> {
        let node = self.new_scalar(
            ScannedScalar::Owned(String::new()),
            ScalarStyle::Plain,
            offset,
            offset,
            None,
            None,
            source,
        )?;
        self.attach_node(node)?;
        Ok(Some(GraphEvent::Node))
    }

    /// Collects directives before a document start (the reference's `yaml_parser_process_directives`). The reference
    /// consumes directive tokens FIRST (duplicates among them are errors, the %YAML version must be 1.1 or 1.2), then
    /// appends the two DEFAULT tag directives with allow-duplicates semantics — an explicit `%TAG !` or `%TAG !!`
    /// replaces its default.
    fn collect_directives(
        &mut self,
        scanner: &mut Scanner,
        text: &[u8],
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        let mut seen_version = false;
        loop {
            let token = self
                .peek_token(scanner, text, source, resources)?
                .ok_or_else(data_contract)?;
            match token.kind {
                TokenKind::VersionDirective { major, minor } => {
                    self.head_token();
                    let _ = minor;
                    if major != 1 {
                        return Err(error::invalid(
                            source,
                            token.start,
                            "version-directive",
                            "found incompatible YAML document",
                        ));
                    }
                    if seen_version {
                        return Err(error::invalid(
                            source,
                            token.start,
                            "version-directive",
                            "found duplicate %YAML directive",
                        ));
                    }
                    seen_version = true;
                }
                TokenKind::TagDirective { handle, prefix } => {
                    self.head_token();
                    if self.tag_directives.iter().any(|d| d.handle == handle) {
                        return Err(error::invalid(
                            source,
                            token.start,
                            "tag-directive",
                            "found duplicate %TAG directive",
                        ));
                    }
                    self.tag_directives.push(TagDirective { handle, prefix });
                }
                TokenKind::ReservedDirective => {
                    self.head_token();
                }
                _ => break,
            }
        }
        // The default directives, appended with allow-duplicates semantics: `!` -> `!`, `!!` -> `tag:yaml.org,2002:`.
        self.append_default_directive("!", "!");
        self.append_default_directive("!!", "tag:yaml.org,2002:");
        let _ = resources;
        Ok(())
    }

    /// Appends a default tag directive unless a same-handle directive is already present.
    fn append_default_directive(&mut self, handle: &str, prefix: &str) {
        if !self.tag_directives.iter().any(|d| d.handle == handle) {
            self.tag_directives.push(TagDirective {
                handle: handle.to_owned(),
                prefix: prefix.to_owned(),
            });
        }
    }

    /// The implicit-document directive processing (the reference's `process_directives` with no directive tokens: only
    /// the defaults).
    fn process_directives(&mut self) {
        self.append_default_directive("!", "!");
        self.append_default_directive("!!", "tag:yaml.org,2002:");
    }
}

/// One poll's outcome.
pub(crate) enum ParserPoll {
    /// The work budget is exhausted; resume with a fresh cooperative entry.
    Pending,
    /// One graph event was produced.
    Event(GraphEvent),
}

#[cfg(test)]
mod tests {
    use super::*;
    use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
    use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

    fn resources<'a>() -> ResourceContext<'a> {
        ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                .expect("account"),
            &ContinueControl,
            WorkMeter::try_new_v1(4096).expect("work"),
        )
        .expect("context")
    }

    fn source(bytes: &'static [u8]) -> ResolvedSource<'static> {
        ResolvedSource::new(SourceRef::new(SourceId::new(1), SourceKind::Input), "t", bytes, 0)
    }

    /// Drives scanner+parser to completion, returning (`root_present`, events).
    fn parse_all(bytes: &'static [u8], resources: &mut ResourceContext<'_>) -> (bool, usize, Result<(), CodecError>) {
        let src = source(bytes);
        let decoded = crate::scan::DecodedSource::try_new(src, resources).expect("decoded");
        let start = decoded.start_offset();
        let mut scanner = crate::scan::Scanner::try_new(start);
        let mut parser = YamlParser::try_new(crate::provider::DialectKind::Core).expect("parser");
        let text = decoded.text(src);
        let mut events = 0usize;
        let mut error = Ok(());
        loop {
            match parser.poll(text, &mut scanner, src, resources) {
                Ok(ParserPoll::Pending) => {}
                Ok(ParserPoll::Event(event)) => {
                    events += 1;
                    if events > 1000 {
                        error = Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                            contract: "parse_all runaway",
                        }));
                        break;
                    }
                    if let GraphEvent::StreamEnd = event {
                        break;
                    }
                }
                Err(e) => {
                    error = Err(e);
                    break;
                }
            }
        }
        (parser.root().is_some(), events, error)
    }

    #[test]
    fn block_sequence_parses() {
        let mut resources = resources();
        // Dump the raw scanner token stream first.
        let src = source(b"- 1\n- 2\n");
        let decoded = crate::scan::DecodedSource::try_new(src, &resources).expect("decoded");
        let start = decoded.start_offset();
        let mut scanner = crate::scan::Scanner::try_new(start);
        let text = decoded.text(src);
        let mut dumped = alloc::string::String::new();
        let mut count = 0usize;
        while let crate::scan::ScanPoll::Token(token) = scanner.poll(text, src, &mut resources).expect("poll") {
            count += 1;
            assert!(count < 50, "scanner spin");
            dumped.push_str(&core::format_args!("{:?} ", token.kind).to_string());
        }
        let (root, events, error) = parse_all(b"- 1\n- 2\n", &mut resources);
        assert!(error.is_ok(), "tokens={dumped} error={error:?}");
        assert!(root, "root must be set; events={events} tokens={dumped}");
    }

    #[test]

    fn block_mapping_parses() {
        let mut resources = resources();
        let (root, events, error) = parse_all(b"name: Ada\nage: 37\n", &mut resources);
        assert!(error.is_ok(), "{error:?}");
        assert!(root, "root must be set; events={events}");
    }

    #[test]
    fn flow_collection_parses() {
        let mut resources = resources();
        let (root, events, error) = parse_all(b"[1, 2, 3]\n", &mut resources);
        assert!(error.is_ok(), "{error:?}");
        assert!(root, "root must be set; events={events}");
    }

    #[test]
    fn anchors_parse() {
        let mut resources = resources();
        // Dump the scanner token stream ALWAYS (before parsing).
        let src = source(b"base: &b {x: 1}\ncopy: *b\n");
        let decoded = crate::scan::DecodedSource::try_new(src, &resources).expect("d");
        let start = decoded.start_offset();
        let mut scanner = crate::scan::Scanner::try_new(start);
        let text = decoded.text(src);
        let mut dumped = alloc::string::String::new();
        while let crate::scan::ScanPoll::Token(token) = scanner.poll(text, src, &mut resources).expect("p") {
            dumped.push_str(&core::format_args!("{:?} ", token.kind).to_string());
        }
        let (root, events, error) = parse_all(b"base: &b {x: 1}\ncopy: *b\n", &mut resources);
        assert!(error.is_ok(), "tokens={dumped} error={error:?}");
        assert!(root, "tokens={dumped} root must be set; events={events}");
    }

    #[test]
    fn block_scalar_parses() {
        let mut resources = resources();
        let yaml = "text: |\n  line one\n  line two\n";
        let (root, events, error) = parse_all(yaml.as_bytes(), &mut resources);
        assert!(error.is_ok(), "{error:?}");
        assert!(root, "root must be set; events={events}");
    }

    #[test]
    fn tagged_scalar_parses() {
        let mut resources = resources();
        let (root, events, error) = parse_all(b"!money \"10\"\n", &mut resources);
        assert!(error.is_ok(), "{error:?}");
        assert!(root, "root must be set; events={events}");
    }

    #[test]
    fn parser_resets_between_documents() {
        let mut resources = resources();
        // Drive the parser through a two-document stream, resetting at each document end (the session's multi-yield
        // drive). The parser must produce a root for BOTH documents across the resets.
        let src = source(b"--- foo\n--- bar\n");
        let decoded = crate::scan::DecodedSource::try_new(src, &resources).expect("decoded");
        let start = decoded.start_offset();
        let mut scanner = crate::scan::Scanner::try_new(start);
        let mut parser = YamlParser::try_new(crate::provider::DialectKind::Core).expect("parser");
        let text = decoded.text(src);
        let mut roots = 0usize;
        let mut guard = 0usize;
        loop {
            guard += 1;
            assert!(guard <= 1000, "parser spin in reset test");
            match parser.poll(text, &mut scanner, src, &mut resources).expect("poll") {
                ParserPoll::Event(GraphEvent::StreamEnd) => break,
                ParserPoll::Event(GraphEvent::DocumentEnd) => {
                    // The session builds the completed document and resets.
                    assert!(parser.root().is_some(), "root must be set at doc end");
                    parser.reset_after_document().expect("reset");
                    roots += 1;
                }
                ParserPoll::Pending | ParserPoll::Event(_) => {}
            }
        }
        assert_eq!(roots, 2, "two document-end boundaries expected");
    }

    #[test]
    fn parser_continues_after_reset() {
        let mut resources = resources();
        let src = source(b"--- foo\n--- bar\n");
        let decoded = crate::scan::DecodedSource::try_new(src, &resources).expect("decoded");
        let start = decoded.start_offset();
        let mut scanner = crate::scan::Scanner::try_new(start);
        let mut parser = YamlParser::try_new(crate::provider::DialectKind::Core).expect("parser");
        let text = decoded.text(src);
        let mut first_doc_end = false;
        let mut guard = 0usize;
        loop {
            guard += 1;
            assert!(guard <= 1000, "spin");
            match parser.poll(text, &mut scanner, src, &mut resources).expect("poll") {
                ParserPoll::Event(GraphEvent::StreamEnd) => {
                    assert!(first_doc_end, "doc end must have been seen before stream end");
                    break;
                }
                ParserPoll::Event(GraphEvent::DocumentEnd) => {
                    if !first_doc_end {
                        first_doc_end = true;
                        parser.reset_after_document().expect("reset");
                    }
                }
                ParserPoll::Pending | ParserPoll::Event(_) => {}
            }
        }
        assert!(first_doc_end, "one doc end expected");
    }

    #[test]
    fn session_yields_two_documents() {
        // A `---` unit is ONE document: every session publishes one document per decode and resets for the next.
        // `decode_documents` drives this mode.
        let mut resources = resources();
        let src = source(b"--- foo\n--- bar\n");
        let mut state = YamlParseState::try_new(
            src,
            crate::provider::DialectKind::Core,
            None,
            jqf_data::BuilderCoverage::minimal_semantic().with_attached_facts(true),
            true,
            &resources,
        )
        .expect("state");
        let mut docs = 0usize;
        let mut ctx = jqf_codec_core::CodecRunContext::new(&mut resources);
        // Straight-line decode: one `decode` call yields one document; the session resets internally for the next, and
        // the trailing-StreamEnd decode (the poll-era `Complete`) sets `stream_done` and is the loop's clean stop.
        while !state.stream_done {
            match state.decode(jqf_codec_core::AccessInput::Source(src), &mut ctx) {
                Ok(result) => {
                    let jqf_codec_core::AccessOutcome::FullDocument(product) = result.outcome() else {
                        panic!("not a full document");
                    };
                    let _ = product;
                    docs += 1;
                    assert!(docs <= 10, "too many docs");
                }
                Err(_) if state.stream_done => break,
                Err(error) => panic!("unexpected decode failure: {error:?}"),
            }
        }
        assert_eq!(docs, 2, "two documents expected");
    }

    #[test]
    fn every_session_publishes_one_document_per_unit_and_reports_its_end() {
        // A `---` unit is ONE document on EVERY route: the first decode publishes document 1 and reports where it
        // ended; the SDK's sequence drive reopens at that offset, where document 2 decodes as its own stream.
        let bytes: &'static [u8] = b"--- a\n--- b\n";
        let mut resources = resources();
        let src = source(bytes);
        let mut state = YamlParseState::try_new(
            src,
            crate::provider::DialectKind::Core,
            None,
            jqf_data::BuilderCoverage::minimal_semantic().with_attached_facts(true),
            true,
            &resources,
        )
        .expect("state");
        let mut ctx = jqf_codec_core::CodecRunContext::new(&mut resources);
        let result = state
            .decode(jqf_codec_core::AccessInput::Source(src), &mut ctx)
            .expect("document 1 decodes");
        let consumed = result
            .report()
            .consumed_offset()
            .expect("every publish reports its consumed offset");
        assert_eq!(
            usize::try_from(consumed).expect("fits usize"),
            6,
            "document 1 ends where the second unit's `---` begins"
        );
        // The reopen-at-offset law: the tail is its own stream whose first document is document 2.
        let tail = &bytes[usize::try_from(consumed).expect("fits usize")..];
        let (root, _events, error) = parse_all(tail, &mut resources);
        assert!(error.is_ok(), "the reopened tail must parse: {error:?}");
        assert!(root, "the reopened tail must yield document 2");
    }

    #[test]
    fn a_four_dot_line_is_a_second_document_served_not_rejected() {
        // A document-end marker is EXACTLY `...` + separation; a 4+-dot line is plain-scalar content of a bare second
        // document. One item per unit: document 1 publishes, and the 4-dot line is the next unit's content — reachable
        // through the reported consumed offset, never a rejection.
        let bytes: &'static [u8] = b"--- a\n...\n....\n";
        let mut resources = resources();
        let src = source(bytes);
        let mut state = YamlParseState::try_new(
            src,
            crate::provider::DialectKind::Core,
            None,
            jqf_data::BuilderCoverage::minimal_semantic().with_attached_facts(true),
            true,
            &resources,
        )
        .expect("state");
        let mut ctx = jqf_codec_core::CodecRunContext::new(&mut resources);
        let result = state
            .decode(jqf_codec_core::AccessInput::Source(src), &mut ctx)
            .expect("document 1 decodes");
        let consumed = result
            .report()
            .consumed_offset()
            .expect("every publish reports its consumed offset");
        let tail = &bytes[usize::try_from(consumed).expect("fits usize")..];
        let (root, _events, error) = parse_all(tail, &mut resources);
        assert!(error.is_ok(), "the 4-dot unit must parse as document 2");
        assert!(root, "the 4-dot line is document 2's bare scalar");
    }

    #[test]
    fn trailing_comments_are_filler_not_a_second_document() {
        // Stream filler after the document (whitespace, `#` comment lines, and redundant `...` marker lines) is not a
        // second document. The implicit-end input's boundary lands at EOF (the scanner skips the filler before
        // StreamEnd), so the EXPLICIT `...` variant is the one that actually exercises the boundary's comment branch.
        for bytes in [
            &b"--- a\n# trailing comment\n\n"[..],
            &b"--- a\n...\n# trailing comment\n"[..],
            &b"--- a\n...\n...\n"[..],
        ] {
            let mut resources = resources();
            let src = source(bytes);
            let mut state = YamlParseState::try_new(
                src,
                crate::provider::DialectKind::Core,
                None,
                jqf_data::BuilderCoverage::minimal_semantic().with_attached_facts(true),
                true,
                &resources,
            )
            .expect("state");
            let mut ctx = jqf_codec_core::CodecRunContext::new(&mut resources);
            let result = state
                .decode(jqf_codec_core::AccessInput::Source(src), &mut ctx)
                .expect("decode");
            assert!(
                matches!(result.outcome(), jqf_codec_core::AccessOutcome::FullDocument(_)),
                "trailing comments / marker lines are filler, not a second document, for {bytes:?}"
            );
        }
    }

    /// Drives one session to completion, collecting each `Ready` result's `consumed_offset`.
    fn collect_consumed_offsets(src: ResolvedSource<'static>, resources: &mut ResourceContext<'_>) -> Vec<u64> {
        let mut state = YamlParseState::try_new(
            src,
            crate::provider::DialectKind::Core,
            None,
            jqf_data::BuilderCoverage::minimal_semantic().with_attached_facts(true),
            true,
            resources,
        )
        .expect("state");
        let mut offsets = Vec::new();
        let mut guard = 0usize;
        while !state.stream_done {
            guard += 1;
            assert!(guard <= 1000, "session spin");
            let mut ctx = jqf_codec_core::CodecRunContext::new(resources);
            match state.decode(jqf_codec_core::AccessInput::Source(src), &mut ctx) {
                Ok(result) => {
                    let offset = result
                        .report()
                        .consumed_offset()
                        .expect("adjacent-value publish must report a consumed offset");
                    offsets.push(offset);
                }
                Err(_) if state.stream_done => break,
                Err(error) => panic!("unexpected decode failure: {error:?}"),
            }
        }
        offsets
    }

    #[test]
    fn every_published_document_reports_a_consumed_offset() {
        // Every published document reports where it ends, strictly increasing, with the last landing exactly at the
        // source's end — the same shape the SDK's reopen-at-offset sequence drive expects from JSON's adjacent-value
        // mode.
        let bytes: &'static [u8] = b"--- foo\n--- bar\n";
        let mut resources = resources();
        let offsets = collect_consumed_offsets(source(bytes), &mut resources);
        assert_eq!(offsets.len(), 2, "two documents, two offsets");
        assert!(offsets[0] < offsets[1], "offsets must strictly increase: {offsets:?}");
        assert_eq!(
            offsets[1],
            u64::try_from(bytes.len()).expect("len fits u64"),
            "the last document's offset must reach the source's end"
        );
    }

    #[test]
    fn the_reported_offset_reopens_at_the_next_document() {
        // The offset reported for document 1 must be a valid reopen point: re-slicing the ORIGINAL bytes there and
        // parsing that slice as its own standalone stream must decode document 2 — exactly what the SDK's
        // `ErasedProvider::open_at` does at each reported offset.
        let bytes: &'static [u8] = b"--- foo\n--- bar\n";
        let mut resources = resources();
        let offsets = collect_consumed_offsets(source(bytes), &mut resources);
        let reopened = &bytes[usize::try_from(offsets[0]).expect("fits usize")..];
        let (root, events, error) = parse_all(reopened, &mut resources);
        assert!(error.is_ok(), "reopened slice must parse: {error:?}");
        assert!(root, "reopened slice must yield a root; events={events}");
    }

    #[test]
    fn explicit_end_markers_keep_their_offsets() {
        // The explicit `...` boundary shape reports the marker's own end, not the next document's start; the reopened
        // slice must still decode the following document correctly.
        let bytes: &'static [u8] = b"--- foo\n...\n--- bar\n...\n";
        let mut resources = resources();
        let offsets = collect_consumed_offsets(source(bytes), &mut resources);
        assert_eq!(offsets.len(), 2, "two documents, two offsets");
        assert!(offsets[0] < offsets[1]);
        let reopened = &bytes[usize::try_from(offsets[0]).expect("fits usize")..];
        let (root, events, error) = parse_all(reopened, &mut resources);
        assert!(error.is_ok(), "reopened slice must parse: {error:?}");
        assert!(root, "reopened slice must yield a root; events={events}");
    }

    #[test]
    fn empty_stream_accounts_for_the_whole_source() {
        // Comment-only and bare-`...` streams are valid empty streams (§4.8) — the whole-document and sequence routes
        // model them as the bare EMPTY DOCUMENT (a null scalar under core), and every published document reports where
        // it ended: the whole source. Without the full-source consumed offset the SDK's reopen-at-offset sequence drive
        // fails its forward-progress guard with "opened without reporting a consumed offset".
        for bytes in [
            b"# c\n".as_slice(),
            b"# only comment\n# another\n".as_slice(),
            b"...\n".as_slice(),
        ] {
            let mut resources = resources();
            let offsets = collect_consumed_offsets(source(bytes), &mut resources);
            assert_eq!(
                offsets.len(),
                1,
                "one empty document, one offset: {bytes:?} {offsets:?}"
            );
            assert_eq!(
                offsets[0],
                u64::try_from(bytes.len()).expect("len fits u64"),
                "the empty stream's single Ready must consume the whole source"
            );
        }
    }

    #[test]
    fn explicit_empty_document_keeps_its_consumed_offset() {
        // Guard: `---\n...\n` is the EXPLICIT empty document (one empty plain scalar, core → null), not an empty
        // stream. It already worked before the empty-stream fix and must stay byte-for-byte unchanged: one Ready whose
        // offset reaches the source end.
        let bytes: &'static [u8] = b"---\n...\n";
        let mut resources = resources();
        let offsets = collect_consumed_offsets(source(bytes), &mut resources);
        assert_eq!(offsets.len(), 1, "one explicit empty document: {offsets:?}");
        assert_eq!(
            offsets[0], 7,
            "the explicit empty document consumes through the trailing `...`; the final newline is trivia the drive's separator skips"
        );
    }
}
