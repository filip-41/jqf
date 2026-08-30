//! The jqft text grammar parser and the jqfjson strict-JSON envelope parser.
//!
//! ONE resumable value parser in two modes. The jqft mode implements the `%jqft 1` header, `---` document streams, core
//! scalars (null/bool/exact number/binary64-by-`f`-suffix/string/bytes/temporal literals), `@tag("name")` layers
//! retained as [`jqf_data::Value::Tagged`] chains, comments attached as facts when the bound requirement demands them
//! (see `ensure_builder`; they are not the value), bare keys and trailing commas. Markup nodes are first-class — a
//! `<name &attr="v">children</name>` element decodes as a role-tagged array of its children, with the node name, the
//! attribute map, and the content attached as facts when demanded — while every
//! reserved spelling (anchors and aliases, namespaced markup names) is refused with a dedicated diagnostic. The jqfjson
//! mode is strict JSON (RFC 8259 member-name and number laws), one document per source.
//!
//! The parser is ITERATIVE (an explicit container stack), so document nesting depth costs heap, not call stack — a
//! deeply nested jqft document cannot overflow the request thread's stack.

use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use jqf_codec_core::byte_scan::{StopSet, StringContent, Ws, prefix_len};
use jqf_codec_core::{
    AccessInput, AccessOutcome, AccessResult, CodecError, CodecFailureKind, CodecRunContext, DocumentProduct,
};
use jqf_data::{
    AccountedDocumentBuilder, AccountedDocumentFinalizer, AccountedIntrinsicTag, AccountedOccurrenceKey,
    AccountedSemanticNode, AuthoritativeEmptyFamilies, BuilderCoverage, DataError, DiagnosticCoverage,
    DocumentCapabilityFamily, DocumentFinalizationPoll, DocumentTextId, FactPayload, LocalDate, LocalOwnerRef,
    LocalTime, NodeId, TagId,
};
use jqf_resource::{ResourceContext, WorkAdmission};
use jqf_source::ResolvedSource;

use crate::error;
use crate::provider::{self, JqftKind};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Step {
    /// The jqft header line is still expected.
    Header,
    /// Parse one value token; a completed value is attached by [`JqftParseState::attach`].
    Value,
    /// After `[`: a value (or `]` for an empty array).
    ArrayValueOrEnd,
    ArrayCommaOrEnd,
    ObjectKeyOrEnd,
    ObjectColon,
    ObjectCommaOrEnd,
    /// After `(` at a bracket key position: parse the key token, expect `)`.
    ObjectBracketKey,
    /// After `<`: the markup node name.
    MarkupName,
    /// In the markup body: attributes, children, or `>`.
    MarkupBody,
    /// After `&` in a markup body: the attribute name (or its bracket form).
    MarkupAttrName,
    /// After an attribute name: its `="…"` value.
    MarkupAttrValue,
    /// After the root: skip whitespace/comments; jqft expects `---` or EOF, jqfjson requires EOF.
    Trailing,
}

/// One open container on the explicit stack.
enum Frame {
    Array {
        owner: NodeId,
        /// Whether at least one item was attached (a leading comma is an error; a trailing comma is a jqft feature).
        has_items: bool,
    },
    Object {
        owner: NodeId,
        /// The decoded key awaiting its value (set between `:` and the value).
        pending_key: Option<DocumentTextId>,
    },
    /// A markup node (`<name &attr="v" children…>`, the angle form of the markup grammar): the owner is an ARRAY of its
    /// ordered children (the array model — children are reached by `.[]`, never a `.@children` fact), and the
    /// name/attrs/content facts are attached at the close.
    Markup {
        owner: NodeId,
        name: String,
        attributes: Vec<(String, String)>,
        content: String,
    },
}

/// The §3.15 comment roles the jqft line-comment grammar can produce. `trailing` and `inner` are unreachable from a `#`
/// line-comment grammar and stay absent (`[]`) in every consolidated payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CommentRole {
    Leading,
    Inline,
    Detached,
}

/// The attachment routing for one completed value node.
enum Parent {
    Root,
    Array(NodeId),
    Object(NodeId, Option<DocumentTextId>),
    Markup(NodeId),
}

enum Phase {
    Seal,
    Parse,
    Finalize,
    Publish,
}

pub(crate) struct JqftParseState {
    kind: JqftKind,
    schema_prefix: &'static str,
    cursor: usize,
    phase: Phase,
    step: Step,
    frames: Vec<Frame>,
    /// Accumulated `@tag("name")` layers awaiting their payload (jqft mode).
    pending_tags: alloc::vec::Vec<(usize, TagId)>,
    /// The frame depth at which the pending tags were read: the tags wrap the value that completes AT that depth — a
    /// container's elements complete deeper and must not consume them. Comments awaiting their owner (§3.15): an
    /// own-line comment is a LEADING comment of the next completed node at or above the depth where it was read (string
    /// children inside markup are exempt — they have no comment positions). The depth is recorded at read time so a
    /// comment inside a nested container or markup body attaches to the node that ENCLOSES it, never to a sibling
    /// completed deeper.
    pending_leading_comments: Vec<PendingComment>,
    /// The most recently completed node: the inline-comment owner (a comment on the same logical line as a completed
    /// value is that value's `jqf.inline` comment).
    last_node: Option<NodeId>,
    /// Every comment event in source order: (owner node, role, text). One consolidated `jqft.comment@1` fact per node
    /// is attached at finalize.
    comment_events: Vec<(NodeId, CommentRole, String)>,
    /// The pending markup attribute name awaiting its `="…"` value.
    markup_pending_attr: Option<String>,
    root: Option<NodeId>,
    builder: Option<AccountedDocumentBuilder<'static>>,
    /// Cached recipe: rebuilt never across adjacent documents of one session.
    recipe: jqf_data::DocumentSchemaRecipe<'static>,
    /// Builder coverage the decode honours: skip attaching comments/markup facts unless demanded.
    coverage: BuilderCoverage,
    /// The in-flight cooperative source seal, pending before the parse begins (the document retains its source
    /// authority).
    binding_stage: Option<jqf_data::DocumentSourceBindingStage>,
    finalizer: Option<AccountedDocumentFinalizer<'static>>,
    product: Option<DocumentProduct<'static>>,
    published: bool,
    allow_adjacent_values: bool,
    /// The source offset at which the parsed document's extent ends (the adjacent-value consumed-offset report).
    document_end: usize,
    /// Decoded string / key scratch (reused across tokens).
    scratch: Vec<u8>,
}

/// One own-line comment awaiting its owner: the text and the frame depth at which it was read (see
/// [`JqftParseState::pending_leading_comments`]).
struct PendingComment {
    text: String,
    depth: usize,
}

impl JqftParseState {
    /// Builds this session's fresh document builder against the CURRENT source, if none exists yet.
    ///
    /// The builder binds its source eagerly (the `--with-source` echo's authority), so a REUSED session must rebuild it
    /// from the new value's source — a stale binding from a previous value could never serve it. The construction is
    /// lazy: a fresh `try_new` and a reset both start with `builder: None`, and the first parse poll builds it from the
    /// poll's source, so the two paths share one law.
    fn try_ensure_builder(&mut self, source: ResolvedSource<'_>) -> Result<(), CodecError> {
        if self.builder.is_some() {
            return Ok(());
        }
        // A FRESH builder from the validated static recipe (not the shared prototype builder): the dynamic
        // `add_node`/`add_occurrence` path this parser uses is refused by the shared-schema prototype builder (the CBOR
        // decoder precedent).
        //
        // Coverage follows the bound requirement: identity `.` skips comments/markup facts; `.@comment` and Preserve
        // attach them. Source attachment happens at publish and does not widen builder coverage.
        let (mut builder, _schema) =
            AccountedDocumentBuilder::try_new_prepared_with_coverage(&self.recipe, self.coverage).map_err(map_data)?;
        // The document retains its source authority (the input bytes) so `with_source` can echo the origin format
        // byte-identically (the memo's conformance level 1). The XML codec's precedent: bind the session source,
        // codec-core attaches the backing at publish. The seal is a linear pass, so it is started here as a cooperative
        // stage and driven to completion in the Seal phase before the parse begins (hash off — every consumer of this
        // binding reads through metadata-checked access, never re-verifying the digest).
        let binding_stage = jqf_data::DocumentSourceBindingStage::new(source).map_err(map_data)?;
        builder.set_authoritative_empty_families(AuthoritativeEmptyFamilies::from_family(
            DocumentCapabilityFamily::Attributes,
        ));
        builder.set_diagnostic_coverage(DiagnosticCoverage::NotRequested);
        self.builder = Some(builder);
        // The stage seals the SAME source the builder was just built from (the cooperative seal, hash off). Drive it to
        // completion in the Seal phase before the parse begins; the poll loops back into it via the phase transition
        // below.
        self.binding_stage = Some(binding_stage);
        self.phase = Phase::Seal;
        Ok(())
    }

    /// Rebinds builder coverage when the requirement changes on the reopen path.
    pub(crate) fn set_coverage(&mut self, coverage: BuilderCoverage) {
        self.coverage = coverage;
    }

    /// Reinitializes this state for one more adjacent document, leaving exactly the state a fresh [`Self::try_new`]
    /// over the next document's source would have produced.
    ///
    /// Everything that belongs to one document is dropped — cursor, frames, comment state, the builder (rebuilt lazily
    /// from the new value's source on the first parse poll), the finalizer, the product — so a document that failed
    /// mid-parse leaves nothing behind for the next one. A reused session never sits at stream start, so the step is
    /// always `Value` (the `%jqft 1` header belongs to the first document only, exactly as a fresh `try_new` computes
    /// it from the source's base offset).
    pub(crate) fn try_reset(&mut self) {
        self.cursor = 0;
        self.phase = Phase::Parse;
        self.step = Step::Value;
        self.frames.clear();
        self.pending_tags.clear();
        self.pending_leading_comments.clear();
        self.last_node = None;
        self.comment_events.clear();
        self.markup_pending_attr = None;
        self.root = None;
        self.builder = None;
        self.binding_stage = None;
        self.finalizer = None;
        self.product = None;
        self.published = false;
        self.document_end = 0;
        self.scratch.clear();
    }

    pub(crate) fn try_new(
        source: ResolvedSource<'_>,
        kind: JqftKind,
        allow_adjacent_values: bool,
        coverage: BuilderCoverage,
    ) -> Self {
        // The header is a stream-start fact: a reopened session (the SDK's adjacent-value sequence drive opens each
        // document at its own offset) never sees the header again.
        let at_stream_start = source.base_offset() == 0;
        Self {
            kind,
            schema_prefix: kind.schema_prefix(),
            cursor: 0,
            // Both the builder and its source-binding stage are constructed lazily on the first parse poll from the
            // poll's source (see [`Self::try_ensure_builder`], which transitions to the Seal phase once they exist), so
            // a reused session binds the NEW value's source. Entering the Seal phase with a missing stage would be a
            // contract violation.
            phase: Phase::Parse,
            step: if kind.is_jqft() && at_stream_start {
                Step::Header
            } else {
                Step::Value
            },
            frames: Vec::new(),
            pending_tags: Vec::new(),
            pending_leading_comments: Vec::new(),
            last_node: None,
            comment_events: Vec::new(),
            markup_pending_attr: None,
            root: None,
            builder: None,
            recipe: provider::jqft_recipe(kind).expect("jqft recipe is a static identity table"),
            coverage,
            binding_stage: None,
            finalizer: None,
            product: None,
            published: false,
            allow_adjacent_values,
            document_end: 0,
            scratch: Vec::new(),
        }
    }
}

impl jqf_codec_core::AccessSession for JqftParseState {
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
    fn decode<'source>(
        &mut self,
        input: AccessInput<'_, 'source>,
        context: &mut CodecRunContext<'_, '_>,
    ) -> Result<AccessResult<'source>, CodecError> {
        let AccessInput::Source(source) = input else {
            return Err(CodecError::new(CodecFailureKind::ProviderRouteMismatch));
        };
        if self.published {
            return Err(data_contract());
        }
        loop {
            match self.phase {
                Phase::Seal => {
                    let stage = self.binding_stage.as_mut().ok_or_else(data_contract)?;
                    // SAFETY: codec-core retains one immutable source
                    // authority for the complete access session and passes that exact authority each decode; the stage
                    // was constructed over the same segment and re-verifies identity on every call.
                    match unsafe { stage.poll(source, context.resources()) }.map_err(map_data)? {
                        jqf_data::DocumentSourceBindingPoll::Pending => {
                            context.replenish_work()?;
                        }
                        jqf_data::DocumentSourceBindingPoll::Ready(binding) => {
                            self.binding_stage = None;
                            let builder = self.builder.as_mut().ok_or_else(data_contract)?;
                            builder.bind_source(binding).map_err(map_data)?;
                            self.phase = Phase::Parse;
                        }
                    }
                }
                Phase::Parse => {
                    // The builder is constructed lazily so a reused session binds the CURRENT value's source; a fresh
                    // session builds it on its first decode from the same source it was opened over.
                    if self.builder.is_none() {
                        self.try_ensure_builder(source)?;
                    }
                    let progress = self.parse_step(source, context.resources())?;
                    if !progress {
                        context.replenish_work()?;
                    }
                }
                Phase::Finalize => {
                    if self.finalizer.is_none() {
                        self.attach_comment_facts(context.resources())?;
                        let root = self.root.take().ok_or_else(data_contract)?;
                        let builder = self.builder.take().ok_or_else(data_contract)?;
                        self.finalizer = Some(builder.begin_finish(root, context.resources()).map_err(map_data)?);
                    }
                    let finalizer = self.finalizer.as_mut().ok_or_else(data_contract)?;
                    let poll = finalizer.poll(context.resources()).map_err(map_data)?;
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
                    // SAFETY: codec-core owns this exact immutable source for
                    // the whole access session; the decoder sealed against it (the source binding) and every span names
                    // it.
                    let product =
                        unsafe { product.attach_borrowed_source_from_access_session(source, context.resources())? };
                    self.published = true;
                    let outcome = AccessOutcome::FullDocument(product);
                    if !self.allow_adjacent_values {
                        return Ok(AccessResult::from_outcome(outcome));
                    }
                    let consumed = u64::try_from(self.document_end).unwrap_or(u64::MAX);
                    return Ok(AccessResult::from_outcome_with_consumed_offset(outcome, consumed));
                }
            }
        }
    }
}

impl JqftParseState {
    /// Longest prefix of `bytes` that is JSON whitespace (space, tab, LF, CR) — the jqft whitespace set is
    /// byte-identical to json's `Ws` set.
    ///
    /// Tiered for the shape the canonical renderer produces: single spaces (`: `, ` # `) and one-byte line breaks exit
    /// after one compare; a run of eight or more bytes — deep indentation — engages the arch-width kernel from
    /// `jqf-codec-core::byte_scan`, which skips whole 16-byte lanes. The scalar tail finishes the boundary lane, so the
    /// first non-whitespace byte is found exactly.
    #[expect(
        clippy::match_same_arms,
        reason = "end-of-input and first-non-whitespace both answer `n`: the run ended, and the \
                  answer is the same either way"
    )]
    fn ws_prefix_len(bytes: &[u8]) -> usize {
        let mut n = 0;
        while n < 8 {
            match bytes.get(n) {
                None => return n,
                Some(&byte) if Ws::hit(byte) => n += 1,
                Some(_) => return n,
            }
        }
        8 + prefix_len::<Ws>(&bytes[8..])
    }

    /// Longest prefix of `bytes` containing no byte that ends or escapes a JSON-shaped string (`"`, `\`, C0 controls).
    /// Non-ASCII bytes are content — the run this delimits is the block copied verbatim into the decoded string,
    /// exactly json's `StringContent` set.
    fn string_content_prefix_len(bytes: &[u8]) -> usize {
        prefix_len::<StringContent>(bytes)
    }

    /// One bounded parse step; `false` means the work budget is exhausted.
    fn parse_step(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        if resources.admit_work_transition()? == WorkAdmission::Pending {
            return Ok(false);
        }
        match self.step {
            Step::Header => self.step_header(source),
            Step::Value => self.step_value(source, resources),
            Step::ArrayValueOrEnd => self.step_array_value_or_end(source, resources),
            Step::ArrayCommaOrEnd => self.step_array_comma_or_end(source, resources),
            Step::ObjectKeyOrEnd => self.step_object_key_or_end(source, resources),
            Step::ObjectColon => self.step_object_colon(source),
            Step::ObjectCommaOrEnd => self.step_object_comma_or_end(source, resources),
            Step::ObjectBracketKey => self.step_object_bracket_key(source, resources),
            Step::MarkupName => self.step_markup_name(source, resources),
            Step::MarkupBody => self.step_markup_body(source, resources),
            Step::MarkupAttrName => self.step_markup_attr_name(source),
            Step::MarkupAttrValue => self.step_markup_attr_value(source),
            Step::Trailing => self.step_trailing(source),
        }
    }

    /// Skips whitespace and (jqft mode) comments; returns the first significant byte or `None` at end of input.
    fn skip_trivia(&mut self, source: ResolvedSource<'_>) -> Result<Option<u8>, CodecError> {
        let bytes = source.bytes();
        loop {
            let run_start = self.cursor;
            // The JSON whitespace set, in bulk: formatted jqft text's indentation and `: `/`,\n` runs are passed whole
            // instead of one bounds-checked byte per step. The comment and markup grammar stay on the scalar path
            // below.
            self.cursor += Self::ws_prefix_len(&bytes[run_start..]);
            if self.kind.is_jqft() && bytes.get(self.cursor) == Some(&b'#') {
                // §3.15: a `#` comment is a portable comment fact, not discarded trivia. Text extraction: remove the
                // delimiter and line terminator, then exactly one immediately following ASCII space (the portfolio
                // law). Same-logical-line (no newline since the last significant token) is `jqf.inline` on the PREVIOUS
                // completed node; an own-line comment is `jqf.leading` on the NEXT completed node; a document-trailer
                // comment with no following node becomes `jqf.detached` on the root at `step_trailing`. The newline
                // fact is read lazily from the just-scanned run: it is consumed only here, so the common comment-free
                // path pays no second pass over the whitespace.
                let newline_seen = bytes[run_start..self.cursor].contains(&b'\n');
                self.cursor += 1;
                let text_start = self.cursor;
                let mut consumed_newline = false;
                while let Some(byte) = bytes.get(self.cursor).copied() {
                    self.cursor += 1;
                    if byte == b'\n' {
                        consumed_newline = true;
                        break;
                    }
                }
                let end = if consumed_newline { self.cursor - 1 } else { self.cursor };
                let validated = core::str::from_utf8(&bytes[text_start..end]).map_err(|_| {
                    error::invalid(source, text_start, "invalid-comment", "a comment is not valid UTF-8")
                })?;
                let mut text = validated.strip_suffix('\r').unwrap_or(validated);
                if text.starts_with(' ') {
                    text = &text[1..];
                }
                if self.coverage.attached_facts() {
                    if newline_seen {
                        self.pending_leading_comments.push(PendingComment {
                            text: String::from(text),
                            depth: self.frames.len(),
                        });
                    } else if let Some(owner) = self.last_node {
                        self.comment_events
                            .push((owner, CommentRole::Inline, String::from(text)));
                    } else {
                        // A comment before any node has no previous owner; it is a leading comment of whatever comes next.
                        self.pending_leading_comments.push(PendingComment {
                            text: String::from(text),
                            depth: self.frames.len(),
                        });
                    }
                }
                continue;
            }
            return Ok(bytes.get(self.cursor).copied());
        }
    }

    fn step_header(&mut self, source: ResolvedSource<'_>) -> Result<bool, CodecError> {
        const HEADER: &[u8] = b"%jqft 1";
        let bytes = source.bytes();
        if !bytes.starts_with(HEADER) {
            return Err(error::invalid(
                source,
                0,
                "missing-header",
                "a jqft document must begin with the %jqft 1 directive line",
            ));
        }
        self.cursor = HEADER.len();
        match bytes.get(self.cursor) {
            Some(b'\n') => self.cursor += 1,
            Some(b'\r') if bytes.get(self.cursor + 1) == Some(&b'\n') => self.cursor += 2,
            Some(_) => {
                return Err(error::invalid(
                    source,
                    0,
                    "malformed-header",
                    "the %jqft 1 directive must occupy its own line",
                ));
            }
            None => {}
        }
        self.step = Step::Value;
        Ok(true)
    }

    fn step_value(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        self.skip_trivia(source)?;
        let Some(byte) = source.bytes().get(self.cursor).copied() else {
            return Err(self.expected_value(source));
        };
        match byte {
            b'{' => {
                let owner = self.open_object(resources)?;
                self.frames.push(Frame::Object {
                    owner,
                    pending_key: None,
                });
                self.step = Step::ObjectKeyOrEnd;
                Ok(true)
            }
            b'[' => {
                let owner = self.open_array(resources)?;
                self.frames.push(Frame::Array {
                    owner,
                    has_items: false,
                });
                self.step = Step::ArrayValueOrEnd;
                Ok(true)
            }
            b'"' => {
                self.scan_string(source)?;
                let node = self.with_scratch(|this, value| {
                    this.add_scalar(source, AccountedSemanticNode::String(value), resources)
                })?;
                self.attach(source, node, resources)
            }
            // A trailing comma's array close (jqft only): after a `,` the next significant byte may be `]`. Object
            // trailing commas close from `ObjectKeyOrEnd`, not here — `{a: }` is a missing value.
            b']' if self.kind.is_jqft() && matches!(self.frames.last(), Some(Frame::Array { .. })) => {
                self.close_array(source, resources)
            }
            b'@' if self.kind.is_jqft() => self.step_tag(source),
            b'<' if self.kind.is_jqft() => {
                self.cursor += 1;
                self.step = Step::MarkupName;
                Ok(true)
            }
            b'&' | b'*' if self.kind.is_jqft() => Err(error::invalid(
                source,
                self.cursor,
                "reserved-anchor",
                "anchors and aliases are reserved for the YAML vertical",
            )),
            _ if self.kind.is_jqft()
                && byte == b'0'
                && source.bytes().get(self.cursor + 1) == Some(&b'x')
                && source.bytes().get(self.cursor + 2) == Some(&b'"') =>
            {
                self.cursor += 2;
                let value = self.scan_bytes_literal(source, true)?;
                let node = self.add_scalar(source, AccountedSemanticNode::Bytes(&value), resources)?;
                self.attach(source, node, resources)
            }
            _ if self.kind.is_jqft()
                && source.bytes().get(self.cursor) == Some(&b'b')
                && source.bytes().get(self.cursor + 1) == Some(&b'6')
                && source.bytes().get(self.cursor + 2) == Some(&b'4')
                && source.bytes().get(self.cursor + 3) == Some(&b'"') =>
            {
                self.cursor += 3;
                let value = self.scan_bytes_literal(source, false)?;
                let node = self.add_scalar(source, AccountedSemanticNode::Bytes(&value), resources)?;
                self.attach(source, node, resources)
            }
            _ if self.kind.is_jqft() => self.step_bare_word(source, resources),
            _ => self.step_json_word(source, resources),
        }
    }

    /// `@tag("name")` (jqft mode): accumulate the layer and keep parsing the payload. The tag text obeys `TagId`
    /// identity (exact, nonempty).
    fn step_tag(&mut self, source: ResolvedSource<'_>) -> Result<bool, CodecError> {
        const TAG: &[u8] = b"@tag";
        let bytes = source.bytes();
        if !bytes[self.cursor..].starts_with(TAG) {
            return Err(error::invalid(
                source,
                self.cursor,
                "invalid-tag",
                "expected @tag after @",
            ));
        }
        self.cursor += TAG.len();
        if bytes.get(self.cursor) != Some(&b'(') {
            return Err(error::invalid(
                source,
                self.cursor,
                "invalid-tag",
                "expected ( after @tag",
            ));
        }
        self.cursor += 1;
        if bytes.get(self.cursor) != Some(&b'"') {
            return Err(error::invalid(
                source,
                self.cursor,
                "invalid-tag",
                "the tag argument must be a quoted string",
            ));
        }
        self.scan_string(source)?;
        if bytes.get(self.cursor) != Some(&b')') {
            return Err(error::invalid(
                source,
                self.cursor,
                "invalid-tag",
                "expected ) after the tag argument",
            ));
        }
        self.cursor += 1;
        let cursor = self.cursor;
        let tag = self.with_scratch(|_, text| {
            TagId::try_new_unaccounted(text)
                .map_err(|_| error::invalid(source, cursor, "invalid-tag", "a tag must be one nonempty string"))
        })?;
        // A tag binds at the depth where it was READ: an inner tag must not escape into an outer batch (`@tag("a")
        // [@tag("b") 3, 2]` — "b" wraps the scalar, never the array). The wrap site below filters by this recorded
        // depth.
        self.pending_tags.push((self.frames.len(), tag));
        Ok(true)
    }

    fn open_array(&mut self, resources: &mut ResourceContext<'_>) -> Result<NodeId, CodecError> {
        self.cursor += 1;
        let prefix = self.schema_prefix;
        let semantic = AccountedSemanticNode::Array {
            item_role: provider::role_for(prefix, "array"),
        };
        let builder = self.builder()?;
        let node = builder
            .add_node(provider::kind_for(prefix, &semantic), semantic, None, resources)
            .map_err(map_data)?;
        Ok(node)
    }

    fn open_object(&mut self, resources: &mut ResourceContext<'_>) -> Result<NodeId, CodecError> {
        self.cursor += 1;
        let prefix = self.schema_prefix;
        let semantic = AccountedSemanticNode::Object {
            member_role: provider::role_for(prefix, "object"),
        };
        let builder = self.builder()?;
        let node = builder
            .add_node(provider::kind_for(prefix, &semantic), semantic, None, resources)
            .map_err(map_data)?;
        Ok(node)
    }

    fn add_scalar(
        &mut self,
        source: ResolvedSource<'_>,
        semantic: AccountedSemanticNode<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<NodeId, CodecError> {
        let _ = source;
        let prefix = self.schema_prefix;
        let builder = self.builder()?;
        let node = builder
            .add_node(provider::kind_for(prefix, &semantic), semantic, None, resources)
            .map_err(map_data)?;
        Ok(node)
    }

    /// Attaches a completed value node: wraps accumulated tag layers (innermost first), then publishes it to the
    /// current frame or the root.
    #[allow(
        clippy::too_many_lines,
        reason = "one attachment dispatch: tag wrapping, the parent routing, and the comment flush sit together"
    )]
    fn attach(
        &mut self,
        _source: ResolvedSource<'_>,
        mut node: NodeId,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        // Tag layers: the FIRST tag read is the OUTERMOST. The builder's tag-layer law: a kindless node carrying a
        // non-core intrinsic tag owns exactly one keyless payload occurrence. The tags wrap only the value that
        // completes at the depth where they were read — a container's elements complete deeper and pass through
        // unwrapped (the container close is what returns to the tag depth).
        let depth = self.frames.len();
        let wrap_tags = self.pending_tags.iter().any(|(d, _)| *d == depth);
        let tags: Vec<TagId> = if wrap_tags {
            // The builder's law: the FIRST tag read is the OUTERMOST layer, so the tags at this depth wrap in REVERSE
            // read order — the wrapping loop below iterates `tags` backwards, which makes the first-read tag the last
            // (outermost) layer. The collected order here is READ order; the loop owns the reversal.
            let mut taken = Vec::new();
            let mut kept = Vec::new();
            for (d, tag) in core::mem::take(&mut self.pending_tags) {
                if d == depth {
                    taken.push(tag);
                } else {
                    kept.push((d, tag));
                }
            }
            self.pending_tags = kept;
            taken
        } else {
            Vec::new()
        };
        if !tags.is_empty() {
            let prefix = self.schema_prefix;
            let builder = self.builder()?;
            for tag in tags.into_iter().rev() {
                let layer = builder
                    .add_node(
                        provider::kind_for(prefix, &AccountedSemanticNode::Unrepresentable),
                        AccountedSemanticNode::Unrepresentable,
                        Some(AccountedIntrinsicTag::Tagged(tag.as_str())),
                        resources,
                    )
                    .map_err(map_data)?;
                builder
                    .add_occurrence(
                        LocalOwnerRef::Node(layer),
                        provider::role_for(prefix, "tag-payload"),
                        None,
                        node,
                        resources,
                    )
                    .map_err(map_data)?;
                node = layer;
            }
        }
        // Extract the parent's attachment facts before borrowing the builder. Attach facts before recording the node as
        // the inline-comment owner.
        let parent = match self.frames.last_mut() {
            None => Parent::Root,
            Some(Frame::Array { owner, has_items }) => {
                *has_items = true;
                Parent::Array(*owner)
            }
            Some(Frame::Object { owner, pending_key }) => Parent::Object(*owner, pending_key.take()),
            Some(Frame::Markup { owner, .. }) => Parent::Markup(*owner),
        };
        let prefix = self.schema_prefix;
        let builder = self.builder()?;
        match parent {
            Parent::Root => {
                self.root = Some(node);
                self.step = Step::Trailing;
            }
            Parent::Array(owner) => {
                builder
                    .add_occurrence(
                        LocalOwnerRef::Node(owner),
                        provider::role_for(prefix, "array"),
                        None,
                        node,
                        resources,
                    )
                    .map_err(map_data)?;
                self.step = Step::ArrayCommaOrEnd;
            }
            Parent::Object(owner, key) => {
                let key = key.ok_or_else(error::data_contract)?;
                builder
                    .add_occurrence(
                        LocalOwnerRef::Node(owner),
                        provider::role_for(prefix, "object"),
                        Some(AccountedOccurrenceKey::StoredText(key)),
                        node,
                        resources,
                    )
                    .map_err(map_data)?;
                self.step = Step::ObjectCommaOrEnd;
            }
            Parent::Markup(owner) => {
                builder
                    .add_occurrence(
                        LocalOwnerRef::Node(owner),
                        provider::role_for(prefix, "markup"),
                        None,
                        node,
                        resources,
                    )
                    .map_err(map_data)?;
                self.step = Step::MarkupBody;
            }
        }
        // §3.15: the pending own-line comments read at or above this node's parent depth become this node's LEADING
        // comments (a comment inside a container or markup body attaches to the node that encloses it, never to a
        // sibling completed deeper — its recorded depth is the frame depth at read time). The node is now the
        // inline-comment owner.
        let attach_depth = self.frames.len();
        if !self.pending_leading_comments.is_empty() {
            let mut index = 0;
            while index < self.pending_leading_comments.len() {
                if self.pending_leading_comments[index].depth < attach_depth {
                    index += 1;
                    continue;
                }
                let comment = self.pending_leading_comments.remove(index);
                self.comment_events.push((node, CommentRole::Leading, comment.text));
            }
        }
        self.last_node = Some(node);
        Ok(true)
    }

    fn step_array_value_or_end(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        self.skip_trivia(source)?;
        match source.bytes().get(self.cursor).copied() {
            Some(b']') => self.close_array(source, resources),
            Some(_) => {
                self.step = Step::Value;
                Ok(true)
            }
            None => Err(error::invalid(
                source,
                self.cursor,
                "unterminated-array",
                "unterminated array",
            )),
        }
    }

    fn step_array_comma_or_end(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        self.skip_trivia(source)?;
        match source.bytes().get(self.cursor).copied() {
            Some(b',') => {
                // A leading comma is rejected at `ArrayValueOrEnd`; this arm only fires after an item.
                self.cursor += 1;
                self.step = Step::Value;
                Ok(true)
            }
            Some(b']') => self.close_array(source, resources),
            None => Err(error::invalid(
                source,
                self.cursor,
                "unterminated-array",
                "unterminated array",
            )),
            _ => Err(error::invalid(
                source,
                self.cursor,
                "invalid-array",
                "expected , or ] in array",
            )),
        }
    }

    fn close_array(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        self.cursor += 1;
        let Some(Frame::Array { owner, .. }) = self.frames.pop() else {
            return Err(data_contract());
        };
        self.attach(source, owner, resources)
    }

    fn close_object(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        self.cursor += 1;
        let Some(Frame::Object { owner, .. }) = self.frames.pop() else {
            return Err(data_contract());
        };
        self.attach(source, owner, resources)
    }

    // ----------------------------------------------------------------------- Markup nodes (the angle form `<name
    // &attr="v" children…>`). The node is an ARRAY of its ordered children (the array model); the name/attrs/content
    // facts and the per-attribute facts attach at the close. The membrane: children are nodes and strings ONLY; a
    // container, a bare scalar, or a tag inside markup is refused (§3.6).
    // -----------------------------------------------------------------------

    fn step_markup_name(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        self.skip_trivia(source)?;
        let bytes = source.bytes();
        let first = *bytes
            .get(self.cursor)
            .ok_or_else(|| error::invalid(source, self.cursor, "unterminated-markup", "unterminated markup node"))?;
        if !(first.is_ascii_alphabetic() || first == b'_') {
            return Err(error::invalid(
                source,
                self.cursor,
                "invalid-markup",
                "a markup node needs a name",
            ));
        }
        let start = self.cursor;
        self.cursor += 1;
        while let Some(byte) = bytes.get(self.cursor).copied() {
            if byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' || byte == b':' {
                self.cursor += 1;
            } else {
                break;
            }
        }
        let name = core::str::from_utf8(&bytes[start..self.cursor])
            .map_err(|_| error::invalid(source, start, "invalid-markup", "invalid UTF-8 in a markup node name"))?;
        if name.contains(':') {
            // Namespaced names are reserved for the XML vertical.
            return Err(error::invalid(
                source,
                start,
                "reserved-name",
                "namespaced markup names are reserved for the XML vertical",
            ));
        }
        let prefix = self.schema_prefix;
        let semantic = AccountedSemanticNode::Array {
            item_role: provider::role_for(prefix, "markup"),
        };
        let owner = self
            .builder()?
            .add_node(provider::kind_for(prefix, &semantic), semantic, None, resources)
            .map_err(map_data)?;
        self.frames.push(Frame::Markup {
            owner,
            name: String::from(name),
            attributes: Vec::new(),
            content: String::new(),
        });
        self.step = Step::MarkupBody;
        Ok(true)
    }

    fn step_markup_body(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        self.skip_trivia(source)?;
        let bytes = source.bytes();
        match bytes.get(self.cursor).copied() {
            Some(b'>') => self.close_markup(source, resources),
            Some(b'<') => {
                self.cursor += 1;
                self.step = Step::MarkupName;
                Ok(true)
            }
            Some(b'"') => {
                self.scan_string(source)?;
                let node = self.with_scratch(|this, value| {
                    if let Some(Frame::Markup { content, .. }) = this.frames.last_mut() {
                        content.push_str(value);
                    }
                    this.add_scalar(source, AccountedSemanticNode::String(value), resources)
                })?;
                let prefix = self.schema_prefix;
                let owner = match self.frames.last_mut() {
                    Some(Frame::Markup { owner, .. }) => *owner,
                    _ => return Err(data_contract()),
                };
                self.builder()?
                    .add_occurrence(
                        LocalOwnerRef::Node(owner),
                        provider::role_for(prefix, "markup"),
                        None,
                        node,
                        resources,
                    )
                    .map_err(map_data)?;
                // A string child is a completed node: it owns same-line inline comments on its own line. Pending
                // own-line comments are NOT consumed here — inside a markup body they attach to the markup NODE at its
                // close (a comment between `<p` and its content documents the element, not its first text run), which
                // is the position the canonical form can spell.
                self.last_node = Some(node);
                self.step = Step::MarkupBody;
                Ok(true)
            }
            Some(b'&') => {
                self.cursor += 1;
                self.step = Step::MarkupAttrName;
                Ok(true)
            }
            Some(_) => Err(error::unsupported(
                source,
                self.cursor,
                self.cursor.saturating_add(1),
                "markup-membrane",
                "markup children are nodes and strings only (the membrane): a container or \
                 bare scalar cannot be represented in a markup target",
            )),
            None => Err(error::invalid(
                source,
                self.cursor,
                "unterminated-markup",
                "unterminated markup node",
            )),
        }
    }

    fn step_markup_attr_name(&mut self, source: ResolvedSource<'_>) -> Result<bool, CodecError> {
        let bytes = source.bytes();
        let name = match bytes.get(self.cursor).copied() {
            Some(b'[') => {
                // The quoted bracket form `&["aria-label"]="v"` mirrors the accessor's bracket form.
                self.cursor += 1;
                if bytes.get(self.cursor) != Some(&b'"') {
                    return Err(error::invalid(
                        source,
                        self.cursor,
                        "invalid-attribute",
                        "expected \" in the quoted attribute name",
                    ));
                }
                self.scan_string(source)?;
                let name = self.with_scratch(|_, name| Ok(String::from(name)))?;
                if bytes.get(self.cursor) != Some(&b']') {
                    return Err(error::invalid(
                        source,
                        self.cursor,
                        "invalid-attribute",
                        "expected ] after the quoted attribute name",
                    ));
                }
                self.cursor += 1;
                name
            }
            Some(first) if first.is_ascii_alphabetic() || first == b'_' => {
                let start = self.cursor;
                self.cursor += 1;
                while let Some(byte) = bytes.get(self.cursor).copied() {
                    if byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' {
                        self.cursor += 1;
                    } else {
                        break;
                    }
                }
                core::str::from_utf8(&bytes[start..self.cursor])
                    .map_err(|_| {
                        error::invalid(source, start, "invalid-attribute", "invalid UTF-8 in an attribute name")
                    })?
                    .to_owned()
            }
            _ => {
                return Err(error::invalid(
                    source,
                    self.cursor,
                    "invalid-attribute",
                    "expected an attribute name after &",
                ));
            }
        };
        self.skip_trivia(source)?;
        if bytes.get(self.cursor) != Some(&b'=') {
            return Err(error::invalid(
                source,
                self.cursor,
                "invalid-attribute",
                "expected = after the attribute name",
            ));
        }
        self.cursor += 1;
        self.markup_pending_attr = Some(name);
        self.step = Step::MarkupAttrValue;
        Ok(true)
    }

    fn step_markup_attr_value(&mut self, source: ResolvedSource<'_>) -> Result<bool, CodecError> {
        self.skip_trivia(source)?;
        if source.bytes().get(self.cursor) != Some(&b'"') {
            return Err(error::invalid(
                source,
                self.cursor,
                "invalid-attribute",
                "attribute values are strings (the v1 membrane: a typed attribute is \
                 reserved pending the XML vertical)",
            ));
        }
        self.scan_string(source)?;
        let name = self.markup_pending_attr.take().ok_or_else(data_contract)?;
        self.with_scratch(|this, value| {
            let Some(Frame::Markup { attributes, .. }) = this.frames.last_mut() else {
                return Err(data_contract());
            };
            attributes.push((name, String::from(value)));
            Ok(())
        })?;
        self.step = Step::MarkupBody;
        Ok(true)
    }

    /// Closes a markup node: attaches the name/attrs/content and per-attribute facts, appends the content to a parent
    /// markup's content, and attaches the node to its parent (or the root).
    fn close_markup(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        self.cursor += 1;
        let Some(Frame::Markup {
            owner,
            name,
            attributes,
            content,
        }) = self.frames.pop()
        else {
            return Err(data_contract());
        };
        let attach_facts = self.coverage.attached_facts();
        let builder = self.builder()?;
        if attach_facts {
            // `.@name`
            builder
                .add_fact(
                    LocalOwnerRef::Node(owner),
                    provider::JQFT_NAME_FACT,
                    provider::JQFT_NAME_FACT,
                    1,
                    &FactPayload::Text(name.clone()),
                    resources,
                )
                .map_err(map_data)?;
            // `.@attrs` — the COMPLETE recovered attribute map.
            let attrs_payload = FactPayload::Map(
                attributes
                    .iter()
                    .map(|(key, value)| (key.clone(), FactPayload::Text(value.clone())))
                    .collect(),
            );
            builder
                .add_fact(
                    LocalOwnerRef::Node(owner),
                    provider::JQFT_ATTRS_FACT,
                    provider::JQFT_ATTRS_FACT,
                    1,
                    &attrs_payload,
                    resources,
                )
                .map_err(map_data)?;
            // One fact PER ATTRIBUTE, so `.&name` serves each expanded-name attribute (the engine's `.&` selector matches
            // role `attribute` + the attribute name as the fact kind).
            for (key, value) in &attributes {
                builder
                    .add_fact(
                        LocalOwnerRef::Node(owner),
                        provider::ATTRIBUTE_FACT,
                        key,
                        1,
                        &FactPayload::Text(value.clone()),
                        resources,
                    )
                    .map_err(map_data)?;
            }
            // `.@content` — concatenated descendant character data in document order (no separators).
            builder
                .add_fact(
                    LocalOwnerRef::Node(owner),
                    provider::JQFT_CONTENT_FACT,
                    provider::JQFT_CONTENT_FACT,
                    1,
                    &FactPayload::Text(content.clone()),
                    resources,
                )
                .map_err(map_data)?;
        }
        // A parent markup node appends this child's content to its own (the `.@content` law: concatenated descendant
        // character data).
        let child_content = content;
        if let Some(Frame::Markup { content, .. }) = self.frames.last_mut() {
            content.push_str(&child_content);
        }
        self.attach(source, owner, resources)
    }

    /// Consolidates the §3.15 comment events per node: the role-keyed map `{leading, inline, trailing, inner,
    /// detached}` — each role a list of `{text, style}` objects; absent roles read `[]` — is attached as
    /// [`provider::JQFT_COMMENT_MAP_FACT`] (the parse output and authority, which the canonical encoder reads), and the
    /// FLAT siblings are projected from it in the same function so they cannot disagree:
    /// [`provider::JQFT_COMMENT_FACT`] carries the leading list (plain text lines, so `.@comment` agrees with the other
    /// codecs), [`provider::JQFT_COMMENT_INLINE_FACT`] the inline list, and [`provider::JQFT_COMMENT_FOOT_FACT`] an
    /// empty list (the `#` grammar has no foot position).
    #[allow(
        clippy::too_many_lines,
        reason = "one consolidation keeps the map build and the three flat projections in one \
                  function so they cannot disagree)"
    )]
    fn attach_comment_facts(&mut self, resources: &mut ResourceContext<'_>) -> Result<(), CodecError> {
        let events = core::mem::take(&mut self.comment_events);
        if events.is_empty() || !self.coverage.attached_facts() {
            return Ok(());
        }
        let mut by_node: Vec<(NodeId, Vec<(CommentRole, String)>)> = Vec::new();
        for (node, role, text) in events {
            match by_node.iter_mut().find(|(owner, _)| *owner == node) {
                Some((_, entries)) => entries.push((role, text)),
                None => by_node.push((node, alloc::vec![(role, text)])),
            }
        }
        let builder = self.builder()?;
        for (node, entries) in by_node {
            let mut roles: [(Vec<(String, FactPayload)>, &str); 5] = [
                (Vec::new(), "leading"),
                (Vec::new(), "inline"),
                (Vec::new(), "trailing"),
                (Vec::new(), "inner"),
                (Vec::new(), "detached"),
            ];
            for (role, text) in entries {
                let slot = match role {
                    CommentRole::Leading => 0,
                    CommentRole::Inline => 1,
                    CommentRole::Detached => 4,
                };
                roles[slot].0.push(("text".to_owned(), FactPayload::Text(text)));
            }
            // The canonical entry shape is `{text, style}`; the `#` line comment's style is `line` (§3.15).
            let map_payload = FactPayload::Map(
                roles
                    .iter()
                    .map(|(entries, key)| {
                        let list = entries
                            .iter()
                            .map(|(field, value)| {
                                let entry = alloc::vec![
                                    (field.clone(), value.clone()),
                                    ("style".to_owned(), FactPayload::Text("line".to_owned()))
                                ];
                                FactPayload::Map(entry)
                            })
                            .collect();
                        ((*key).to_owned(), FactPayload::List(list))
                    })
                    .collect(),
            );
            builder
                .add_fact(
                    LocalOwnerRef::Node(node),
                    provider::JQFT_COMMENT_MAP_FACT,
                    provider::JQFT_COMMENT_MAP_FACT,
                    1,
                    &map_payload,
                    resources,
                )
                .map_err(map_data)?;
            // The flat siblings: plain text lines projected from the map's role lists (the `{text, style}` shape stays
            // on the map).
            let flat = |slot: usize| {
                let texts = roles[slot]
                    .0
                    .iter()
                    .filter_map(|(field, value)| match (field.as_str(), value) {
                        ("text", FactPayload::Text(text)) => Some(FactPayload::Text(text.clone())),
                        _ => None,
                    })
                    .collect::<alloc::vec::Vec<_>>();
                FactPayload::List(texts)
            };
            if !roles[0].0.is_empty() {
                builder
                    .add_fact(
                        LocalOwnerRef::Node(node),
                        provider::JQFT_COMMENT_FACT,
                        provider::JQFT_COMMENT_FACT,
                        1,
                        &flat(0),
                        resources,
                    )
                    .map_err(map_data)?;
            }
            if !roles[1].0.is_empty() {
                builder
                    .add_fact(
                        LocalOwnerRef::Node(node),
                        provider::JQFT_COMMENT_INLINE_FACT,
                        provider::JQFT_COMMENT_INLINE_FACT,
                        1,
                        &flat(1),
                        resources,
                    )
                    .map_err(map_data)?;
            }
            // The foot sibling is attached EMPTY on every comment-bearing node: the `#` grammar has no foot position,
            // so the fact exists only to complete the surface and answers `[]`.
            builder
                .add_fact(
                    LocalOwnerRef::Node(node),
                    provider::JQFT_COMMENT_FOOT_FACT,
                    provider::JQFT_COMMENT_FOOT_FACT,
                    1,
                    &FactPayload::List(alloc::vec![]),
                    resources,
                )
                .map_err(map_data)?;
        }
        Ok(())
    }

    fn step_object_key_or_end(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        self.skip_trivia(source)?;
        match source.bytes().get(self.cursor).copied() {
            Some(b'}') => self.close_object(source, resources),
            Some(b'(') => {
                self.cursor += 1;
                self.step = Step::ObjectBracketKey;
                Ok(true)
            }
            Some(b'"') => {
                self.scan_string(source)?;
                self.with_scratch(|this, key| this.store_pending_key(source, key, resources))?;
                self.step = Step::ObjectColon;
                Ok(true)
            }
            Some(first) if self.kind.is_jqft() && (first.is_ascii_alphabetic() || first == b'_') => {
                // A bare identifier key: `[A-Za-z_][A-Za-z0-9_-]*`.
                let start = self.cursor;
                self.cursor += 1;
                let bytes = source.bytes();
                while let Some(byte) = bytes.get(self.cursor).copied() {
                    if byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' {
                        self.cursor += 1;
                    } else {
                        break;
                    }
                }
                let key = core::str::from_utf8(&bytes[start..self.cursor])
                    .map_err(|_| error::invalid(source, start, "invalid-key", "invalid UTF-8 in bare key"))?;
                self.store_pending_key(source, key, resources)?;
                self.step = Step::ObjectColon;
                Ok(true)
            }
            _ => Err(error::invalid(
                source,
                self.cursor,
                "invalid-key",
                "expected an object key or }",
            )),
        }
    }

    /// The bracket-key form `{(key): value}`: the key is a data value spelled in parentheses. v1 accepts a DIRECT CORE
    /// STRING key (quoted or a bare identifier) — the model's object projection is string-keyed (`ObjectView` resolves
    /// text keys only), exactly the CBOR codec's narrowing, which refuses a non-projectable map at decode with the same
    /// law. A non-string key is a clean typed refusal naming that law, never a silent coercion.
    fn step_object_bracket_key(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        self.skip_trivia(source)?;
        let bytes = source.bytes();
        let key: Option<String> = match bytes.get(self.cursor).copied() {
            Some(b'"') => {
                self.scan_string(source)?;
                Some(self.with_scratch(|_, key| Ok(String::from(key)))?)
            }
            Some(first) if first.is_ascii_alphabetic() || first == b'_' => {
                let start = self.cursor;
                self.cursor += 1;
                while let Some(byte) = bytes.get(self.cursor).copied() {
                    if byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' {
                        self.cursor += 1;
                    } else {
                        break;
                    }
                }
                let key = core::str::from_utf8(&bytes[start..self.cursor])
                    .map_err(|_| error::invalid(source, start, "invalid-key", "invalid UTF-8 in bare bracket key"))?
                    .to_owned();
                Some(key)
            }
            _ => None,
        };
        self.skip_trivia(source)?;
        if bytes.get(self.cursor) != Some(&b')') {
            return Err(error::invalid(
                source,
                self.cursor,
                "invalid-key",
                "expected ) after the bracket key",
            ));
        }
        self.cursor += 1;
        let Some(key) = key else {
            // A non-string key is a non-projectable map member under the string-keyed object projection (the CBOR v1
            // narrowing's exact law). The grammar is READY — the full data-value key grammar is the recorded deferral
            // that lands with the projection upgrade.
            return Err(error::unsupported(
                source,
                self.cursor.saturating_sub(1),
                self.cursor,
                "non-string-key",
                "a non-string map key is not projectable: the document model's object \
                 projection resolves text keys only (the CBOR v1 narrowing); the bracket \
                 form's full data-value key grammar lands with that projection",
            ));
        };
        self.store_pending_key(source, &key, resources)?;
        self.step = Step::ObjectColon;
        Ok(true)
    }

    fn store_pending_key(
        &mut self,
        source: ResolvedSource<'_>,
        key: &str,
        resources: &ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        let _ = source;
        let stored = self.builder()?.store_text(key, resources).map_err(map_data)?;
        if let Some(Frame::Object { pending_key, .. }) = self.frames.last_mut() {
            *pending_key = Some(stored);
        }
        Ok(())
    }

    fn step_object_colon(&mut self, source: ResolvedSource<'_>) -> Result<bool, CodecError> {
        self.skip_trivia(source)?;
        if source.bytes().get(self.cursor) != Some(&b':') {
            return Err(error::invalid(
                source,
                self.cursor,
                "invalid-object",
                "expected : after the object key",
            ));
        }
        self.cursor += 1;
        self.step = Step::Value;
        Ok(true)
    }

    fn step_object_comma_or_end(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        self.skip_trivia(source)?;
        match source.bytes().get(self.cursor).copied() {
            Some(b',') => {
                self.cursor += 1;
                // A trailing comma's close is a jqft feature: strict JSON (jqfjson) rejects `{a: 1,}`.
                self.skip_trivia(source)?;
                if !self.kind.is_jqft() && source.bytes().get(self.cursor) == Some(&b'}') {
                    return Err(error::invalid(
                        source,
                        self.cursor,
                        "trailing-comma",
                        "a trailing comma is not permitted",
                    ));
                }
                self.step = Step::ObjectKeyOrEnd;
                Ok(true)
            }
            Some(b'}') => self.close_object(source, resources),
            None => Err(error::invalid(
                source,
                self.cursor,
                "unterminated-object",
                "unterminated object",
            )),
            _ => Err(error::invalid(
                source,
                self.cursor,
                "invalid-object",
                "expected , or } in object",
            )),
        }
    }

    fn step_trailing(&mut self, source: ResolvedSource<'_>) -> Result<bool, CodecError> {
        self.skip_trivia(source)?;
        // §3.15: a document-trailer comment (an own-line comment with no following node) is `jqf.detached` on the ROOT.
        if !self.pending_leading_comments.is_empty() {
            if let Some(root) = self.root {
                for comment in core::mem::take(&mut self.pending_leading_comments) {
                    self.comment_events.push((root, CommentRole::Detached, comment.text));
                }
            } else {
                self.pending_leading_comments.clear();
            }
        }
        if self.kind.is_jqft() {
            if source.bytes().get(self.cursor) == Some(&b'-')
                && source.bytes().get(self.cursor + 1) == Some(&b'-')
                && source.bytes().get(self.cursor + 2) == Some(&b'-')
            {
                // Another document follows: consume the separator AND the trivia after it, so the adjacent-value reopen
                // lands on the next document's first significant byte.
                self.cursor += 3;
                self.skip_trivia(source)?;
            } else if !self.allow_adjacent_values && self.cursor < source.bytes().len() {
                return Err(error::invalid(
                    source,
                    self.cursor,
                    "trailing-content",
                    "trailing content after the value is rejected",
                ));
            }
        } else if !self.allow_adjacent_values && self.cursor < source.bytes().len() {
            // jqfjson is one document per source when read whole; under the ADJACENT-VALUE contract (§4) the sequence
            // drive owns the stream and reopens at the consumed offset, so trailing content after the first envelope is
            // the NEXT envelope, not an error — exactly how plain json treats an adjacent stream.
            return Err(error::invalid(
                source,
                self.cursor,
                "trailing-content",
                "jqfjson is one document per source; trailing content after the value is rejected",
            ));
        }
        self.document_end = self.cursor;
        self.phase = Phase::Finalize;
        Ok(true)
    }

    fn expected_value(&self, source: ResolvedSource<'_>) -> CodecError {
        if self.kind.is_jqft() {
            error::invalid(
                source,
                self.cursor,
                "expected-value",
                "expected a value after the %jqft 1 header",
            )
        } else {
            error::invalid(source, self.cursor, "expected-value", "expected a JSON value")
        }
    }

    /// Scans a quoted string (JSON escapes) and returns its decoded text.
    #[allow(
        clippy::too_many_lines,
        reason = "one escape state machine: the decode dispatch is a single lexer loop"
    )]
    fn scan_string(&mut self, source: ResolvedSource<'_>) -> Result<(), CodecError> {
        let bytes = source.bytes();
        debug_assert_eq!(bytes.get(self.cursor), Some(&b'"'));
        self.scratch.clear();
        let mut cursor = self.cursor + 1;
        loop {
            let Some(&byte) = bytes.get(cursor) else {
                return Err(error::invalid(
                    source,
                    self.cursor,
                    "unterminated-string",
                    "unterminated string",
                ));
            };
            match byte {
                b'"' => {
                    cursor += 1;
                    self.cursor = cursor;
                    core::str::from_utf8(&self.scratch).map_err(|_| {
                        error::invalid(source, self.cursor, "invalid-string", "string is not valid UTF-8")
                    })?;
                    return Ok(());
                }
                b'\\' => {
                    cursor += 1;
                    let Some(&escape) = bytes.get(cursor) else {
                        return Err(error::invalid(
                            source,
                            self.cursor,
                            "unterminated-string",
                            "unterminated string escape",
                        ));
                    };
                    match escape {
                        b'"' => self.scratch.push(b'"'),
                        b'\\' => self.scratch.push(b'\\'),
                        b'/' => self.scratch.push(b'/'),
                        b'b' => self.scratch.push(0x08),
                        b'f' => self.scratch.push(0x0c),
                        b'n' => self.scratch.push(b'\n'),
                        b'r' => self.scratch.push(b'\r'),
                        b't' => self.scratch.push(b'\t'),
                        b'u' => {
                            cursor += 1;
                            let first = Self::scan_hex4(source, cursor)?;
                            cursor += 4;
                            let scalar = if (0xD800..=0xDBFF).contains(&first) {
                                // A high surrogate must pair with a low one.
                                if bytes.get(cursor) != Some(&b'\\') || bytes.get(cursor + 1) != Some(&b'u') {
                                    return Err(error::invalid(
                                        source,
                                        cursor,
                                        "invalid-escape",
                                        "a lone high surrogate in a \\u escape",
                                    ));
                                }
                                cursor += 2;
                                let low = Self::scan_hex4(source, cursor)?;
                                cursor += 4;
                                if !(0xDC00..=0xDFFF).contains(&low) {
                                    return Err(error::invalid(
                                        source,
                                        cursor,
                                        "invalid-escape",
                                        "a high surrogate must pair with a low surrogate",
                                    ));
                                }
                                0x10000 + (u32::from(first - 0xD800) << 10) + u32::from(low - 0xDC00)
                            } else if (0xDC00..=0xDFFF).contains(&first) {
                                return Err(error::invalid(
                                    source,
                                    cursor,
                                    "invalid-escape",
                                    "a lone low surrogate in a \\u escape",
                                ));
                            } else {
                                u32::from(first)
                            };
                            let mut encoded = [0u8; 4];
                            let text = char::from_u32(scalar).ok_or_else(|| {
                                error::invalid(source, cursor, "invalid-escape", "invalid \\u escape")
                            })?;
                            self.scratch
                                .extend_from_slice(text.encode_utf8(&mut encoded).as_bytes());
                            // Hex digits already consumed; do not take the shared one-byte step.
                            continue;
                        }
                        _ => {
                            return Err(error::invalid(
                                source,
                                cursor,
                                "invalid-escape",
                                "invalid string escape",
                            ));
                        }
                    }
                    cursor += 1;
                }
                0x00..=0x1f => {
                    return Err(error::invalid(
                        source,
                        cursor,
                        "invalid-string",
                        "a control character must be escaped inside a string",
                    ));
                }
                0x7f => {
                    // DEL is content; the shared StringContent stop set names it so JSON's canonicality probe does not
                    // memchr every run.
                    self.scratch.push(0x7f);
                    cursor += 1;
                }
                _ => {
                    // JSON-shaped string content: `"`, `\`, and C0 controls end the run; every other byte — non-ASCII
                    // included — is copied verbatim, matching the byte-at-a-time loop this replaced exactly.
                    let run_end = cursor + Self::string_content_prefix_len(&bytes[cursor..]);
                    self.scratch.extend_from_slice(&bytes[cursor..run_end]);
                    cursor = run_end;
                }
            }
        }
    }

    fn scan_hex4(source: ResolvedSource<'_>, cursor: usize) -> Result<u16, CodecError> {
        let bytes = source
            .bytes()
            .get(cursor..cursor + 4)
            .ok_or_else(|| error::invalid(source, cursor, "invalid-escape", "a \\u escape needs four hex digits"))?;
        let mut value: u16 = 0;
        for &byte in bytes {
            let digit = match byte {
                b'0'..=b'9' => u16::from(byte - b'0'),
                b'a'..=b'f' => u16::from(byte - b'a' + 10),
                b'A'..=b'F' => u16::from(byte - b'A' + 10),
                _ => {
                    return Err(error::invalid(
                        source,
                        cursor,
                        "invalid-escape",
                        "a \\u escape needs hex digits",
                    ));
                }
            };
            value = value * 16 + digit;
        }
        Ok(value)
    }

    /// Scans the contents of a `0x"…"` (hex) or `b64"…"` (base64) bytes literal; the cursor is positioned at the
    /// opening quote.
    fn scan_bytes_literal(&mut self, source: ResolvedSource<'_>, is_hex: bool) -> Result<Vec<u8>, CodecError> {
        let bytes = source.bytes();
        let quote = self.cursor;
        if bytes.get(quote) != Some(&b'"') {
            return Err(error::invalid(
                source,
                quote,
                "invalid-bytes",
                "expected \" after the bytes prefix",
            ));
        }
        let mut end = quote + 1;
        while let Some(&byte) = bytes.get(end) {
            if byte == b'"' {
                break;
            }
            end += 1;
        }
        if bytes.get(end) != Some(&b'"') {
            return Err(error::invalid(
                source,
                quote,
                "unterminated-bytes",
                "unterminated bytes literal",
            ));
        }
        let content = &bytes[quote + 1..end];
        let decoded = if is_hex {
            if content.len() % 2 != 0 || content.iter().any(|b| !b.is_ascii_hexdigit()) {
                return Err(error::invalid(
                    source,
                    quote,
                    "invalid-bytes",
                    "a 0x bytes literal needs an even number of hex digits",
                ));
            }
            content
                .chunks_exact(2)
                .map(|pair| (hex_digit(pair[0]) << 4) | hex_digit(pair[1]))
                .collect()
        } else {
            decode_base64(content).ok_or_else(|| {
                error::invalid(source, quote, "invalid-bytes", "invalid base64 in a b64 bytes literal")
            })?
        };
        self.cursor = end + 1;
        Ok(decoded)
    }

    /// A bare word at a value position (jqft mode): literal, exact number, `f`-suffixed float, or temporal literal.
    fn step_bare_word(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        let bytes = source.bytes();
        let start = self.cursor;
        while let Some(&byte) = bytes.get(self.cursor) {
            if byte == b' '
                || byte == b'\t'
                || byte == b'\r'
                || byte == b'\n'
                || byte == b','
                || byte == b'}'
                || byte == b']'
                || byte == b'#'
            {
                break;
            }
            self.cursor += 1;
        }
        let word = &bytes[start..self.cursor];
        let text = core::str::from_utf8(word)
            .map_err(|_| error::invalid(source, start, "invalid-value", "invalid UTF-8 in a bare value token"))?;
        match text {
            "null" => {
                let node = self.add_scalar(source, AccountedSemanticNode::Null, resources)?;
                self.attach(source, node, resources)
            }
            "true" => {
                let node = self.add_scalar(source, AccountedSemanticNode::Bool(true), resources)?;
                self.attach(source, node, resources)
            }
            "false" => {
                let node = self.add_scalar(source, AccountedSemanticNode::Bool(false), resources)?;
                self.attach(source, node, resources)
            }
            "inf" | "+inf" => self.attach_float(source, f64::INFINITY, resources),
            "-inf" => self.attach_float(source, f64::NEG_INFINITY, resources),
            "nan" | "-nan" | "+nan" => self.attach_float(source, FIXED_NAN, resources),
            _ => {
                if let Some(rest) = text.strip_suffix('f') {
                    return self.attach_float_spelling(source, rest, resources);
                }
                if let Some(temporal) = try_temporal(text) {
                    let semantic = match &temporal {
                        Temporal::LocalDate(date) => AccountedSemanticNode::LocalDate(*date),
                        Temporal::LocalTime(time) => AccountedSemanticNode::LocalTime(time),
                        Temporal::LocalDateTime(datetime) => AccountedSemanticNode::LocalDateTime(datetime),
                        Temporal::OffsetDateTime(datetime) => AccountedSemanticNode::OffsetDateTime(datetime),
                    };
                    let node = self.add_scalar(source, semantic, resources)?;
                    return self.attach(source, node, resources);
                }
                let node = self.add_exact_number(source, text, resources)?;
                self.attach(source, node, resources)
            }
        }
    }

    /// A strict-JSON bare token (jqfjson mode): a literal or an exact number.
    fn step_json_word(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        let bytes = source.bytes();
        let start = self.cursor;
        while let Some(&byte) = bytes.get(self.cursor) {
            if byte == b' '
                || byte == b'\t'
                || byte == b'\r'
                || byte == b'\n'
                || byte == b','
                || byte == b'}'
                || byte == b']'
            {
                break;
            }
            self.cursor += 1;
        }
        let word = &bytes[start..self.cursor];
        let text = core::str::from_utf8(word)
            .map_err(|_| error::invalid(source, start, "invalid-value", "invalid UTF-8 in a JSON token"))?;
        match text {
            "null" => {
                let node = self.add_scalar(source, AccountedSemanticNode::Null, resources)?;
                self.attach(source, node, resources)
            }
            "true" => {
                let node = self.add_scalar(source, AccountedSemanticNode::Bool(true), resources)?;
                self.attach(source, node, resources)
            }
            "false" => {
                let node = self.add_scalar(source, AccountedSemanticNode::Bool(false), resources)?;
                self.attach(source, node, resources)
            }
            _ => {
                let node = self.add_exact_number(source, text, resources)?;
                self.attach(source, node, resources)
            }
        }
    }

    fn attach_float(
        &mut self,
        source: ResolvedSource<'_>,
        value: f64,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        let node = self.add_scalar(
            source,
            AccountedSemanticNode::Float(jqf_data::Float::new(value)),
            resources,
        )?;
        self.attach(source, node, resources)
    }

    fn attach_float_spelling(
        &mut self,
        source: ResolvedSource<'_>,
        spelling: &str,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        let value = parse_float_spelling(spelling)
            .ok_or_else(|| error::invalid(source, self.cursor, "invalid-float", "invalid binary64 float spelling"))?;
        self.attach_float(source, value, resources)
    }

    /// Adds an exact integer or decimal node from a bare spelling.
    fn add_exact_number(
        &mut self,
        source: ResolvedSource<'_>,
        text: &str,
        resources: &mut ResourceContext<'_>,
    ) -> Result<NodeId, CodecError> {
        let exact = parse_exact_number(text)
            .ok_or_else(|| error::invalid(source, self.cursor, "invalid-number", "invalid number literal"))?;
        let semantic = match &exact {
            ExactNumber::Integer(canonical) => AccountedSemanticNode::Integer(canonical.as_ref()),
            ExactNumber::Decimal { coefficient, scale } => AccountedSemanticNode::Decimal {
                coefficient,
                scale: *scale,
            },
        };
        self.add_scalar(source, semantic, resources)
    }

    fn with_scratch<R>(&mut self, f: impl FnOnce(&mut Self, &str) -> Result<R, CodecError>) -> Result<R, CodecError> {
        let scratch = core::mem::take(&mut self.scratch);
        let result = match core::str::from_utf8(&scratch) {
            Ok(text) => f(self, text),
            Err(_) => Err(error::data_contract()),
        };
        self.scratch = scratch;
        result
    }

    fn builder(&mut self) -> Result<&mut AccountedDocumentBuilder<'static>, CodecError> {
        self.builder.as_mut().ok_or_else(|| {
            CodecError::new(CodecFailureKind::InternalContractViolation {
                contract: "jqft builder present",
            })
        })
    }
}

/// The fixed positive quiet NaN bit pattern (the textual binary64 law, §3.14; YAML pins the same bits).
const FIXED_NAN: f64 = f64::from_bits(0x7ff8_0000_0000_0000);

/// Parses an `f`-suffixed binary64 spelling: underscores stripped, Rust's correctly-rounded decimal-to-double
/// conversion.
fn parse_float_spelling(spelling: &str) -> Option<f64> {
    let mut digits = String::with_capacity(spelling.len());
    for byte in spelling.bytes() {
        if byte != b'_' {
            digits.push(char::from(byte));
        }
    }
    if digits.is_empty() {
        return None;
    }
    digits.parse::<f64>().ok()
}

#[derive(Debug)]
enum ExactNumber<'a> {
    Integer(alloc::borrow::Cow<'a, str>),
    Decimal { coefficient: String, scale: i64 },
}

/// Parses an exact number spelling (both modes): the JSON number grammar. Underscores are not accepted — both jqft and
/// jqfjson share this parser.
fn parse_exact_number(text: &str) -> Option<ExactNumber<'_>> {
    let bytes = text.as_bytes();
    let mut index = 0;
    let sign = if bytes.first() == Some(&b'-') {
        index = 1;
        "-"
    } else {
        ""
    };
    if index >= bytes.len() {
        return None;
    }
    // Integer part: no leading zeros.
    let int_start = index;
    let mut int_digits = 0usize;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
        int_digits += 1;
    }
    if int_digits == 0 {
        return None;
    }
    if int_digits > 1 && bytes[int_start] == b'0' {
        return None; // leading zero
    }
    let has_fraction = index < bytes.len() && bytes[index] == b'.';
    let mut frac_digits = 0usize;
    if has_fraction {
        index += 1;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
            frac_digits += 1;
        }
        if frac_digits == 0 {
            return None; // trailing dot
        }
    }
    let mut exponent: i64 = 0;
    if index < bytes.len() && (bytes[index] == b'e' || bytes[index] == b'E') {
        index += 1;
        let exp_sign = if index < bytes.len() && (bytes[index] == b'+' || bytes[index] == b'-') {
            let sign = bytes[index];
            index += 1;
            if sign == b'-' { -1 } else { 1 }
        } else {
            1
        };
        let exp_start = index;
        let mut exp_digits = 0usize;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
            exp_digits += 1;
        }
        if exp_digits == 0 {
            return None;
        }
        let magnitude: i64 = text[exp_start..index].parse().ok()?;
        exponent = exp_sign * magnitude;
    }
    if index != bytes.len() {
        return None;
    }
    let int_text = &text[int_start..int_start + int_digits];
    if !has_fraction && exponent == 0 {
        let canonical = if sign.is_empty() {
            alloc::borrow::Cow::Borrowed(int_text)
        } else {
            alloc::borrow::Cow::Owned(format!("{sign}{int_text}"))
        };
        return Some(ExactNumber::Integer(canonical));
    }
    let frac_text = &text[int_start + int_digits + 1..int_start + int_digits + 1 + frac_digits];
    let mut coefficient = String::with_capacity(1 + int_digits + frac_digits);
    coefficient.push_str(sign);
    coefficient.push_str(int_text);
    coefficient.push_str(frac_text);
    let mut scale = i64::try_from(frac_digits).ok()?.checked_sub(exponent)?;
    // The dynamic semantic's canonical coefficient carries no trailing zeros (the value is preserved by compensating
    // the scale): `1250.00` is the canonical decimal coefficient `125` at scale `-1`.
    while coefficient.len() > 1 && coefficient.ends_with('0') {
        coefficient.pop();
        scale = scale.checked_sub(1)?;
    }
    // Leading zeros in a longer magnitude (`0.85` → `085`) and the empty / `-`-only leftover of an all-zero fraction
    // (`-0.0` → `-`) are the two spellings the builder's canonical coefficient law rejects. Strip the zeros down to a
    // single `0`; a zero magnitude lives at scale 0.
    let negative = coefficient.starts_with('-');
    let raw = coefficient.strip_prefix('-').unwrap_or(coefficient.as_str());
    let stripped = raw.trim_start_matches('0');
    let digits = if stripped.is_empty() { "0" } else { stripped };
    if digits == "0" {
        scale = 0;
    }
    if raw != digits {
        let mut normalized = String::with_capacity(usize::from(negative) + digits.len());
        if negative {
            normalized.push('-');
        }
        normalized.push_str(digits);
        coefficient = normalized;
    }
    Some(ExactNumber::Decimal { coefficient, scale })
}

/// A temporal literal (jqft mode), TOML-shaped.
pub(crate) enum Temporal {
    LocalDate(LocalDate),
    LocalTime(LocalTime),
    LocalDateTime(jqf_data::LocalDateTime),
    OffsetDateTime(jqf_data::OffsetDateTime),
}

/// Tries to parse a bare word as one of the four temporal literals.
pub(crate) fn try_temporal(text: &str) -> Option<Temporal> {
    let bytes = text.as_bytes();
    if bytes.len() >= 10 && bytes[4] == b'-' && bytes[7] == b'-' {
        // A date, possibly extended with a time.
        let year = digit4(&bytes[0..4])?;
        let month = digit2(&bytes[5..7])?;
        let day = digit2(&bytes[8..10])?;
        let date = LocalDate::new(year, month, day)?;
        if bytes.len() == 10 {
            return Some(Temporal::LocalDate(date));
        }
        if bytes[10] != b'T' && bytes[10] != b't' {
            return None;
        }
        let time_bytes = &bytes[11..];
        let time_len = time_text_len(time_bytes);
        let time = parse_local_time(&time_bytes[..time_len])?;
        let datetime = jqf_data::LocalDateTime { date, time };
        if time_len == time_bytes.len() {
            return Some(Temporal::LocalDateTime(datetime));
        }
        let offset = parse_utc_offset(&time_bytes[time_len..])?;
        return Some(Temporal::OffsetDateTime(jqf_data::OffsetDateTime {
            local: datetime,
            offset,
        }));
    }
    if bytes.len() >= 8 && bytes[2] == b':' && bytes[5] == b':' {
        let time = parse_local_time(bytes)?;
        return Some(Temporal::LocalTime(time));
    }
    None
}

fn time_text_len(text: &[u8]) -> usize {
    let mut len = 0;
    while len < text.len() {
        let byte = text[len];
        if byte == b'+' || byte == b'-' || byte == b'Z' || byte == b'z' {
            break;
        }
        len += 1;
    }
    len
}

fn parse_local_time(bytes: &[u8]) -> Option<LocalTime> {
    if bytes.len() < 8 || bytes[2] != b':' || bytes[5] != b':' {
        return None;
    }
    let hour = digit2(&bytes[0..2])?;
    let minute = digit2(&bytes[3..5])?;
    let second = digit2(&bytes[6..8])?;
    let fraction = if bytes.len() > 8 {
        if bytes[8] != b'.' {
            return None;
        }
        let digits = core::str::from_utf8(&bytes[9..]).ok()?;
        if !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        jqf_data::FractionalSecond::parse(digits).ok()?
    } else {
        jqf_data::FractionalSecond::parse("").ok()?
    };
    LocalTime::new(hour, minute, second, fraction)
}

fn parse_utc_offset(text: &[u8]) -> Option<jqf_data::UtcOffset> {
    if text == b"Z" || text == b"z" {
        return Some(jqf_data::UtcOffset::KnownSeconds(jqf_data::KnownUtcOffset::new(0)?));
    }
    if text.len() != 6 || (text[0] != b'+' && text[0] != b'-') || text[3] != b':' {
        return None;
    }
    let hours = u32::from(digit2(&text[1..3])?);
    let minutes = u32::from(digit2(&text[4..6])?);
    if hours > 23 || minutes > 59 {
        return None;
    }
    let seconds = i32::try_from(hours * 3600 + minutes * 60).ok()?;
    let seconds = if text[0] == b'-' { -seconds } else { seconds };
    Some(jqf_data::UtcOffset::KnownSeconds(jqf_data::KnownUtcOffset::new(
        seconds,
    )?))
}

fn digit4(bytes: &[u8]) -> Option<u16> {
    if bytes.len() != 4 || bytes.iter().any(|byte| !byte.is_ascii_digit()) {
        return None;
    }
    let mut value: u16 = 0;
    for &byte in bytes {
        value = value * 10 + u16::from(byte - b'0');
    }
    Some(value)
}

fn digit2(bytes: &[u8]) -> Option<u8> {
    if bytes.len() != 2 || bytes.iter().any(|byte| !byte.is_ascii_digit()) {
        return None;
    }
    Some((bytes[0] - b'0') * 10 + (bytes[1] - b'0'))
}

fn hex_digit(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        b'A'..=b'F' => byte - b'A' + 10,
        _ => 0,
    }
}

/// Decodes RFC 4648 standard base64 with `=` padding.
fn decode_base64(input: &[u8]) -> Option<Vec<u8>> {
    fn value(byte: u8) -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut buffer = [0u8; 4];
    let mut count = 0usize;
    let mut seen_pad = false;
    for &byte in input {
        if byte == b'=' {
            seen_pad = true;
            continue;
        }
        // RFC 4648 §3.3: no data may follow the first padding character.
        if seen_pad {
            return None;
        }
        buffer[count] = value(byte)?;
        count += 1;
        if count == 4 {
            out.push((buffer[0] << 2) | (buffer[1] >> 4));
            out.push((buffer[1] << 4) | (buffer[2] >> 2));
            out.push((buffer[2] << 6) | buffer[3]);
            count = 0;
        }
    }
    match count {
        0 => {}
        2 => {
            out.push((buffer[0] << 2) | (buffer[1] >> 4));
        }
        3 => {
            out.push((buffer[0] << 2) | (buffer[1] >> 4));
            out.push((buffer[1] << 4) | (buffer[2] >> 2));
        }
        _ => return None,
    }
    Some(out)
}

pub(crate) fn map_data(error: DataError) -> CodecError {
    // A builder can raise an UNREPRESENTABLE shape on the jqft graph; that arm is the codec's own, everything else is
    // the shared mapping.
    match error {
        DataError::UnrepresentableSemantic | DataError::CyclicSemanticGraph => {
            CodecError::new(CodecFailureKind::UnsupportedRepresentation)
        }
        other => jqf_codec_core::map_data(other, "jqft builder rejected document construction"),
    }
}

fn data_contract() -> CodecError {
    error::data_contract()
}

#[cfg(test)]
mod parse_exact_number_tests {
    use super::{ExactNumber, parse_exact_number};

    fn decimal(text: &str) -> (String, i64) {
        match parse_exact_number(text) {
            Some(ExactNumber::Decimal { coefficient, scale }) => (coefficient, scale),
            other => panic!("{text:?} should be a decimal, got {other:?}"),
        }
    }

    fn integer(text: &str) -> String {
        match parse_exact_number(text) {
            Some(ExactNumber::Integer(canonical)) => canonical.into_owned(),
            other => panic!("{text:?} should be an integer, got {other:?}"),
        }
    }

    #[test]
    fn a_zero_integer_part_strips_the_leading_zero() {
        // `0.85` assembled as `085` is the row the builder rejects; the leading-zero strip is the law, and `1.05`
        // (nonzero integer part) is the sibling that already worked.
        assert_eq!(decimal("0.85"), (String::from("85"), 2));
        assert_eq!(decimal("0.05"), (String::from("5"), 2));
        assert_eq!(decimal("0.1"), (String::from("1"), 1));
        assert_eq!(decimal("1.05"), (String::from("105"), 2));
    }

    #[test]
    fn a_signed_all_zero_fraction_normalizes_to_signed_zero() {
        // Trailing-zero strip of `-0.0` leaves `-`; that degenerate is rewritten to `-0` at scale 0 before the builder
        // sees it.
        assert_eq!(decimal("-0.0"), (String::from("-0"), 0));
        assert_eq!(decimal("0.0"), (String::from("0"), 0));
    }

    #[test]
    fn a_bare_integer_is_unchanged() {
        assert_eq!(integer("42"), "42");
        assert_eq!(integer("-7"), "-7");
        assert_eq!(integer("0"), "0");
    }
}
