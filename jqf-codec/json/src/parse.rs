//! Cooperative iterative RFC 8259 parser and document constructor.
//!
//! Token-batch hot path, string/number windows, and the frame stack that holds one [`jqf_resource::OwnedDepthGuard`]
//! per open container. JSONC/JSON5 grammar bits live on [`JsonGrammar`]. Scoped validation is [`crate::scoped`].

use alloc::string::String;
use alloc::vec::Vec;
use core::mem;

use jqf_codec_core::{
    AccessInput, AccessOutcome, AccessResult, AccessSession, CodecError, CodecFailureKind, CodecRunContext,
    DocumentProduct,
};
use jqf_data::{
    AccountedDocumentBuilder, AuthoritativeEmptyFamilies, BuilderCoverage, DataError, DiagnosticCoverage,
    DocumentCapabilityFamily, DocumentCapacity, DocumentFinalizationPoll, DocumentSchemaPrototype,
    DocumentSchemaRecipe, DocumentTextId, FactPayload, LazySpanMaterializer, LocalOwnerRef, NodeId,
    PreparedDocumentSchema, PreparedSemanticNode,
};
use jqf_resource::{ResourceContext, WorkAdmission};
use jqf_source::ResolvedSource;

use crate::error::diag;
use crate::error::{data_contract, invalid, unsupported_number};
use crate::lex::{
    BYTE_ORDER_MARK, CAPACITY_REPROJECT_MARGIN_DENOMINATOR, CAPACITY_REPROJECT_MIN_NODES,
    CAPACITY_REPROJECT_MIN_SOURCE, CAPACITY_REPROJECT_SAMPLE_DENOMINATOR, LENIENT_OVERFLOW_ZERO_SCALE,
    LENIENT_UNDERFLOW_SCALE, MAX_HEX_DIGITS, MIN_BYTES_PER_NODE, admit_bytes, advance_number, apply_unicode_scalar,
    byte_order_mark_truncated, consume_digit_run, decimal_source_is_canonical, hex, hex_to_decimal, is_value_boundary,
    key_fingerprint, map_data, nonfinite_literal, nonfinite_spelling_prefix_len, nonfinite_spelling_truncated,
    number_finish_plan, number_is_verbatim_source, number_lex_complete, number_render_plan,
    number_survives_owned_round_trip, plain_string_run_end, plain_string_run_end_short, push_char, push_chunk,
    reserve_estimate, reserve_hint, reserve_window, single_quoted_run_end, string_content_run_end, take_unicode_hex,
    verbatim_integer_end,
};
use crate::storage::{
    Builder, EscapeState, Finalizer, Frame, FrameState, JsonGrammar, NumberLex, NumberNormalizeState, NumberState,
    ObjectKey, ParseMode, ParsePhase, StringState, StringTarget,
};

/// Nested records are commonly shallow. Retain a modest parser workspace but release a one-off deeply nested record
/// before the next adjacent value.
pub(crate) const FRAME_REUSE_CAP: usize = 64;

/// Frame prune sentinel: everything below is kept — the default, and the only value any frame carries when no prune
/// map is armed.
pub(crate) const PRUNE_ALL: u32 = u32::MAX;
/// Frame prune sentinel: inside an omitted subtree — every commit is `None`, exactly like a value inside a deferred
/// subtree.
pub(crate) const PRUNE_OMIT: u32 = u32::MAX - 1;

/// The JSONC comment fact: one list-of-texts payload on the value node whose leading comments the decode attached, the
/// landed `<fmt>.comment@1` shape verbatim. The codec-agnostic matcher (`fact_role_serves` in the edit lane,
/// `jqf-sdk/src/drive/edit.rs`) serves `.@comment` with zero SDK/engine change.
pub(crate) const COMMENT_FACT: &str = "jsonc.comment@1";

/// The session-owned copy of a requirement's kept-subtree prune hint: the transported tree flattened into plain lookup
/// nodes, so the parse state owns it without borrowing the requirement.
struct PruneMap {
    nodes: Vec<PruneMapNode>,
}

struct PruneMapNode {
    all: bool,
    element: Option<u32>,
    /// Ascending by key bytes (the transport's push-order contract).
    keys: Vec<(alloc::boxed::Box<[u8]>, u32)>,
}

impl PruneMapNode {
    /// The named member's demand node: exact key hit, else the shared element node, else `None` (unobservable — the
    /// caller omits).
    ///
    /// WHY a copy and not a call into [`jqf_codec_core::PruneTreeNode::member`]: this state must own its hint without
    /// borrowing the requirement, and the shared type offers no way to build an owned node here — its fields are
    /// private and its only clone path (`try_clone_in`) needs a resource context that `enable_prune`'s arm-time
    /// signature does not carry. The codec-core module doc already names this arrangement — "the join happens AT
    /// lookup ... and again in every consumer-side copy's own table" — so this method IS that consumer-side join;
    /// keep it textually parallel with the shared one.
    fn member(&self, name: &[u8]) -> Option<u32> {
        self.keys
            .binary_search_by(|(key, _)| key.as_ref().cmp(name))
            .ok()
            .map(|position| self.keys[position].1)
            .or(self.element)
    }
}

impl PruneMap {
    /// Copies the transported tree; `None` when the hint keeps everything at the root (nothing to prune).
    ///
    /// The scan walks ids from 0 and stops at the FIRST gap, so contiguity is inferred, not proven. That is sound
    /// because the engine assigns prune-node ids densely from 0 and reserves the TOP of the u32 range for its sentinels
    /// (`PRUNE_ALL`, `PRUNE_OMIT`) — a non-contiguous or sentinel-colliding transport would silently truncate here.
    /// `PruneMap::from_transport` therefore trusts the engine's constructor, never arbitrary bytes.
    fn from_transport(tree: &jqf_codec_core::PruneTree) -> Option<Self> {
        if tree.root().is_all() {
            return None;
        }
        let mut nodes = Vec::new();
        for id in 0..=u32::MAX - 2 {
            let Some(node) = tree.node(id) else { break };
            nodes.push(PruneMapNode {
                all: node.is_all(),
                element: node.element(),
                keys: node
                    .members()
                    .map(|(name, child)| (alloc::boxed::Box::from(name.as_bytes()), child))
                    .collect(),
            });
        }
        Some(Self { nodes })
    }

    fn node(&self, id: u32) -> Option<&PruneMapNode> {
        self.nodes.get(id as usize)
    }
}

pub(crate) fn reset_reusable_frames<T>(frames: &mut Vec<T>) {
    frames.clear();
    if frames.capacity() > FRAME_REUSE_CAP {
        *frames = Vec::new();
    }
}

/// The largest coefficient this module's number normalizer builds on the stack instead of appending digit-run by
/// digit-run. Covers every realistic decoder-produced literal; a longer one falls back to the piecewise append.
const NUMBER_COEFF_STACK_CAP: usize = 48;

const ARRAY_ROLE: &str = "json.array.item@1";
const OBJECT_ROLE: &str = "json.object.member@1";
pub(crate) const JSON_NODE_KINDS: &[&str] = &[
    "json.null@1",
    "json.bool@1",
    "json.number@1",
    "json.string@1",
    "json.array@1",
    "json.object@1",
];
pub(crate) const JSON_OCCURRENCE_ROLES: &[&str] = &[ARRAY_ROLE, OBJECT_ROLE];

fn json_schema_recipe() -> Result<DocumentSchemaRecipe<'static>, DataError> {
    DocumentSchemaRecipe::try_new(
        "json",
        Some("rfc8259"),
        JSON_NODE_KINDS,
        JSON_OCCURRENCE_ROLES,
        &[],
        &[],
    )
}

pub(crate) fn try_schema_prototype(_resources: &ResourceContext<'_>) -> Result<DocumentSchemaPrototype, CodecError> {
    let recipe = json_schema_recipe().map_err(map_data)?;
    DocumentSchemaPrototype::try_new(&recipe).map_err(map_data)
}

/// One container subtree the frontier is deferring to a span.
///
/// `open` is the offset of the container's opening bracket; `nesting` counts how many opens are still to close before
/// the subtree is complete. The validating scan runs over the whole subtree exactly as it does eagerly — it is the
/// SAME loop, so error code, message and offset are identical by construction — only the node, occurrence, and text
/// commits are withheld.
struct DeferredContainer {
    open: usize,
    nesting: usize,
    container: jqf_data::ContainerSpanKind,
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "independent session facts that must survive poll boundaries; a bitflag would hide which fact each poll sets"
)]
pub(crate) struct JsonParseState {
    phase: ParsePhase,
    schema_prototype: Option<DocumentSchemaPrototype>,
    builder: Option<Builder>,
    schema: Option<PreparedDocumentSchema>,
    finalizer: Option<Finalizer>,
    frames: Vec<Frame>,
    /// Authoring scratch recycled across the records of one stream, beside the frame workspace and reused on the same
    /// terms: capacity only, bounded, and surviving `reset` because a record stream builds one document per record and
    /// would otherwise regrow the same few shapes per record. Opaque — the vectors inside belong to `jqf-data`.
    transients: jqf_data::DocumentTransients,
    root: Option<NodeId>,
    cursor: usize,
    diagnostics: DiagnosticCoverage,
    coverage: BuilderCoverage,
    product: Option<DocumentProduct<'static>>,
    binding_stage: Option<jqf_data::DocumentSourceBindingStage>,
    source_binding: Option<jqf_data::DocumentSourceBinding>,
    mode: ParseMode,
    /// Whether the decoded source is CANONICAL — its compact render is the source itself. Cleared permanently by the
    /// disqualifier hooks: any skipped whitespace, a source-start byte-order mark, an exponent number, a non-minimal
    /// string escape (`\u`, `\/`), a decimal that is not its own render, or a duplicate object key (the inline
    /// fingerprint probe's decline). One exception: the DEL-byte clear fires only when the probe itself is armed (see
    /// the string automaton's DEL arm) — sound because this flag's only reader is that probe's owner. Only consulted
    /// for the single-document shape; see `JsonParseState::new`.
    pub(crate) source_canonical: bool,
    /// Whether the per-key duplicate fingerprint probe may run.
    ///
    /// The probe's fingerprints exist solely to decide the canonical-identity echo's verdict (`source_canonical` at the
    /// roundtrip gate), and the echo lane (`execute_source_roundtrip`) is the only session that ever arms this flag.
    /// Every other decode pays nothing for a verdict it cannot consume — `false` by default, so the FNV fingerprint
    /// and its window scan are skipped on pretty-rendered documents and on record streams.
    canonicality_probe: bool,
    /// Frame depth at or beyond which a container subtree is DEFERRED to a span instead of being built (the lazy
    /// frontier).
    ///
    /// `None` is the eager default and the only shape the default route takes. `Some(depth)` is reachable only through
    /// the force switch, and only in a mode that retains source spans — an `OwnedRun` session recycles its buffer, so
    /// a span into it could never be published.
    lazy_frontier: Option<usize>,
    /// The container subtree currently being deferred, if any.
    deferred: Option<DeferredContainer>,
    /// Whether any zero-copy span was committed, and so whether the source segment the root value occupies has to be
    /// sealed before finalization.
    source_spans_committed: bool,
    /// Length of that segment, known once the root value is complete.
    sealed_len: Option<usize>,
    consumed_offset: Option<u64>,
    /// Whether the token just completed ran to the LAST byte the decoder was given without a delimiter closing it, so
    /// more input could extend it into a different token (`1234` into `1234567`, `inf` into `infinity`). The decoder
    /// cannot tell "the input ended" from "this window's bytes ran out"; it publishes the value it has and reports the
    /// ambiguity on [`jqf_codec_core::AccessReport::open_ended`], which the caller — who alone knows whether the
    /// source is still growing — resolves.
    open_ended: bool,
    capacity_reprojected: bool,
    /// Whether any consumer of this decode may read the out-of-band authored-span table — the requirement's
    /// `authored_spans` hint. TRUE by default, the safe direction: recording spans no consumer reads costs only the
    /// table, while dropping spans an edit lane needs floors the edit. A drive that proves no edit, splice, or lossless
    /// encode consumes the document clears it, and every `record_leaf_span`/`record_container_anchor` write is skipped.
    /// Requirement-scoped like `canonicality_probe`, so `reset` keeps it.
    authored_spans: bool,
    /// The armed kept-subtree prune hint: object members matching neither a named key nor the element node of the
    /// governing map node commit NOTHING — the deferred-subtree `None` path, selectively. `None` (the default)
    /// decodes everything.
    prune: Option<PruneMap>,
    /// The grammar this session accepts: strict RFC 8259 by default, with the leniency dial copied from the resource
    /// dial at provider construction; a JSONC session arms comments and/or trailing commas. A field, never a resource
    /// lookup: the hot path reads the grammar's three bits directly.
    pub(crate) grammar: JsonGrammar,
    /// The DYNAMIC-schema recipe (TOML/YAML's `try_new_prepared_with_coverage` builder shape) when this session must
    /// attach facts: a prototype-built builder carries a shared prepared schema that refuses `add_fact`
    /// (`require_dynamic_schema`). The JSONC provider sets this instead of a schema prototype; strict JSON keeps the
    /// prototype path and never attaches facts.
    dynamic_recipe: Option<DocumentSchemaRecipe<'static>>,
    /// The current comment run awaiting its owning node: the trivia scanner appends each skipped comment's text here,
    /// and the next completed VALUE node takes the run as its leading `jsonc.comment@1` fact. Only ever written when
    /// `grammar.comments` AND the coverage demands attached facts; a comment-free or comment-undemanded decode
    /// allocates nothing. A run left pending at finalize (a trailing comment with no following value) attaches to the
    /// document root, the TOML trailer law.
    pending_comments: Vec<alloc::string::String>,
    /// Bytes already granted toward the trivia run at the cursor, carried across cooperative entries. One admission
    /// covers at most 256 bytes (`MAX_LINEAR_ITEMS_PER_CREDIT`), so a comment longer than one grant accumulates
    /// admissions over the same offset inside `trivia_advance`; this field suspends that accumulation when the entry's
    /// credits run out mid-comment, so re-entry resumes instead of rescanning the same prefix forever. Zero unless a
    /// run is suspended mid-comment.
    trivia_window: usize,
    /// The fact role this session attaches comments under: `jsonc.comment@1` for the JSONC provider (the default) and
    /// `json5.comment@1` for the JSON5 provider — the codec-agnostic matcher (`fact_role_serves` in
    /// `jqf-sdk/src/drive/edit.rs`) strips the namespace, so `.@comment` is served with zero SDK change for either.
    comment_fact_role: &'static str,
    /// The source bytes this session is parsing were already proved valid UTF-8 (a scoped pre-scan, or a materializer
    /// handed a `&str`). The string walk then skips the block `utf8_first_invalid` / `from_utf8` revalidation. Bytes
    /// that were not proved keep the check.
    utf8_proved: bool,
    /// The reader bound when this session defers container spans. Strict JSON leaves this unset and binds
    /// [`crate::lazy::JSON_SPAN_MATERIALIZER`]; a commented dialect sets its own so a `//` inside a deferred span
    /// cannot take the STRICT materializer.
    span_materializer: Option<&'static dyn LazySpanMaterializer>,
}

impl JsonParseState {
    pub(crate) const fn new(diagnostics: DiagnosticCoverage, coverage: BuilderCoverage, mode: ParseMode) -> Self {
        Self {
            phase: ParsePhase::Value,
            schema_prototype: None,
            builder: None,
            schema: None,
            finalizer: None,
            frames: Vec::new(),
            transients: jqf_data::DocumentTransients::new(),
            root: None,
            cursor: 0,
            diagnostics,
            coverage,
            product: None,
            binding_stage: None,
            source_binding: None,
            mode,
            lazy_frontier: None,
            deferred: None,
            source_spans_committed: false,
            sealed_len: None,
            consumed_offset: None,
            open_ended: false,
            capacity_reprojected: false,
            authored_spans: true,
            prune: None,
            // The S4 canonicality flag: the document's source is assumed canonical and any disqualifier clears it
            // permanently. It is only ever consulted for the SINGLE-document shape (the canonical-identity echo
            // declines for a second adjacent text), so the flag's value at the first value's finalize is the whole
            // story.
            source_canonical: true,
            canonicality_probe: false,
            grammar: JsonGrammar::STRICT,
            pending_comments: Vec::new(),
            trivia_window: 0,
            dynamic_recipe: None,
            comment_fact_role: COMMENT_FACT,
            utf8_proved: false,
            span_materializer: None,
        }
    }

    /// Arms this session's grammar dial. The strict provider sets the leniency bit from the resource dial at
    /// construction; a JSONC provider arms comments and/or trailing commas per its dialect's `Options`: the grammar is
    /// fed from the request, never from a global flag.
    pub(crate) fn set_grammar(&mut self, grammar: JsonGrammar) {
        self.grammar = grammar;
    }

    /// Names the fact role this session's comment attachment uses: the JSONC default or the JSON5 provider's
    /// `json5.comment@1`.
    pub(crate) fn set_comment_fact_role(&mut self, role: &'static str) {
        self.comment_fact_role = role;
    }

    /// Names the deferred-span reader this session binds when the lazy frontier is armed. Commented dialects set this
    /// so a `//` inside a deferred container cannot take the STRICT materializer.
    pub(crate) fn set_span_materializer(&mut self, materializer: &'static dyn LazySpanMaterializer) {
        self.span_materializer = Some(materializer);
    }

    /// Marks the session's source as already-proved UTF-8 so the string materializer does not re-validate spans the
    /// scoped pre-scan (or a `&str` handoff) already accepted.
    pub(crate) fn set_utf8_proved(&mut self) {
        self.utf8_proved = true;
    }

    /// Switches this session to the dynamic-schema builder shape a fact-attaching codec needs: the recipe binds the
    /// same node kinds and occurrence roles as the strict prototype, plus the codec's fact roles, and the builder it
    /// produces is the `try_new_prepared_with_coverage` shape (no shared schema), so `add_fact` is legal.
    pub(crate) fn with_dynamic_recipe(&mut self, recipe: DocumentSchemaRecipe<'static>) {
        self.dynamic_recipe = Some(recipe);
    }

    /// The trivia prefix of `bytes`: whitespace, and — when the grammar arms comments — `//`/`/* */` comment runs,
    /// whose texts join the pending leading-comment run when the coverage demands attached facts. With comments off
    /// this is exactly `byte_scan::ws_prefix_len`: the strict-JSON call sites compile back to today's call. The bool is
    /// [`byte_scan::trivia_prefix_len`]'s ended-inside-a-comment flag.
    fn trivia_len(&mut self, bytes: &[u8]) -> Result<(usize, bool), CodecError> {
        let collect = self.grammar.comments && self.coverage.attached_facts();
        let mut comments = collect.then(Vec::new);
        let (len, open_comment) = crate::byte_scan::trivia_prefix_len(bytes, self.grammar, comments.as_mut())?;
        if let Some(comments) = comments {
            // Ordering dependency: every text appended below belongs to THIS window's scan alone. `trivia_advance`
            // captures `pending_comments.len()` immediately before calling this method and truncates back to that mark
            // when the run turns out to have been cut by the WINDOW's end rather than the source's, so a truncated
            // attempt leaves no partial comment behind. Texts appended anywhere else would sit below that mark's reach
            // and survive a discard that must remove them — append collected comment texts ONLY here.
            self.pending_comments
                .try_reserve(comments.len())
                .map_err(jqf_resource::ResourceError::from)?;
            self.pending_comments.extend(comments);
        }
        Ok((len, open_comment))
    }

    /// Advances one trivia run — whitespace plus, when the grammar arms them, comments and the JSON5 line separators
    /// — or classifies why it could not.
    ///
    /// # The zero-progress law
    ///
    /// [`Self::trivia_len`] can legitimately consume nothing: a `/` that does not open a comment (`/x` under jsonc) and
    /// a JSON5 lead byte that does not begin U+2028/U+2029/U+FEFF are CONTENT, and a comment marker or three-byte
    /// separator whose tail lies beyond the end of the source is an INCOMPLETE CANDIDATE. Neither may report progress
    /// — the step would re-enter at the same offset forever, because the meter's replenish REPLACES credits and so
    /// bounds nothing. Classification reads the FULL source, never just the granted slice, so a credit slice ending
    /// inside a `//` marker retries with a fresh admission (which grants at least the marker's two bytes) instead of
    /// failing a valid whole-document decode.
    ///
    /// # The grant-window law
    ///
    /// One admission covers at most 256 bytes (`MAX_LINEAR_ITEMS_PER_CREDIT`), so a comment longer than one admission
    /// must ACCUMULATE admissions over the same offset until its terminator lands inside the window — the meter
    /// grants `min(remaining, 256)` per call, never more. A run cut by the WINDOW end (not the source's) consumes
    /// NOTHING: its truncated text would publish as a short `.@comment` and the parse would resume mid-comment,
    /// rejecting valid JSONC. Each retry discards the attempt's fact texts and admits another grant; when an entry's
    /// credits run out mid-run, the accumulated size suspends in [`Self::trivia_window`] and re-entry resumes from it,
    /// so every entry strictly grows the window and any input terminates.
    fn trivia_advance(
        &mut self,
        source: ResolvedSource<'_>,
        at: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<TriviaAdvance, CodecError> {
        let bytes = source.bytes();
        let mut window = core::mem::take(&mut self.trivia_window);
        loop {
            let Some(granted) = admit_bytes(resources, bytes.len() - at)? else {
                // Mid-run suspension: keep the accumulated window for the next entry's resume.
                self.trivia_window = window;
                return Ok(TriviaAdvance::Yield);
            };
            window += granted;
            let limit = (at + window).min(bytes.len());
            let pending_base = self.pending_comments.len();
            let (consumed, open_comment) = self.trivia_len(&bytes[at..limit])?;
            if open_comment && limit < bytes.len() {
                // The comment run was cut by the WINDOW end, not the source's: consuming would publish a truncated
                // `.@comment` and resume MID-COMMENT. Discard this attempt's fact texts and admit more over the same
                // offset.
                self.pending_comments.truncate(pending_base);
                continue;
            }
            self.trivia_window = 0;
            if consumed > 0 {
                return Ok(TriviaAdvance::Skipped(consumed));
            }
            let retry =
                matches!(bytes[at], b'/' if self.grammar.comments) && matches!(bytes.get(at + 1), Some(b'/' | b'*'));
            if retry {
                continue;
            }
            match bytes[at] {
                b'/' if self.grammar.comments => match bytes.get(at + 1) {
                    // Marker split by the end of the SOURCE: an incomplete candidate, not garbage — hold semantics.
                    None => return Ok(TriviaAdvance::Cut),
                    Some(b'/' | b'*') => {}
                    Some(_) => return Ok(TriviaAdvance::Content),
                },
                b'\xe2' | b'\xef' if self.grammar.json5 => {
                    if bytes.len() - at < 3 {
                        return Ok(TriviaAdvance::Cut);
                    }
                    // All three bytes are present and did not match a JSON5 separator: content the dispatch below
                    // diagnoses.
                    return Ok(TriviaAdvance::Content);
                }
                // The entry predicates guarantee whitespace or a comment / JSON5 lead; anything else is a session
                // invariant violation.
                _ => return Err(data_contract()),
            }
        }
    }

    /// Arms or withdraws the per-key duplicate fingerprint probe (the duplicate-key disqualifier) from the
    /// requirement's hint. Only the canonical-identity echo lane sets it, and a RECYCLED session re-arms it from each
    /// new requirement — a stale `true` from a previous echo would keep probing for a caller that cannot consume the
    /// verdict; see the field doc.
    pub(crate) fn set_canonicality_probe(&mut self, on: bool) {
        self.canonicality_probe = on;
    }

    /// Arms or withdraws the authored-span demand for this session, from the requirement's hint. See the field doc for
    /// the TRUE-by-default law.
    pub(crate) fn set_authored_spans(&mut self, on: bool) {
        self.authored_spans = on;
    }

    /// Enables the container-span frontier for this session.
    ///
    /// `depth` is the frame depth at which deferral starts: `1` defers every container below the root, which is the
    /// shape S1 measures. Depth `0` is refused — a deferred ROOT would publish a document whose only node is a span,
    /// which no reader below can navigate.
    ///
    /// # The copy-mode refusal, and why the predicate is not the whole argument
    ///
    /// [`ParseMode::retains_source_spans`] refuses [`ParseMode::OwnedRun`], which is the copy-mode constraint: a lazy
    /// span document may never be published out of a session whose buffer is recycled. That predicate is NECESSARY but
    /// it is not, on its own, sufficient — it says the mode may name its source, not that the source outlives the
    /// document. Sufficiency comes from the predicate together with the fact that every span-retaining state in this
    /// codec is constructed over codec-core's retained session authority. Enumerated, because a future route could
    /// break it silently:
    ///
    /// - Whole-document open (`provider.rs` and `jsonc/provider.rs`, route slot 0) — `Document` or `AdjacentValue`
    ///   over `input.source()`. The sites that call this method in production. Sound.
    /// - Whole-document reopen (`provider.rs`, `try_reopen_route`) — resets into the same two modes over the same
    ///   authority, and deliberately KEEPS the frontier, which is how one parser serves value after value of an
    ///   adjacent-value stream. Sound.
    /// - The scoped route's element-range materializer (`scoped.rs`) — `OwnedRun` over a session-owned synthetic
    ///   `[...]` buffer that is dropped after the one selection, and it never asks. Refused twice over.
    /// - The nested read of a deferred span (`crate::lazy`) — `OwnedRun`, which is also what makes materialization
    ///   TERMINATE: the nested parse is structurally unable to defer anything of its own.
    /// - The scoped materializer for a non-range selection (`scoped.rs`) — `Document` over the retained input, so it
    ///   would be sound, but it never asks either.
    ///
    /// A new route that decodes a SYNTHETIC buffer under a span-retaining mode would satisfy the predicate and still
    /// dangle. That is the invariant to re-check, and `a_copy_mode_session_refuses_the_frontier_and_publishes_no_span`
    /// is the test that holds the refusal itself.
    pub(crate) fn enable_lazy_frontier(&mut self, depth: usize) {
        if depth >= 1 && self.mode.retains_source_spans() {
            self.lazy_frontier = Some(depth);
        }
    }

    /// Arms the kept-subtree prune hint. Mutually exclusive with the lazy frontier by the engine's lowering (a prune
    /// forces the eager frontier); a deferring session ignores the hint anyway, because the deferred `None` path
    /// already commits nothing.
    pub(crate) fn enable_prune(&mut self, tree: &jqf_codec_core::PruneTree) {
        self.prune = PruneMap::from_transport(tree);
    }

    /// The prune ref governing the value ABOUT to be produced at the current position: [`PRUNE_ALL`] to build it whole,
    /// [`PRUNE_OMIT`] to commit nothing, else the prune-map node its own children resolve against.
    ///
    /// The resolution joins at lookup — an exact key hit, else the governing node's shared element child
    /// ([`jqf_codec_core::PruneTreeNode::member`]'s law; nothing pre-folds element demand into named keys) — and
    /// every unexpected shape resolves to [`PRUNE_ALL`]: over-delivery is always sound (the hint is monotone). Arrays
    /// never omit: omission would shift indices and counts, so an array position without an element node keeps its
    /// value whole.
    fn value_prune(&self, source: ResolvedSource<'_>) -> u32 {
        // One prune law: the non-hoisted read is the hoisted one over the top frame's own state (the two copies drifted
        // once already).
        let top_state = self.frames.as_slice().last().map(|frame| frame.state);
        self.value_prune_hoisted(source, top_state.as_ref())
    }

    /// The pending object key's bytes: a source span reads the input, a stored span reads the builder's staged text
    /// (both already unescaped where it matters — stored text IS the unescaped form, and a source span only exists
    /// for an escape-free key).
    fn key_bytes<'a>(&'a self, source: ResolvedSource<'a>, key: &ObjectKey) -> Option<&'a [u8]> {
        match key {
            ObjectKey::Source(span) => source.bytes().get(span.start() as usize..span.end() as usize),
            ObjectKey::Stored(span) => {
                let builder = self.builder.as_ref()?;
                // SAFETY: the span was produced by `consume_bound_stored_text_span` on this exact builder when the key
                // finished parsing, and nothing consumed it since.
                Some(unsafe { builder.bound_stored_text(*span) }?.as_bytes())
            }
        }
    }

    /// Whether the value about to finish must commit nothing under the armed prune hint.
    fn omitting_value(&self, source: ResolvedSource<'_>) -> bool {
        self.prune.is_some() && self.value_prune(source) == PRUNE_OMIT
    }

    pub(crate) fn with_schema_prototype(
        schema_prototype: DocumentSchemaPrototype,
        diagnostics: DiagnosticCoverage,
        coverage: BuilderCoverage,
        mode: ParseMode,
    ) -> Self {
        let mut state = Self::new(diagnostics, coverage, mode);
        state.schema_prototype = Some(schema_prototype);
        state
    }

    /// Reinitializes this state for one more adjacent value, retaining only a bounded frame workspace from the previous
    /// value.
    ///
    /// Every phase, cursor, builder, product, and source-binding carrier is dropped. Only the request-scoped immutable
    /// schema prototype and bounded frame capacity survive: a value that failed mid-parse leaves no document-local
    /// state behind for the next one. The builder arenas are NOT recycled — a finished document owns them — so only
    /// the safe shell and prototype are reused.
    pub(crate) fn reset(&mut self, diagnostics: DiagnosticCoverage, coverage: BuilderCoverage, mode: ParseMode) {
        self.phase = ParsePhase::Value;
        self.builder = None;
        self.schema = None;
        self.finalizer = None;
        reset_reusable_frames(&mut self.frames);
        self.root = None;
        self.cursor = 0;
        self.diagnostics = diagnostics;
        self.coverage = coverage;
        self.product = None;
        self.binding_stage = None;
        self.source_binding = None;
        self.mode = mode;
        self.deferred = None;
        // A recycled session keeps its frontier POLICY but re-checks it against the new mode: a reset into a mode that
        // recycles its buffer must not inherit permission to name spans of it.
        if !self.mode.retains_source_spans() {
            self.lazy_frontier = None;
        }
        self.source_spans_committed = false;
        self.sealed_len = None;
        self.consumed_offset = None;
        self.open_ended = false;
        self.capacity_reprojected = false;
        // The canonicality verdict belongs to ONE document. It only ever falls, so a session that decoded a
        // non-canonical value would answer "not canonical" for every later one and silently retire the echo.
        self.source_canonical = true;
        // A value that failed mid-parse (or finalized with no following node) must not leak its pending comment run
        // into the next adjacent value.
        self.pending_comments.clear();
        // Nor its suspended trivia-run window: the next value's first trivia run starts from zero granted bytes.
        self.trivia_window = 0;
        // The prune hint belongs to ONE requirement; a reopened session re-arms it from the new requirement after this
        // reset.
        self.prune = None;
    }

    fn initialize(&mut self, source_len: usize, resources: &ResourceContext<'_>) -> Result<(), CodecError> {
        if self.builder.is_none() {
            if let Some(recipe) = &self.dynamic_recipe {
                let (mut builder, schema) =
                    AccountedDocumentBuilder::try_new_prepared_with_coverage(recipe, self.coverage)
                        .map_err(map_data)?;
                builder.set_authoritative_empty_families(AuthoritativeEmptyFamilies::from_family(
                    DocumentCapabilityFamily::Attributes,
                ));
                builder.set_diagnostic_coverage(self.diagnostics);
                self.bind_lazy_materializer(&mut builder);
                builder.install_transients(&mut self.transients);
                // The prototype arm reserves an initial arena slab below; the fact-attaching (dynamic-recipe) arm grows
                // by doubling from empty instead — the same first-guess estimate applies, so large JSONC documents no
                // longer pay log2 growth steps strict documents skip.
                reserve_estimate(&mut builder, source_len, false, resources)?;
                self.builder = Some(builder);
                self.schema = Some(schema);
                return Ok(());
            }
            if self.schema_prototype.is_none() {
                self.schema_prototype = Some(try_schema_prototype(resources)?);
            }
            let prototype = self.schema_prototype.as_ref().ok_or_else(data_contract)?;
            let (mut builder, schema) = prototype
                .try_new_builder_with_coverage(self.coverage)
                .map_err(map_data)?;
            builder.set_authoritative_empty_families(AuthoritativeEmptyFamilies::from_family(
                DocumentCapabilityFamily::Attributes,
            ));
            builder.set_diagnostic_coverage(self.diagnostics);
            self.bind_lazy_materializer(&mut builder);
            builder.install_transients(&mut self.transients);
            reserve_estimate(&mut builder, source_len, self.mode.ends_before_source(), resources)?;
            self.builder = Some(builder);
            self.schema = Some(schema);
        }
        Ok(())
    }

    fn bind_lazy_materializer(&self, builder: &mut Builder) {
        if self.lazy_frontier.is_some() {
            builder.bind_span_materializer(self.span_materializer.unwrap_or(&crate::lazy::JSON_SPAN_MATERIALIZER));
        }
    }

    fn builder(&mut self) -> Result<&mut Builder, CodecError> {
        self.builder.as_mut().ok_or_else(data_contract)
    }

    /// Decodes one complete escape sequence in a single step — the bulk lane (jump the whole sequence at once instead
    /// of one automaton transition per byte), bounded by the cooperative window: the sequence must sit ENTIRELY inside
    /// the granted window and be fully valid, and anything else — a JSON5 special, an invalid escape, a window edge
    /// mid-sequence — returns `false` with the cursor still ON the escape character, so the byte automaton replays it
    /// and every error verdict, error position, and suspension point stays byte-identical to the slow path. Handles
    /// exactly the escapes whose slow arms are grammar-independent (`" \ / b f n r t u`).
    ///
    /// On `true` the cursor is past the sequence, its decoded scalar is pushed to the detached staged-text arena, and
    /// the escape state is still `Plain`.
    fn fast_escape(
        &mut self,
        bytes: &[u8],
        state: &mut StringState,
        staged: &mut String,
        limit: usize,
    ) -> Result<bool, CodecError> {
        let cursor = state.cursor;
        if cursor >= limit {
            return Ok(false);
        }
        let decoded = match bytes[cursor] {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => {
                // The escape disqualifier, exactly as the slow arm: the reference renders `\/` as `/`.
                self.source_canonical = false;
                '/'
            }
            b'b' => '\u{0008}',
            b'f' => '\u{000c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'u' => {
                // The escape disqualifier fires on the SPELLING, before the digits are judged — the slow arm's order,
                // and idempotent with the replay's own clear on fallback.
                self.source_canonical = false;
                // `u` plus four digits, all admitted, all hex — anything less replays through the automaton, which
                // suspends or reports at the exact byte this path declined to judge.
                if cursor + 5 > limit {
                    return Ok(false);
                }
                let (Some(d0), Some(d1), Some(d2), Some(d3)) = (
                    hex(bytes[cursor + 1]),
                    hex(bytes[cursor + 2]),
                    hex(bytes[cursor + 3]),
                    hex(bytes[cursor + 4]),
                ) else {
                    return Ok(false);
                };
                let value = (u16::from(d0) << 12) | (u16::from(d1) << 8) | (u16::from(d2) << 4) | u16::from(d3);
                let (scalar, end) = if (0xd800..=0xdbff).contains(&value) {
                    // A high surrogate needs a complete valid `\uXXXX` low surrogate; on any shortfall the automaton
                    // replays the pair from the high surrogate's digits and raises
                    // `missing-low-surrogate`/`invalid-surrogate-pair` at the slow path's exact positions.
                    if cursor + 11 > limit || bytes[cursor + 5] != b'\\' || bytes[cursor + 6] != b'u' {
                        return Ok(false);
                    }
                    let (Some(l0), Some(l1), Some(l2), Some(l3)) = (
                        hex(bytes[cursor + 7]),
                        hex(bytes[cursor + 8]),
                        hex(bytes[cursor + 9]),
                        hex(bytes[cursor + 10]),
                    ) else {
                        return Ok(false);
                    };
                    let low = (u16::from(l0) << 12) | (u16::from(l1) << 8) | (u16::from(l2) << 4) | u16::from(l3);
                    if !(0xdc00..=0xdfff).contains(&low) {
                        return Ok(false);
                    }
                    (
                        0x1_0000 + ((u32::from(value) - 0xd800) << 10) + (u32::from(low) - 0xdc00),
                        cursor + 11,
                    )
                } else if (0xdc00..=0xdfff).contains(&value) {
                    // A lone low surrogate decodes to U+FFFD, exactly as the slow arm and the reference do.
                    (0xfffd, cursor + 5)
                } else {
                    (u32::from(value), cursor + 5)
                };
                push_char(staged, char::from_u32(scalar).ok_or_else(data_contract)?);
                state.cursor = end;
                return Ok(true);
            }
            _ => return Ok(false),
        };
        push_char(staged, decoded);
        state.cursor = cursor + 1;
        Ok(true)
    }

    /// Reprojects the node/occurrence table capacity from the density observed in the consumed prefix, once, for large
    /// inputs.
    ///
    /// The one-shot [`reserve_estimate`] stays conservative so no small or sparse document over-reserves; on a dense
    /// multi-megabyte document that conservatism leaves the arenas doubling repeatedly, and every doubling holds the
    /// old and new buffers live at once — the peak-memory cost. Once a representative prefix has been decoded the
    /// final counts are predictable (`produced * source_len / consumed`); reserving that (plus a small margin) in one
    /// step collapses the remaining doublings.
    ///
    /// Like the initial estimate this is a hint, never an admission requirement: a denied reservation leaves the parser
    /// growing by amortized doubling, so a document that fits under exact accounting is never rejected (see
    /// [`reserve_hint`] for what "denied" covers). Publication compaction still releases any slack left.
    fn reproject_capacity(&mut self, source_len: usize, resources: &ResourceContext<'_>) -> Result<(), CodecError> {
        if self.capacity_reprojected {
            return Ok(());
        }
        if source_len < CAPACITY_REPROJECT_MIN_SOURCE {
            self.capacity_reprojected = true;
            return Ok(());
        }
        if self.cursor.saturating_mul(CAPACITY_REPROJECT_SAMPLE_DENOMINATOR) < source_len {
            return Ok(());
        }
        self.capacity_reprojected = true;
        let consumed = self.cursor;
        let Some(builder) = self.builder.as_mut() else {
            return Ok(());
        };
        let nodes = builder.node_count();
        if consumed == 0 || nodes < CAPACITY_REPROJECT_MIN_NODES {
            return Ok(());
        }
        let occurrences = builder.occurrence_count();
        // A projection from a dense sample of a non-uniform document can grossly over-reserve: an adversarial input
        // whose first eighth is a dense array of one-byte numbers followed by one giant string samples a node density
        // that the tail never sustains. Clamp every projection to what the whole source could hold at all: no JSON
        // value packs more than one node per ~`MIN_BYTES_PER_NODE` source bytes. The densest shape, `[1,1,1,...]`,
        // approaches two bytes per node from BELOW, so `source_len / MIN_BYTES_PER_NODE` is a conservative ceiling that
        // can undercut the true final count by about one node on tiny inputs — acceptable, because the clamp only
        // lowers an over-estimate and stays a hint that never rejects.
        let ceiling = source_len / MIN_BYTES_PER_NODE;
        let project = |produced: usize| -> usize {
            let projected = (produced as u128 * source_len as u128 / consumed as u128).min(usize::MAX as u128) as usize;
            projected
                .saturating_add(projected / CAPACITY_REPROJECT_MARGIN_DENOMINATOR)
                .min(ceiling)
        };
        reserve_hint(
            builder,
            DocumentCapacity {
                nodes: project(nodes).saturating_sub(nodes),
                occurrences: project(occurrences).saturating_sub(occurrences),
                ..DocumentCapacity::default()
            },
            resources,
        )
    }

    fn prepared_builder(&mut self) -> Result<(&mut Builder, &PreparedDocumentSchema), CodecError> {
        match (&mut self.builder, &self.schema) {
            (Some(builder), Some(schema)) => Ok((builder, schema)),
            _ => Err(data_contract()),
        }
    }

    fn parse_decode<'source>(
        &mut self,
        source: ResolvedSource<'source>,
        context: &mut CodecRunContext<'_, '_>,
    ) -> Result<AccessResult<'source>, CodecError> {
        if source.bytes().len() > u32::MAX as usize {
            return Err(CodecError::new(CodecFailureKind::Overflow));
        }
        // A byte-order mark SPLIT by a read boundary is an incomplete mark, not a wrong byte: what arrived is a proper
        // prefix of the mark and the window ends there. The complete mark is consumed at session open, so only the cut
        // reaches here, and labelling it at the cut is what makes the streaming drive refill — the same law a token
        // cut by the window follows. Only the first value's window can carry a mark at all.
        if self.mode == ParseMode::AdjacentValue
            && source.base_offset() == 0
            && self.offset() == 0
            && byte_order_mark_truncated(source.bytes())
        {
            return Err(invalid(
                source,
                source.bytes().len(),
                diag::EXPECTED_VALUE,
                diag::MSG_EXPECTED_VALUE,
            ));
        }
        let source_len = source.bytes().len();
        self.initialize(source_len, context.resources())?;
        loop {
            self.reproject_capacity(source_len, context.resources())?;
            let progress = match &mut self.phase {
                ParsePhase::Value => self.value_step(source, context.resources())?,
                ParsePhase::String(_) => self.string_step(source, context.resources())?,
                ParsePhase::SealSource => self.seal_source_step(source, context.resources())?,
                ParsePhase::Number(_) => self.number_step(source, context.resources())?,
                ParsePhase::NumberNormalize(_) => self.number_normalize_step(source, context.resources())?,
                // The direct decoder is driven only by `poll_run_direct`, never by the grammar drivers.
                ParsePhase::Trailing => self.trailing_step(source, context.resources())?,
                ParsePhase::Finalize => self.finalize_step(source, context.resources())?,
                ParsePhase::Publish => return self.publish_step(source, context.resources()),
            };
            if !progress {
                // Straight-line decode: replenish the cooperative budget and continue instead of yielding a poll.
                context.replenish_work()?;
            }
        }
    }

    /// Drives the parser over a caller-owned copy-mode buffer and yields the fully self-contained (`'static`) document
    /// once complete, rather than a source-borrowing product.
    ///
    /// The element-stream route that once drove this poll over a recycled run buffer is gone from the product — the
    /// standing access inventory is the two slots (Whole, Exact) plus the record stream. Copy mode itself remains for
    /// the callers that still hand this state a SYNTHETIC buffer whose lifetime does not outlive the poll: the scoped
    /// route's element-range materialization and the lazy frontier's nested span reads. Constructing this state with
    /// [`ParseMode::OwnedRun`] copies every string into the builder arenas so no zero-copy source span is committed,
    /// leaving the finalized product owning all of its bytes. This method returns that owned product directly instead
    /// of narrowing it to the borrowed-buffer lifetime, so a run product can safely escape the run's buffer. A source
    /// binding here is a contract violation (it would mean copy mode was not requested).
    pub(crate) fn poll_owned(
        &mut self,
        source: ResolvedSource<'_>,
        context: &mut CodecRunContext<'_, '_>,
    ) -> Result<OwnedRunPoll, CodecError> {
        if source.bytes().len() > u32::MAX as usize {
            return Err(CodecError::new(CodecFailureKind::Overflow));
        }
        let source_len = source.bytes().len();
        self.initialize(source_len, context.resources())?;
        loop {
            self.reproject_capacity(source_len, context.resources())?;
            let progress = match &mut self.phase {
                ParsePhase::Value => self.value_step(source, context.resources())?,
                ParsePhase::String(_) => self.string_step(source, context.resources())?,
                ParsePhase::SealSource => return Err(data_contract()),
                ParsePhase::Number(_) => self.number_step(source, context.resources())?,
                ParsePhase::NumberNormalize(_) => self.number_normalize_step(source, context.resources())?,
                ParsePhase::Trailing => self.trailing_step(source, context.resources())?,
                ParsePhase::Finalize => self.finalize_step(source, context.resources())?,
                ParsePhase::Publish => {
                    if self.source_binding.is_some() {
                        return Err(data_contract());
                    }
                    return Ok(OwnedRunPoll::Ready(self.product.take().ok_or_else(data_contract)?));
                }
            };
            if !progress {
                return Ok(OwnedRunPoll::Pending);
            }
        }
    }

    fn seal_source_step(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        let sealed = self.sealed_source(source)?;
        if self.binding_stage.is_none() {
            self.binding_stage = Some(jqf_data::DocumentSourceBindingStage::new(sealed).map_err(map_data)?);
        }
        let seal_stage = self.binding_stage.as_mut().ok_or_else(data_contract)?;
        // SAFETY: codec-core retains one immutable source authority for the complete access session and passes that
        // exact authority each poll; `sealed_source` only narrows it, and always to the same extent.
        match unsafe { seal_stage.poll(sealed, resources) }.map_err(map_data)? {
            jqf_data::DocumentSourceBindingPoll::Pending => Ok(false),
            jqf_data::DocumentSourceBindingPoll::Ready(binding) => {
                self.source_binding = Some(binding);
                self.binding_stage = None;
                self.builder()?.bind_source(binding).map_err(map_data)?;
                self.begin_finalize(resources)
            }
        }
    }

    /// The exact source segment this session's document names: everything its root value occupies, and nothing after
    /// it.
    ///
    /// Narrowing preserves the source identity and base offset, so a span, a diagnostic, and the seal all keep naming
    /// the same absolute positions.
    fn sealed_source<'a>(&self, source: ResolvedSource<'a>) -> Result<ResolvedSource<'a>, CodecError> {
        let len = self.sealed_len.ok_or_else(data_contract)?;
        let bytes = source.bytes().get(..len).ok_or_else(data_contract)?;
        Ok(ResolvedSource::new(
            source.source(),
            source.label(),
            bytes,
            source.base_offset(),
        ))
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one transition table keeps JSON token dispatch auditable"
    )]
    fn value_step(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        let offset = self.offset();
        let lead = source.bytes().get(offset).copied();
        if lead.is_some_and(crate::byte_scan::is_json_ws)
            || (self.grammar.comments && lead == Some(b'/'))
            || (self.grammar.json5 && matches!(lead, Some(b'\xe2' | b'\xef')))
        {
            match self.trivia_advance(source, offset, resources)? {
                TriviaAdvance::Skipped(consumed) => {
                    // The S4 whitespace disqualifier: the compact render carries no structural whitespace, so any
                    // skipped whitespace — leading, trailing, or interior — makes the source non-canonical.
                    self.source_canonical = false;
                    self.set_offset(offset + consumed);
                    return Ok(true);
                }
                // Not trivia after all (`/x` under jsonc): token dispatch below owns the byte and raises the proper
                // diagnostic.
                TriviaAdvance::Content => {}
                TriviaAdvance::Yield => return Ok(false),
                TriviaAdvance::Cut => {
                    return Err(invalid(
                        source,
                        source.bytes().len(),
                        diag::EXPECTED_VALUE,
                        diag::MSG_EXPECTED_VALUE,
                    ));
                }
            }
        }
        // The hoisted token loop: the top frame's state plus the cursor live in LOCALS across the run, so the dispatch
        // neither re-loads parser state from memory per token nor round-trips the cursor through `self` — the
        // narrow-store→wide-load forwarding stall at a per-token loop top is what this removes. `self` is touched
        // only at frame pushes/closes, the phase handoffs, and the run end below.
        //
        // Admission stays PER TOKEN (`admit_work_transition`), exactly as the pre-change loop: the meter's protocol is
        // grant-equals-use, so a batched `admit_work_transitions` grant would be consumed even for tokens a phase
        // handoff ends the run before processing, silently shrinking every poll to a fraction of its budget. Per-token
        // admission keeps the meter exact: credits still bound the run (a Pending admission ends the run and the poll),
        // cooperative control is observed at the same cadence as before, and the ledger charge is untouched. The batch
        // is the run of tokens between handoffs.
        let bytes = source.bytes();
        let mut cursor = offset;
        // The top frame's state, hoisted. The Vec slot's copy is ALREADY current at this point (the previous run's
        // end-sync wrote it), so it is only shadowed by the local while this frame is the top; pushes and closes write
        // it back when the frame becomes non-top, and the run end below writes it back on exit.
        let mut top_state = self.frames.as_slice().last().map(|frame| frame.state);
        // Whether the hoisted `top_state` may now differ from its Vec slot. The run-end sync writes the local back only
        // when it diverged: a handoff run (string/number start) leaves the state untouched, so most deferred-mode runs
        // skip the store entirely. Pushes and closes resync the local FROM the Vec, which clears the flag.
        let mut dirty = false;
        // A `?` inside the loop (a builder or admission error) returns without syncing the hoisted locals — the
        // session is latched by any error, so nobody reads the stale cursor afterwards.
        let outcome = 'batch: loop {
            let byte = bytes.get(cursor).copied();
            if byte.is_some_and(crate::byte_scan::is_json_ws)
                || (self.grammar.comments && byte == Some(b'/'))
                || (self.grammar.json5 && matches!(byte, Some(b'\xe2' | b'\xef')))
            {
                // A trivia run pays the LINEAR meter exactly like the pre-batch arm, so a pathological whitespace-heavy
                // stream stays bounded per poll.
                match self.trivia_advance(source, cursor, resources)? {
                    TriviaAdvance::Skipped(consumed) => {
                        cursor += consumed;
                        self.source_canonical = false;
                        continue;
                    }
                    // Not trivia after all (`/x` under jsonc): the token dispatch below owns the byte and raises the
                    // proper diagnostic instead of re-entering this arm forever.
                    TriviaAdvance::Content => {}
                    TriviaAdvance::Yield => break 'batch Ok(false),
                    TriviaAdvance::Cut => {
                        break 'batch Err(invalid(
                            source,
                            bytes.len(),
                            diag::EXPECTED_VALUE,
                            diag::MSG_EXPECTED_VALUE,
                        ));
                    }
                }
            }
            if resources.admit_work_transition()? == WorkAdmission::Pending {
                break 'batch Ok(false);
            }
            match top_state {
                Some(FrameState::ArrayCommaOrEnd) => match byte {
                    Some(b',') => {
                        // The trailing-comma grammar arm: under `jsonc.trailing@1` a comma may sit directly before the
                        // closing `]`, so the state after it admits an end. Under strict RFC 8259 the arm writes the
                        // same `false` it always did — the one predictable branch is the whole cost of the dial.
                        top_state = Some(FrameState::ArrayValueOrEnd {
                            may_end: self.grammar.trailing_commas,
                        });
                        dirty = true;
                        cursor += 1;
                    }
                    Some(b']') => {
                        if self.close_container_hoisted(
                            source,
                            &mut cursor,
                            false,
                            &mut top_state,
                            &mut dirty,
                            resources,
                        )? {
                            break 'batch Ok(true);
                        }
                    }
                    _ => {
                        break 'batch Err(invalid(source, cursor, diag::EXPECTED_COMMA, diag::MSG_COMMA_OR_ARRAY));
                    }
                },
                Some(FrameState::ObjectCommaOrEnd) => match byte {
                    Some(b',') => {
                        top_state = Some(FrameState::ObjectKeyOrEnd {
                            may_end: self.grammar.trailing_commas,
                        });
                        dirty = true;
                        cursor += 1;
                    }
                    Some(b'}') => {
                        if self.close_container_hoisted(
                            source,
                            &mut cursor,
                            true,
                            &mut top_state,
                            &mut dirty,
                            resources,
                        )? {
                            break 'batch Ok(true);
                        }
                    }
                    _ => {
                        break 'batch Err(invalid(source, cursor, diag::EXPECTED_COMMA, diag::MSG_COMMA_OR_OBJECT));
                    }
                },
                Some(FrameState::ObjectColon(key)) => {
                    if byte != Some(b':') {
                        break 'batch Err(invalid(source, cursor, diag::EXPECTED_COLON, diag::MSG_COLON));
                    }
                    // The S4 duplicate-key disqualifier: the compact render keeps only the LAST occurrence of a
                    // repeated key, so a duplicate makes the source's member count differ from its render's. The probe
                    // is decline-only: eight inline fingerprints serve the common small objects without allocating, and
                    // a ninth key — or a fingerprint match — clears the session flag, which only ever costs the
                    // echo, never the bytes. It is gated on the echo lane's probe flag: every other decode pays nothing
                    // for a verdict it cannot consume.
                    if self.source_canonical && self.canonicality_probe {
                        // The probe reads the key through `key_bytes`: a stored key's span resolves against the
                        // builder's staged text, never the source, and an escaped key whose escapes stay minimal
                        // (`"a\nb"`) reaches here with `source_canonical` still true because only `\/` and `\u` clear
                        // it. Slicing the SOURCE with a stored span fingerprints unrelated bytes, misses the duplicate,
                        // and lets the echo republish what a reference render would dedup last-wins.
                        let fingerprint = self.key_bytes(source, &key).map(key_fingerprint);
                        let frame = self.frames.as_mut_slice().last_mut().ok_or_else(data_contract)?;
                        if let Some((seen, count)) = frame.seen_key_fingerprints.as_mut() {
                            // Every exit here clears the flag for the same reason: the window can no longer PROVE the
                            // keys are distinct — either a ninth key overflowed it, a fingerprint repeated inside it,
                            // or the key text was unresolvable. None is a duplicate proof, so the echo is declined and
                            // the bytes are re-rendered.
                            match fingerprint {
                                Some(fingerprint) if *count < 8 && !seen[..*count as usize].contains(&fingerprint) => {
                                    seen[*count as usize] = fingerprint;
                                    *count += 1;
                                }
                                _ => {
                                    self.source_canonical = false;
                                    frame.seen_key_fingerprints = None;
                                }
                            }
                        } else if let Some(fingerprint) = fingerprint {
                            let mut seen = [0u64; 8];
                            seen[0] = fingerprint;
                            frame.seen_key_fingerprints = Some((seen, 1));
                        } else {
                            self.source_canonical = false;
                        }
                    }
                    top_state = Some(FrameState::ObjectValue(key));
                    dirty = true;
                    cursor += 1;
                }
                Some(FrameState::ObjectKeyOrEnd { may_end }) => {
                    if byte == Some(b'}') {
                        if !may_end {
                            break 'batch Err(invalid(source, cursor, diag::TRAILING_COMMA, diag::MSG_TRAILING_COMMA));
                        }
                        if self.close_container_hoisted(
                            source,
                            &mut cursor,
                            true,
                            &mut top_state,
                            &mut dirty,
                            resources,
                        )? {
                            break 'batch Ok(true);
                        }
                    } else if self.grammar.json5 && matches!(byte, Some(b'\'')) {
                        // A JSON5 single-quoted key: the same string machine, with the quote carried in the state.
                        self.phase = ParsePhase::String(StringState {
                            target: StringTarget::Key,
                            start: cursor + 1,
                            cursor: cursor + 1,
                            quote: b'\'',
                            text: None,
                            escape: EscapeState::Plain,
                            had_escape: false,
                            utf8_carry: crate::byte_scan::Utf8Carry::EMPTY,
                        });
                        break 'batch Ok(true);
                    } else if self.grammar.json5 && matches!(byte, Some(b'$' | b'_' | b'a'..=b'z' | b'A'..=b'Z')) {
                        // A JSON5 unquoted IdentifierName key: the identifier bytes ARE the key text (no escapes), so
                        // the key is the source span of the run — the ObjectColon state and the occurrence insertion
                        // read it exactly like an escape-free quoted key's. A non-ASCII identifier key is the
                        // documented ASCII-arm narrowing.
                        let start = cursor;
                        let mut end = cursor + 1;
                        while matches!(
                            bytes.get(end),
                            Some(b'$' | b'_' | b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z')
                        ) {
                            end += 1;
                        }
                        let span = jqf_source::Span::try_new(
                            u32::try_from(start).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?,
                            u32::try_from(end).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?,
                        )
                        .ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?;
                        self.source_spans_committed = true;
                        // An unquoted key is never the encoder's render (json5.jqf@1 writes quoted keys), so the source
                        // is not canonical — the echo lane declines and re-encodes instead of echoing a spelling it
                        // cannot reproduce.
                        self.source_canonical = false;
                        top_state = Some(FrameState::ObjectColon(ObjectKey::Source(span)));
                        dirty = true;
                        cursor = end;
                    } else if byte != Some(b'"') {
                        // The JSONC hint, mirrored at the key site: a comment where a key should be names the dialect
                        // that would accept it.
                        let message = diag::expected_key_message(bytes, cursor);
                        break 'batch Err(invalid(source, cursor, "expected-key", message));
                    } else if let Some(end) = self.try_batch_plain_string(
                        source,
                        cursor,
                        StringTarget::Key,
                        &mut top_state,
                        &mut dirty,
                        resources,
                    )? {
                        cursor = end;
                    } else {
                        self.phase = ParsePhase::String(StringState {
                            target: StringTarget::Key,
                            start: cursor + 1,
                            cursor: cursor + 1,
                            quote: b'"',
                            text: None,
                            escape: EscapeState::Plain,
                            had_escape: false,
                            utf8_carry: crate::byte_scan::Utf8Carry::EMPTY,
                        });
                        break 'batch Ok(true);
                    }
                }
                Some(FrameState::ArrayValueOrEnd { may_end }) if byte == Some(b']') => {
                    if !may_end {
                        break 'batch Err(invalid(source, cursor, diag::TRAILING_COMMA, diag::MSG_TRAILING_COMMA));
                    }
                    if self.close_container_hoisted(
                        source,
                        &mut cursor,
                        false,
                        &mut top_state,
                        &mut dirty,
                        resources,
                    )? {
                        break 'batch Ok(true);
                    }
                }
                _ => {
                    // No frame, a mid-value object/array state, or an array element that is not `]`: dispatch the
                    // value.
                    match byte {
                        Some(b'n') => {
                            // `nan` is the reference's non-finite spelling; `null` stays the lowercase-only literal it
                            // always was. The non-finite check runs FIRST because both start with `n`.
                            if let Some((semantic, length)) = nonfinite_literal(source, cursor, 1.0) {
                                cursor = self.literal_nonfinite_hoisted(
                                    source,
                                    cursor,
                                    length,
                                    semantic,
                                    &mut top_state,
                                    &mut dirty,
                                    resources,
                                )?;
                            } else if nonfinite_spelling_truncated(bytes, cursor) {
                                // The bytes ran out mid-spelling (`na`, `infi`): an incomplete token, labeled at the
                                // cut so the streaming drive holds and refills instead of failing.
                                break 'batch Err(invalid(
                                    source,
                                    bytes.len(),
                                    diag::INVALID_LITERAL,
                                    diag::MSG_INVALID_LITERAL,
                                ));
                            } else if let Some(length) = nonfinite_spelling_prefix_len(bytes, cursor) {
                                // A complete non-finite spelling butted against a non-boundary byte (`nanx`) is one
                                // malformed token, labeled at the offending byte exactly like the bare literals'
                                // `nullx` law.
                                break 'batch Err(invalid(
                                    source,
                                    cursor + length,
                                    diag::INVALID_LITERAL,
                                    diag::MSG_INVALID_LITERAL,
                                ));
                            } else {
                                cursor = self.literal_hoisted(
                                    source,
                                    cursor,
                                    b"null",
                                    PreparedSemanticNode::Null,
                                    0,
                                    &mut top_state,
                                    &mut dirty,
                                    resources,
                                )?;
                            }
                            if top_state.is_none() {
                                self.phase = ParsePhase::Trailing;
                                break 'batch Ok(true);
                            }
                        }
                        Some(b'N' | b'i' | b'I') => {
                            // Uppercase NaN and the inf/infinity spellings have no lowercase-literal twin; a non-match
                            // is an expected-value error, exactly as before this widening.
                            if let Some((semantic, length)) = nonfinite_literal(source, cursor, 1.0) {
                                cursor = self.literal_nonfinite_hoisted(
                                    source,
                                    cursor,
                                    length,
                                    semantic,
                                    &mut top_state,
                                    &mut dirty,
                                    resources,
                                )?;
                            } else if nonfinite_spelling_truncated(bytes, cursor) {
                                break 'batch Err(invalid(
                                    source,
                                    bytes.len(),
                                    diag::INVALID_LITERAL,
                                    diag::MSG_INVALID_LITERAL,
                                ));
                            } else if let Some(length) = nonfinite_spelling_prefix_len(bytes, cursor) {
                                break 'batch Err(invalid(
                                    source,
                                    cursor + length,
                                    diag::INVALID_LITERAL,
                                    diag::MSG_INVALID_LITERAL,
                                ));
                            } else {
                                break 'batch Err(invalid(
                                    source,
                                    cursor,
                                    diag::EXPECTED_VALUE,
                                    diag::MSG_EXPECTED_VALUE,
                                ));
                            }
                            if top_state.is_none() {
                                self.phase = ParsePhase::Trailing;
                                break 'batch Ok(true);
                            }
                        }
                        Some(b's' | b'S') => {
                            // `snan`: the non-finite family's one missing spelling, completed. A NaN is a NaN —
                            // accepted at every dial position like `nan`, with the same value-boundary law (`snanx` is
                            // one malformed token).
                            if let Some((semantic, length)) = nonfinite_literal(source, cursor, 1.0) {
                                cursor = self.literal_nonfinite_hoisted(
                                    source,
                                    cursor,
                                    length,
                                    semantic,
                                    &mut top_state,
                                    &mut dirty,
                                    resources,
                                )?;
                            } else if nonfinite_spelling_truncated(bytes, cursor) {
                                break 'batch Err(invalid(
                                    source,
                                    bytes.len(),
                                    diag::INVALID_LITERAL,
                                    diag::MSG_INVALID_LITERAL,
                                ));
                            } else if let Some(length) = nonfinite_spelling_prefix_len(bytes, cursor) {
                                break 'batch Err(invalid(
                                    source,
                                    cursor + length,
                                    diag::INVALID_LITERAL,
                                    diag::MSG_INVALID_LITERAL,
                                ));
                            } else {
                                break 'batch Err(invalid(
                                    source,
                                    cursor,
                                    diag::EXPECTED_VALUE,
                                    diag::MSG_EXPECTED_VALUE,
                                ));
                            }
                            if top_state.is_none() {
                                self.phase = ParsePhase::Trailing;
                                break 'batch Ok(true);
                            }
                        }
                        Some(b't') => {
                            cursor = self.literal_hoisted(
                                source,
                                cursor,
                                b"true",
                                PreparedSemanticNode::Bool(true),
                                1,
                                &mut top_state,
                                &mut dirty,
                                resources,
                            )?;
                            if top_state.is_none() {
                                self.phase = ParsePhase::Trailing;
                                break 'batch Ok(true);
                            }
                        }
                        Some(b'f') => {
                            cursor = self.literal_hoisted(
                                source,
                                cursor,
                                b"false",
                                PreparedSemanticNode::Bool(false),
                                1,
                                &mut top_state,
                                &mut dirty,
                                resources,
                            )?;
                            if top_state.is_none() {
                                self.phase = ParsePhase::Trailing;
                                break 'batch Ok(true);
                            }
                        }
                        Some(b'+') => {
                            // JSON numbers have no leading `+`; the reference accepts `+nan` / `+Infinity` as
                            // non-finite number spellings, and nothing else with a leading plus.
                            if let Some((semantic, length)) = nonfinite_literal(source, cursor + 1, 1.0) {
                                cursor = self.literal_nonfinite_hoisted(
                                    source,
                                    cursor,
                                    length + 1,
                                    semantic,
                                    &mut top_state,
                                    &mut dirty,
                                    resources,
                                )?;
                            } else if nonfinite_spelling_truncated(bytes, cursor + 1) {
                                break 'batch Err(invalid(
                                    source,
                                    bytes.len(),
                                    diag::INVALID_LITERAL,
                                    diag::MSG_INVALID_LITERAL,
                                ));
                            } else if let Some(length) = nonfinite_spelling_prefix_len(bytes, cursor + 1) {
                                break 'batch Err(invalid(
                                    source,
                                    cursor + 1 + length,
                                    diag::INVALID_LITERAL,
                                    diag::MSG_INVALID_LITERAL,
                                ));
                            } else if (self.grammar.lenient || self.grammar.json5)
                                && matches!(bytes.get(cursor + 1), Some(b'0'..=b'9' | b'.'))
                            {
                                // Lenient / JSON5: a leading `+` followed by a digit or a bare point (`+1`, `+.5`) is
                                // the accepted spelling of the same number. The span starts AFTER the plus, so the
                                // canonical text never carries it, and the spelling is marked non-canonical.
                                self.phase = ParsePhase::Number(NumberState::start_at(cursor + 1, true)?);
                                break 'batch Ok(true);
                            } else {
                                break 'batch Err(invalid(
                                    source,
                                    cursor,
                                    diag::EXPECTED_VALUE,
                                    diag::MSG_EXPECTED_VALUE,
                                ));
                            }
                            if top_state.is_none() {
                                self.phase = ParsePhase::Trailing;
                                break 'batch Ok(true);
                            }
                        }
                        Some(b'.') if self.grammar.lenient || self.grammar.json5 => {
                            // Lenient / JSON5: a bare point followed by a digit (`.5`) is the accepted spelling of the
                            // decimal; a point without a digit stays the expected-value refusal below.
                            if matches!(bytes.get(cursor + 1), Some(b'0'..=b'9')) {
                                self.phase = ParsePhase::Number(NumberState::start_at(cursor, true)?);
                                break 'batch Ok(true);
                            }
                            break 'batch Err(invalid(source, cursor, diag::EXPECTED_VALUE, diag::MSG_EXPECTED_VALUE));
                        }
                        Some(b'-') => {
                            // `-Infinity` / `-inf` / `-nan` are the reference's signed non-finite spellings; anything
                            // else after `-` is a number, whose grammar rejects it with the invalid-number class.
                            if let Some((semantic, length)) = nonfinite_literal(source, cursor + 1, -1.0) {
                                cursor = self.literal_nonfinite_hoisted(
                                    source,
                                    cursor,
                                    length + 1,
                                    semantic,
                                    &mut top_state,
                                    &mut dirty,
                                    resources,
                                )?;
                            } else if nonfinite_spelling_truncated(bytes, cursor + 1) {
                                break 'batch Err(invalid(
                                    source,
                                    bytes.len(),
                                    diag::INVALID_LITERAL,
                                    diag::MSG_INVALID_LITERAL,
                                ));
                            } else if let Some(length) = nonfinite_spelling_prefix_len(bytes, cursor + 1) {
                                break 'batch Err(invalid(
                                    source,
                                    cursor + 1 + length,
                                    diag::INVALID_LITERAL,
                                    diag::MSG_INVALID_LITERAL,
                                ));
                            } else if let Some(end) =
                                self.try_batch_verbatim_integer(source, cursor, &mut top_state, &mut dirty, resources)?
                            {
                                cursor = end;
                            } else {
                                self.phase = ParsePhase::Number(NumberState::start_at(cursor, false)?);
                                break 'batch Ok(true);
                            }
                            if top_state.is_none() {
                                self.phase = ParsePhase::Trailing;
                                break 'batch Ok(true);
                            }
                        }
                        Some(b'0'..=b'9') => {
                            if let Some(end) =
                                self.try_batch_verbatim_integer(source, cursor, &mut top_state, &mut dirty, resources)?
                            {
                                cursor = end;
                                if top_state.is_none() {
                                    self.phase = ParsePhase::Trailing;
                                    break 'batch Ok(true);
                                }
                            } else {
                                self.phase = ParsePhase::Number(NumberState::start_at(cursor, false)?);
                                break 'batch Ok(true);
                            }
                        }
                        Some(b'"') => {
                            if let Some(end) = self.try_batch_plain_string(
                                source,
                                cursor,
                                StringTarget::Value,
                                &mut top_state,
                                &mut dirty,
                                resources,
                            )? {
                                cursor = end;
                                if top_state.is_none() {
                                    self.phase = ParsePhase::Trailing;
                                    break 'batch Ok(true);
                                }
                            } else {
                                self.phase = ParsePhase::String(StringState {
                                    target: StringTarget::Value,
                                    start: cursor + 1,
                                    cursor: cursor + 1,
                                    quote: b'"',
                                    text: None,
                                    escape: EscapeState::Plain,
                                    had_escape: false,
                                    utf8_carry: crate::byte_scan::Utf8Carry::EMPTY,
                                });
                                break 'batch Ok(true);
                            }
                        }
                        Some(b'\'') if self.grammar.json5 => {
                            // A JSON5 single-quoted VALUE string.
                            self.phase = ParsePhase::String(StringState {
                                target: StringTarget::Value,
                                start: cursor + 1,
                                cursor: cursor + 1,
                                quote: b'\'',
                                text: None,
                                escape: EscapeState::Plain,
                                had_escape: false,
                                utf8_carry: crate::byte_scan::Utf8Carry::EMPTY,
                            });
                            break 'batch Ok(true);
                        }
                        Some(b'[') => {
                            self.open_container_hoisted(
                                source,
                                &mut cursor,
                                false,
                                &mut top_state,
                                &mut dirty,
                                resources,
                            )?;
                        }
                        Some(b'{') => {
                            self.open_container_hoisted(
                                source,
                                &mut cursor,
                                true,
                                &mut top_state,
                                &mut dirty,
                                resources,
                            )?;
                        }
                        _ => {
                            // The JSONC hint: a comment start where a value should be is the `tsconfig.json` user story
                            // — the diagnostic names the dialect that would accept it. Detection is a hint, never
                            // auto-selection.
                            let message = diag::expected_value_message(bytes, cursor);
                            break 'batch Err(invalid(source, cursor, diag::EXPECTED_VALUE, message));
                        }
                    }
                }
            }
        };
        // Run boundary: write the hoisted cursor back, and the top state back only when it diverged from its Vec slot.
        self.cursor = cursor;
        if dirty && let Some(state) = top_state {
            self.frames.as_mut_slice().last_mut().ok_or_else(data_contract)?.state = state;
        }
        outcome
    }

    /// Completes an in-window unescaped `"..."` inside the token batch. `None` means the token is not a plain in-window
    /// string (escape, non-ASCII, missing closer) and the caller must hand off to the string phase. Charging is the
    /// batch's already-admitted token.
    fn try_batch_plain_string(
        &mut self,
        source: ResolvedSource<'_>,
        cursor: usize,
        target: StringTarget,
        top_state: &mut Option<FrameState>,
        dirty: &mut bool,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<usize>, CodecError> {
        if !self.mode.retains_source_spans() {
            return Ok(None);
        }
        let bytes = source.bytes();
        // `start = cursor + 1` cannot exceed `len`: `cursor` is the matched opening quote's position (the guard that
        // made this one dead is gone).
        let start = cursor + 1;
        let run_end = plain_string_run_end(bytes, start, bytes.len());
        if run_end >= bytes.len() || bytes[run_end] != b'"' {
            return Ok(None);
        }
        let span = jqf_source::Span::try_new(
            u32::try_from(start).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?,
            u32::try_from(run_end).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?,
        )
        .ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?;
        self.source_spans_committed = true;
        match target {
            StringTarget::Key => {
                *top_state = Some(FrameState::ObjectColon(ObjectKey::Source(span)));
                *dirty = true;
            }
            StringTarget::Value => {
                let node = if self.deferring() || self.omitting_value_hoisted(source, top_state.as_ref()) {
                    None
                } else {
                    let (builder, schema) = self.prepared_builder()?;
                    let kind = schema.node_kind(3).ok_or_else(data_contract)?;
                    // SAFETY: the run is copy-safe ASCII (plain-string scan) between in-bounds quotes; codec-core owns
                    // the authority.
                    Some(
                        unsafe { builder.add_prepared_bound_source_string_node(schema, kind, span, resources) }
                            .map_err(map_data)?,
                    )
                };
                self.attach_hoisted(source, top_state, dirty, node, resources)?;
            }
        }
        Ok(Some(run_end + 1))
    }

    /// Completes an in-window verbatim integer inside the token batch. `None` means the token is not a source-verbatim
    /// integer (fraction, exponent, leading zero, incomplete window) and the caller must hand off to the number phase.
    fn try_batch_verbatim_integer(
        &mut self,
        source: ResolvedSource<'_>,
        cursor: usize,
        top_state: &mut Option<FrameState>,
        dirty: &mut bool,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<usize>, CodecError> {
        if !self.mode.retains_source_spans() {
            return Ok(None);
        }
        let bytes = source.bytes();
        let Some(end) = verbatim_integer_end(bytes, cursor) else {
            return Ok(None);
        };
        if end == bytes.len() {
            self.open_ended = true;
        }
        let span = jqf_source::Span::try_new(
            u32::try_from(cursor).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?,
            u32::try_from(end).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?,
        )
        .ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?;
        self.source_spans_committed = true;
        let node = if self.deferring() || self.omitting_value_hoisted(source, top_state.as_ref()) {
            None
        } else {
            let (builder, schema) = self.prepared_builder()?;
            let kind = schema.node_kind(2).ok_or_else(data_contract)?;
            // SAFETY: `verbatim_integer_end` is the in-window reading of `number_is_verbatim_source`; the span is
            // in-bounds.
            Some(
                unsafe { builder.add_prepared_bound_source_integer_node(schema, kind, span, resources) }
                    .map_err(map_data)?,
            )
        };
        self.attach_hoisted(source, top_state, dirty, node, resources)?;
        Ok(Some(end))
    }

    /// Wires a completed value's node into the enclosing container, reading the owner and key from the hoisted top
    /// state instead of the frame's Vec slot (whose state is stale inside a token batch). The root case (`top_state ==
    /// None`) installs the node as the document root.
    ///
    /// This is the one implementation of the occurrence law; [`Self::attach`] delegates here by taking the top state
    /// out of the Vec and writing it back.
    fn attach_hoisted(
        &mut self,
        _source: ResolvedSource<'_>,
        top_state: &mut Option<FrameState>,
        dirty: &mut bool,
        node: Option<NodeId>,
        resources: &ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        // The pending comment run flushes onto the value node that completes here — the comment-ownership law read in
        // forward: the first node whose span starts at or after a comment's end owns it as a LEADING comment. A comment
        // before a key lands on the key's VALUE because the key completes without touching this funnel; a comment
        // before a container lands on the container because the container node exists at open.
        if let Some(node) = node
            && !self.pending_comments.is_empty()
        {
            self.attach_pending_comments(node, resources)?;
        }
        let Some(top) = top_state.as_mut() else {
            let node = node.ok_or_else(data_contract)?;
            if self.root.replace(node).is_some() {
                return Err(data_contract());
            }
            return Ok(());
        };
        *dirty = true;
        let owner = self.frames.as_slice().last().ok_or_else(data_contract)?.node;
        // An array value completion leaves the frame in `ArrayCommaOrEnd`; an object member completion must leave it in
        // `ObjectCommaOrEnd` (the dispatch states are distinct: the object's `,` returns to `ObjectKeyOrEnd` and its
        // `}` closes). The `mem::replace` seeds the array state, and the object arm below overwrites it.
        let state = mem::replace(top, FrameState::ArrayCommaOrEnd);
        match state {
            FrameState::ArrayValueOrEnd { .. } => {
                // A value INSIDE a deferred subtree commits nothing: the frame transition is the grammar, the
                // occurrence would be structure the toucher rebuilds from the span.
                if let Some(node) = node {
                    let (builder, schema) = self.prepared_builder()?;
                    let role = schema.occurrence_role(0).ok_or_else(data_contract)?;
                    builder
                        .add_prepared_occurrence(schema, LocalOwnerRef::Node(owner), role, None, node, resources)
                        .map_err(map_data)?;
                }
            }
            FrameState::ObjectValue(key) => {
                let Some(node) = node else {
                    *top = FrameState::ObjectCommaOrEnd;
                    return Ok(());
                };
                let (builder, schema) = self.prepared_builder()?;
                let role = schema.occurrence_role(1).ok_or_else(data_contract)?;
                match key {
                    ObjectKey::Stored(span) => {
                        // SAFETY: the span was consumed from this retained builder when the object key finished
                        // parsing.
                        unsafe {
                            builder.add_prepared_bound_stored_occurrence(
                                schema,
                                LocalOwnerRef::Node(owner),
                                role,
                                span,
                                node,
                                resources,
                            )
                        }
                        .map_err(map_data)?
                    }
                    ObjectKey::Source(span) => {
                        // SAFETY: the capacity scan and parser validated this key span against the same immutable bound
                        // authority.
                        unsafe {
                            builder.add_prepared_bound_source_occurrence(
                                schema,
                                LocalOwnerRef::Node(owner),
                                role,
                                span,
                                node,
                                resources,
                            )
                        }
                        .map_err(map_data)?
                    }
                };
                *top = FrameState::ObjectCommaOrEnd;
            }
            state => {
                *top = state;
                return Err(data_contract());
            }
        }
        Ok(())
    }

    /// Pushes one container frame inside a token batch, keeping the hoisted top state authoritative: the previous top's
    /// (possibly transitioned) state is written back to its Vec slot first, then the new frame is pushed with its
    /// initial state, and the caller's `top_state` local becomes the new frame's state.
    fn push_container_frame_hoisted(
        &mut self,
        object: bool,
        prune: u32,
        node: NodeId,
        top_state: &mut Option<FrameState>,
        dirty: &mut bool,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        let depth = resources.enter_nesting_owned()?;
        let state = if object {
            FrameState::ObjectKeyOrEnd { may_end: true }
        } else {
            FrameState::ArrayValueOrEnd { may_end: true }
        };
        if let Some(state) = *top_state {
            self.frames.as_mut_slice().last_mut().ok_or_else(data_contract)?.state = state;
        }
        self.frames.push(Frame {
            node,
            state,
            _depth: depth,
            seen_key_fingerprints: None,
            prune,
        });
        *top_state = Some(state);
        // The new top's Vec slot already holds its initial state, so the local and the Vec agree again.
        *dirty = false;
        Ok(())
    }

    /// Opens one container inside a token batch: attaches the container node to the enclosing value (or installs it as
    /// the root), pushes its frame, and advances the cursor. The deferred, frontier, and prune-omit shapes commit no
    /// node, exactly as the eager open's shared tail did.
    fn open_container_hoisted(
        &mut self,
        source: ResolvedSource<'_>,
        cursor: &mut usize,
        object: bool,
        top_state: &mut Option<FrameState>,
        dirty: &mut bool,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        let next = *cursor + 1;
        let zero_node = NodeId::try_from_index(0).ok_or_else(data_contract)?;
        if let Some(deferred) = self.deferred.as_mut() {
            deferred.nesting = deferred.nesting.checked_add(1).ok_or_else(data_contract)?;
            // A container nested INSIDE the deferred subtree transitions its parent's frame state exactly when the
            // eager open does — the grammar is the same walk — it just commits no node or occurrence.
            self.attach_hoisted(source, top_state, dirty, None, resources)?;
            self.push_container_frame_hoisted(object, PRUNE_ALL, zero_node, top_state, dirty, resources)?;
        } else if self.lazy_frontier.is_some_and(|frontier| self.frames.len() >= frontier) {
            // The container that STARTS the deferral does NOT attach here: its node does not exist until the subtree
            // closes, so its parent stays mid-value and takes both the occurrence and the transition then.
            self.deferred = Some(DeferredContainer {
                open: next - 1,
                nesting: 1,
                container: if object {
                    jqf_data::ContainerSpanKind::Object
                } else {
                    jqf_data::ContainerSpanKind::Array
                },
            });
            self.push_container_frame_hoisted(object, PRUNE_ALL, zero_node, top_state, dirty, resources)?;
        } else {
            let prune = if self.prune.is_some() {
                self.value_prune_hoisted(source, top_state.as_ref())
            } else {
                PRUNE_ALL
            };
            if prune == PRUNE_OMIT {
                // The omitted container mirrors a nested deferred open: the parent takes its grammar transition with no
                // occurrence, the subtree's frames run committing nothing, and close pops with nothing to do.
                self.attach_hoisted(source, top_state, dirty, None, resources)?;
                self.push_container_frame_hoisted(object, PRUNE_OMIT, zero_node, top_state, dirty, resources)?;
            } else {
                let (builder, schema) = self.prepared_builder()?;
                let (kind_slot, role_slot) = if object { (5, 1) } else { (4, 0) };
                let kind = schema.node_kind(kind_slot).ok_or_else(data_contract)?;
                let role = schema.occurrence_role(role_slot).ok_or_else(data_contract)?;
                let semantic = if object {
                    PreparedSemanticNode::Object(role)
                } else {
                    PreparedSemanticNode::Array(role)
                };
                let node = builder
                    .add_prepared_node(schema, kind, semantic, resources)
                    .map_err(map_data)?;
                // The structural splice's anchor: a BUILT container carries no source span of its own (only the
                // deferred path binds `ContainerSpan` semantic nodes), so the opening delimiter is recorded out-of-band
                // — in node order, before any of the subtree's own records — and the splice scans from it to name
                // the container's region and observe its separator run. Out-of-band like a leaf: the semantic is the
                // real object/array, the record is an addressing channel.
                self.record_container_anchor(node, *cursor, resources)?;
                self.attach_hoisted(source, top_state, dirty, Some(node), resources)?;
                self.push_container_frame_hoisted(object, prune, node, top_state, dirty, resources)?;
            }
        }
        *cursor = next;
        Ok(())
    }

    /// Closes the top container frame inside a token batch: validates the popped frame's kind, advances the cursor,
    /// runs the deferred-subtree completion, and re-hoists the parent's state. Returns `Ok(true)` when the root value
    /// just completed (the caller transitions to Trailing and ends the batch), `Ok(false)` to continue.
    fn close_container_hoisted(
        &mut self,
        source: ResolvedSource<'_>,
        cursor: &mut usize,
        object: bool,
        top_state: &mut Option<FrameState>,
        dirty: &mut bool,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        let state_was_object = matches!(
            top_state,
            Some(FrameState::ObjectKeyOrEnd { .. } | FrameState::ObjectCommaOrEnd)
        );
        if object != state_was_object {
            return Err(data_contract());
        }
        if self.frames.pop().is_none() {
            return Err(data_contract());
        }
        *cursor += 1;
        if let Some(deferred) = self.deferred.as_mut() {
            deferred.nesting -= 1;
            if deferred.nesting == 0 {
                let deferred = self.deferred.take().ok_or_else(data_contract)?;
                self.commit_container_span(source, &deferred, *cursor, resources)?;
            }
        }
        // The new top is the parent below, whose Vec state is current (it was written back when it became non-top, and
        // a span commit's attach transitioned it in place); re-hoist it once, after either path. The local is stale and
        // unused between the pop and this load.
        *top_state = self.frames.as_slice().last().map(|frame| frame.state);
        // The local was re-hoisted FROM the Vec, so the two agree again.
        *dirty = false;
        if self.frames.is_empty() {
            self.phase = ParsePhase::Trailing;
            return Ok(true);
        }
        Ok(false)
    }

    /// One bare literal completion inside the token batch: validates the spelling and its value boundary, materializes
    /// the node (unless the subtree is deferred or omitted), attaches it, and returns the advanced cursor.
    #[expect(
        clippy::too_many_arguments,
        reason = "one completion law: spelling, semantic, kind slot, and the hoisted top state travel together"
    )]
    fn literal_hoisted(
        &mut self,
        source: ResolvedSource<'_>,
        cursor: usize,
        spelling: &[u8],
        semantic: PreparedSemanticNode,
        kind_slot: usize,
        top_state: &mut Option<FrameState>,
        dirty: &mut bool,
        resources: &mut ResourceContext<'_>,
    ) -> Result<usize, CodecError> {
        let bytes = source.bytes();
        if bytes.get(cursor..cursor + spelling.len()) != Some(spelling) {
            // A spelling cut off by the END of the provided window is an incomplete token, not a wrong byte: label the
            // cut at the window's end so the streaming adjacent-value drive holds and refills. A wrong byte inside the
            // window keeps its own position.
            let at = if bytes.len() < cursor + spelling.len() && spelling.starts_with(&bytes[cursor..]) {
                bytes.len()
            } else {
                cursor
            };
            return Err(invalid(source, at, diag::INVALID_LITERAL, diag::MSG_INVALID_LITERAL));
        }
        // A literal must end at a value boundary; a bare-word suffix (`truex`, `nullx`, `true1`) is one malformed token
        // that the reference rejects wholesale before emitting, so the check happens here, before any node is built.
        if let Some(&next) = bytes.get(cursor + spelling.len())
            && !is_value_boundary(next)
        {
            return Err(invalid(
                source,
                cursor + spelling.len(),
                diag::INVALID_LITERAL,
                diag::MSG_INVALID_LITERAL,
            ));
        }
        // No byte followed the spelling, so no delimiter closed it — the bytes ran out. The token is complete and
        // published, but the next byte would have decided between this literal and a malformed one.
        if cursor + spelling.len() == bytes.len() {
            self.open_ended = true;
        }
        let node = if self.deferring() || self.omitting_value_hoisted(source, top_state.as_ref()) {
            None
        } else {
            let (builder, schema) = self.prepared_builder()?;
            let kind = schema.node_kind(kind_slot).ok_or_else(data_contract)?;
            Some(
                builder
                    .add_prepared_node(schema, kind, semantic, resources)
                    .map_err(map_data)?,
            )
        };
        // A bool or null token is stored as its semantic with no span of its own; record the authored token out-of-band
        // so the edit lane can patch a CHANGED leaf instead of flooring.
        if let Some(node) = node {
            let span = jqf_source::Span::try_new(
                u32::try_from(cursor).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?,
                u32::try_from(cursor + spelling.len()).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?,
            )
            .ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?;
            self.record_leaf_span(node, span, resources)?;
        }
        self.attach_hoisted(source, top_state, dirty, node, resources)?;
        Ok(cursor + spelling.len())
    }

    /// Commits a non-finite number literal inside the token batch: the `json.number@1` node kind (slot 2), exactly as
    /// the number path's own materialization does, then the ordinary literal attach/advance law.
    ///
    /// A non-finite spelling is never its own render — `NaN`, `Infinity` and their lenient siblings all render as
    /// `null` — so the source-canonical flag clears here exactly as it does at the clamp and hex sites.
    #[expect(
        clippy::too_many_arguments,
        reason = "one completion law: length, semantic, and the hoisted top state travel together"
    )]
    fn literal_nonfinite_hoisted(
        &mut self,
        source: ResolvedSource<'_>,
        cursor: usize,
        length: usize,
        semantic: PreparedSemanticNode,
        top_state: &mut Option<FrameState>,
        dirty: &mut bool,
        resources: &mut ResourceContext<'_>,
    ) -> Result<usize, CodecError> {
        // `inf` is a proper prefix of `infinity`, so a non-finite spelling that runs to the last byte the decoder was
        // given is open-ended for the same reason a bare number is: no delimiter closed it.
        if cursor + length == source.bytes().len() {
            self.open_ended = true;
        }
        self.source_canonical = false;
        let node = if self.deferring() || self.omitting_value_hoisted(source, top_state.as_ref()) {
            None
        } else {
            let (builder, schema) = self.prepared_builder()?;
            let kind = schema.node_kind(2).ok_or_else(data_contract)?;
            Some(
                builder
                    .add_prepared_node(schema, kind, semantic, resources)
                    .map_err(map_data)?,
            )
        };
        // A non-finite spelling is stored as Float bits whose render is never the authored token (`Infinity` renders as
        // the clamped widest binary64), so record the authored span out-of-band exactly as the bool/null twins above do
        // — without it the edit lane would floor this leaf instead of patching it.
        if let Some(node) = node {
            let span = jqf_source::Span::try_new(
                u32::try_from(cursor).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?,
                u32::try_from(cursor + length).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?,
            )
            .ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?;
            self.record_leaf_span(node, span, resources)?;
        }
        self.attach_hoisted(source, top_state, dirty, node, resources)?;
        Ok(cursor + length)
    }

    /// The hoisted-top variant of [`Self::value_prune`]: the frame's prune slot and the value's key come from the Vec
    /// slot and the hoisted top state respectively, since the state is stale in the Vec inside a batch.
    fn value_prune_hoisted(&self, source: ResolvedSource<'_>, top_state: Option<&FrameState>) -> u32 {
        let Some(map) = self.prune.as_ref() else {
            return PRUNE_ALL;
        };
        let Some(frame) = self.frames.as_slice().last() else {
            // The root value (of each adjacent value) decodes under the root.
            return jqf_codec_core::PruneTree::ROOT;
        };
        match frame.prune {
            PRUNE_ALL => PRUNE_ALL,
            PRUNE_OMIT => PRUNE_OMIT,
            id => {
                let Some(node) = map.node(id) else {
                    return PRUNE_ALL;
                };
                if node.all {
                    return PRUNE_ALL;
                }
                let child = match top_state {
                    Some(FrameState::ObjectValue(key)) => {
                        let Some(bytes) = self.key_bytes(source, key) else {
                            return PRUNE_ALL;
                        };
                        node.member(bytes).unwrap_or(PRUNE_OMIT)
                    }
                    Some(FrameState::ArrayValueOrEnd { .. }) => node.element.unwrap_or(PRUNE_ALL),
                    // No other frame state produces a value; keep whole.
                    _ => PRUNE_ALL,
                };
                match child {
                    PRUNE_ALL | PRUNE_OMIT => child,
                    child if map.node(child).is_some_and(|node| node.all) => PRUNE_ALL,
                    child => child,
                }
            }
        }
    }

    /// The hoisted-top variant of [`Self::omitting_value`].
    fn omitting_value_hoisted(&self, source: ResolvedSource<'_>, top_state: Option<&FrameState>) -> bool {
        self.prune.is_some() && self.value_prune_hoisted(source, top_state) == PRUNE_OMIT
    }

    fn string_step(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        let phase = mem::replace(&mut self.phase, ParsePhase::Value);
        let ParsePhase::String(mut state) = phase else {
            unreachable!()
        };
        let remaining = source.bytes().len().saturating_sub(state.cursor);
        let Some(granted) = admit_bytes(resources, remaining)? else {
            self.phase = ParsePhase::String(state);
            return Ok(false);
        };
        if granted == 0 {
            return Err(invalid(
                source,
                state.cursor,
                diag::UNTERMINATED_STRING,
                diag::MSG_UNTERMINATED_STRING,
            ));
        }
        // Copy mode: the published document must outlive the session-owned buffer it is being built from, so its
        // strings are copied into the builder arenas instead of naming bytes that will be recycled. On first entry
        // `cursor == start`, so no already-scanned prefix needs copying; on resume `text` is already set and this is
        // skipped.
        if !self.mode.retains_source_spans() && state.text.is_none() {
            let decoded = self.builder()?.begin_text(resources).map_err(map_data)?;
            state.text = Some(decoded);
        }
        // The streaming window: an ESCAPED string's staged arena is DETACHED from the builder for the whole window —
        // its stage identity is validated ONCE here (or at the first `\` that opens it) instead of once per decoded
        // chunk — and every decoded chunk pushes straight into it. The arena is restored on EVERY exit — close,
        // suspension, or error — before the exit is acted on, so the builder never observes a fork.
        let mut staged = if state.had_escape {
            let text_stage = state.text.as_ref().ok_or_else(data_contract)?;
            Some(self.builder()?.take_staged_text(text_stage).map_err(map_data)?)
        } else {
            None
        };
        let limit = state.cursor + granted;
        let exit = self.string_window(source, &mut state, &mut staged, limit, resources);
        if let Some(text) = staged {
            let text_stage = state.text.as_mut().ok_or_else(data_contract)?;
            self.builder()?
                .restore_staged_text(text_stage, text)
                .map_err(map_data)?;
        }
        match exit? {
            StringWindowExit::Suspended => {
                self.phase = ParsePhase::String(state);
                Ok(true)
            }
            StringWindowExit::Closed => {
                if let Some(mut stage) = state.text.take() {
                    let text = if state.had_escape {
                        // Every decoded chunk was pushed into the detached arena as it was produced and the restore
                        // above settled the stage; the close only finishes the entry.
                        self.builder()?.finish_text(stage, resources).map_err(map_data)?
                    } else {
                        // A no-escape string appends its source bytes to the text arena DIRECTLY — one copy, one
                        // validated append. Every byte of the slice was validated during the scan (ASCII runs by the
                        // plain-string scan's contract, non-ASCII blocks by the block validator), so the unchecked
                        // conversion is sound.
                        let builder = self.builder()?;
                        let value =
                            unsafe { core::str::from_utf8_unchecked(&source.bytes()[state.start..state.cursor - 1]) };
                        builder.append_text(&mut stage, value, resources).map_err(map_data)?;
                        builder.finish_text(stage, resources).map_err(map_data)?
                    };
                    // In span-retaining mode this arm is reached only for an ESCAPED string (the escape path is what
                    // sets `text`); record the inner content span so a changed escaped leaf patches instead of
                    // flooring.
                    let authored = if self.mode.retains_source_spans() {
                        Some(
                            jqf_source::Span::try_new(
                                u32::try_from(state.start).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?,
                                u32::try_from(state.cursor - 1)
                                    .map_err(|_| CodecError::new(CodecFailureKind::Overflow))?,
                            )
                            .ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?,
                        )
                    } else {
                        None
                    };
                    return self.finish_string(source, state.target, text, authored, resources);
                }
                let span = jqf_source::Span::try_new(
                    u32::try_from(state.start).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?,
                    u32::try_from(state.cursor - 1).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?,
                )
                .ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?;
                self.finish_source_string(source, state.target, span, resources)
            }
        }
    }

    /// One granted window of the string automaton. An escaped string's staged arena rides DETACHED in `staged` (see
    /// [`Self::string_step`]): decoded chunks push straight into it, and every exit path — the two `Ok` exits and
    /// every error — returns to the caller, which restores the arena before acting on the exit.
    #[expect(
        clippy::too_many_lines,
        reason = "the escape automaton is clearer as one transition table"
    )]
    fn string_window(
        &mut self,
        source: ResolvedSource<'_>,
        state: &mut StringState,
        staged: &mut Option<String>,
        limit: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<StringWindowExit, CodecError> {
        if let Some(text) = staged.as_mut() {
            reserve_window(text, limit - state.cursor)?;
        }
        while state.cursor < limit {
            let byte = source.bytes()[state.cursor];
            match state.escape {
                EscapeState::Plain => {
                    // After a run that stopped at the closer (or a string whose next remaining byte IS the closer) a
                    // 16-byte scan would only rediscover that stop. Skip it.
                    let run_end = if byte == state.quote {
                        state.cursor
                    } else if state.quote == b'\'' {
                        single_quoted_run_end(source.bytes(), state.cursor, limit)
                    } else {
                        plain_string_run_end(source.bytes(), state.cursor, limit)
                    };
                    if run_end != state.cursor {
                        if state.had_escape {
                            // Post-escape content joins the decoded stream in the detached arena directly. Pre-escape
                            // content is pushed in ONE call at the first `\`, and a no-escape string never stages
                            // chunks at all — the close appends the source slice directly.
                            // SAFETY: the run is ASCII by the scan.
                            let run = unsafe { core::str::from_utf8_unchecked(&source.bytes()[state.cursor..run_end]) };
                            push_chunk(staged.as_mut().ok_or_else(data_contract)?, run);
                        }
                        state.cursor = run_end;
                        continue;
                    }
                    match byte {
                        b'"' | b'\'' if byte == state.quote => {
                            state.cursor += 1;
                            self.set_offset(state.cursor);
                            return Ok(StringWindowExit::Closed);
                        }
                        b'\\' => {
                            if state.text.is_none() {
                                let decoded = self.builder()?.begin_text(resources).map_err(map_data)?;
                                state.text = Some(decoded);
                            }
                            if staged.is_none() {
                                // The first escape of the window detaches the staged arena; see `string_step`. The
                                // window reserve bound starts at the STRING start: the pre-escape prefix
                                // (start..cursor) is pushed below, before the window's remainder.
                                let text_stage = state.text.as_ref().ok_or_else(data_contract)?;
                                let text = self.builder()?.take_staged_text(text_stage).map_err(map_data)?;
                                reserve_window(staged.insert(text), limit - state.start)?;
                            }
                            if !state.had_escape {
                                // The first escape: push the whole pre-escape content (start..cursor) in ONE call —
                                // nothing was staged before this point. The arena accumulates decoded content: this
                                // verbatim prefix, escapes pushed as `encode_utf8` output, and post-escape runs.
                                // SAFETY: the prefix is a concatenation of ASCII runs (proved by the plain-string scan)
                                // and blocks the block UTF-8 validator accepted.
                                let prefix = unsafe {
                                    core::str::from_utf8_unchecked(&source.bytes()[state.start..state.cursor])
                                };
                                push_chunk(staged.as_mut().ok_or_else(data_contract)?, prefix);
                                state.had_escape = true;
                            }
                            // The escape burst: alternate one-step escape decodes and plain-run copies in one tight
                            // loop — escape-dense strings are runs of exactly this alternation, and re-entering the
                            // outer automaton between events re-derives every invariant the burst already holds. Each
                            // decision is byte-for-byte the outer loop's own: an anomaly (fast-lane refusal) breaks to
                            // the automaton with the cursor ON the escape character, a control or non-ASCII byte breaks
                            // to the arms that own it, the window edge suspends, and the close quote closes.
                            let text = staged.as_mut().ok_or_else(data_contract)?;
                            let bytes = source.bytes();
                            loop {
                                state.cursor += 1;
                                if !self.fast_escape(bytes, state, text, limit)? {
                                    state.escape = EscapeState::Escape;
                                    break;
                                }
                                let run_end = if state.quote == b'\'' {
                                    single_quoted_run_end(bytes, state.cursor, limit)
                                } else {
                                    plain_string_run_end_short(bytes, state.cursor, limit)
                                };
                                if run_end != state.cursor {
                                    // SAFETY: the run is ASCII by the scan.
                                    let run = unsafe { core::str::from_utf8_unchecked(&bytes[state.cursor..run_end]) };
                                    push_chunk(text, run);
                                    state.cursor = run_end;
                                }
                                if state.cursor >= limit {
                                    return Ok(StringWindowExit::Suspended);
                                }
                                let next = bytes[state.cursor];
                                if next == state.quote {
                                    state.cursor += 1;
                                    self.set_offset(state.cursor);
                                    return Ok(StringWindowExit::Closed);
                                }
                                if next != b'\\' {
                                    break;
                                }
                            }
                        }
                        0x00..=0x1f => {
                            return Err(invalid(
                                source,
                                state.cursor,
                                "control-in-string",
                                "unescaped control byte in string",
                            ));
                        }
                        0x7f => {
                            // DEL is accepted unescaped but is never its own render. The string-run stop set already
                            // halted here, so the canonicality probe is this one byte rather than a memchr over every
                            // run. COUPLING: this clear is gated on the probe while every sibling disqualifier clears
                            // unconditionally — sound only because the echo lane is `source_canonical`'s sole reader;
                            // if a second consumer appears, ungate it.
                            if self.canonicality_probe {
                                self.source_canonical = false;
                            }
                            if let Some(text) = staged.as_mut() {
                                push_char(text, '\u{7f}');
                            }
                            state.cursor += 1;
                        }
                        _ => {
                            // Block UTF-8 validation of string content: scan to the next structural byte (`"`, `\`, or
                            // a C0 control — a byte that cannot begin or continue a scalar) and validate the whole
                            // block with one SIMD call instead of one `from_utf8` per scalar. A structural byte
                            // terminates any sequence before it, so a block that stops BEFORE the last byte the decoder
                            // was given is final: a truncated tail there is an error at its lead. A block that runs to
                            // that last byte is not final — the decoder cannot tell "the input ended" from "this
                            // window's bytes ran out", and a sequence the cut truncated is legal input whose remaining
                            // bytes have not arrived. It is reported at the cut, not at the lead, so the streaming
                            // drive recognizes "the value would complete with more input" and holds; at true end of
                            // input it is the same refusal, labeled where the input stopped.
                            let content_end = string_content_run_end(source.bytes(), state.cursor, limit);
                            // A scoped pre-scan (or a `&str` materializer handoff) already proved these bytes. Do not
                            // skip the check over a source that was not. A work-grant cut mid-string is not end of
                            // input: persist carry and advance so the next grant continues. Leftover carry at true EOF
                            // is an invalid sequence.
                            if !self.utf8_proved {
                                // A structural stop (`"` / `\` / C0) is its own window: a leftover carry from an
                                // earlier escape-separated run must not poison this one. Work-grant cuts persist
                                // `state.utf8_carry`; this arm is the structural stop.
                                let mut carry = crate::byte_scan::Utf8Carry::EMPTY;
                                if let Some(position) = crate::byte_scan::utf8_first_invalid(
                                    &source.bytes()[state.cursor..content_end],
                                    state.cursor,
                                    &mut carry,
                                    content_end < source.bytes().len(),
                                ) {
                                    return Err(invalid(source, position, diag::INVALID_UTF8, diag::MSG_INVALID_UTF8));
                                }
                                if !carry.is_empty() {
                                    return Err(invalid(
                                        source,
                                        source.bytes().len(),
                                        diag::INVALID_UTF8,
                                        diag::MSG_INVALID_UTF8,
                                    ));
                                }
                            }
                            if state.had_escape {
                                // SAFETY: the block UTF-8 validator accepted exactly this block above.
                                let block = unsafe {
                                    core::str::from_utf8_unchecked(&source.bytes()[state.cursor..content_end])
                                };
                                push_chunk(staged.as_mut().ok_or_else(data_contract)?, block);
                            }
                            state.cursor = content_end;
                        }
                    }
                }
                EscapeState::Escape => {
                    // This arm is the REPLAY path: the `\\` arm's fast lane decodes every fully-granted valid sequence
                    // in one step, so this automaton sees only what that lane declined — JSON5 specials, invalid
                    // escapes, and sequences cut by the window edge — plus resumes from a saved mid-escape state.
                    state.cursor += 1;
                    // Every appending branch below shares the one detached arena: the escape arms are only reachable
                    // through the `\\` arm, which detached it.
                    let decoded_text = staged.as_mut().ok_or_else(data_contract)?;
                    match byte {
                        b'"' => push_char(decoded_text, '"'),
                        b'\\' => push_char(decoded_text, '\\'),
                        b'/' => {
                            // The escape disqualifier: the reference renders `\/` as `/`, so the escaped spelling is
                            // never its render.
                            self.source_canonical = false;
                            push_char(decoded_text, '/');
                        }
                        b'b' => push_char(decoded_text, '\u{0008}'),
                        b'f' => push_char(decoded_text, '\u{000c}'),
                        b'n' => push_char(decoded_text, '\n'),
                        b'r' => push_char(decoded_text, '\r'),
                        b't' => push_char(decoded_text, '\t'),
                        b'\'' if self.grammar.json5 => {
                            // JSON5 allows the `\'` escape in BOTH quote kinds.
                            push_char(decoded_text, '\'');
                        }
                        b'x' if self.grammar.json5 => {
                            // The JSON5 `\xHH` escape: two hex digits, one byte.
                            state.escape = EscapeState::Hex { value: 0, digits: 0 };
                            continue;
                        }
                        b'0' if self.grammar.json5 => {
                            // The JSON5 `\0` NUL escape is valid only when not followed by a decimal digit (`\01` is
                            // the octal spelling JSON5 forbids). `state.cursor` is already past the `0`.
                            if matches!(source.bytes().get(state.cursor), Some(b'0'..=b'9')) {
                                return Err(invalid(
                                    source,
                                    state.cursor - 1,
                                    diag::INVALID_ESCAPE,
                                    diag::MSG_INVALID_ESCAPE_JSON5,
                                ));
                            }
                            push_char(decoded_text, '\0');
                        }
                        b'\n' if self.grammar.json5 => {
                            // A JSON5 line continuation: a backslash before a line terminator contributes nothing to
                            // the string.
                        }
                        b'\r' if self.grammar.json5 => {
                            // `\<CR>` alone or `\<CR><LF>` continues the line; consume the optional `\n`.
                            if source.bytes().get(state.cursor) == Some(&b'\n') {
                                state.cursor += 1;
                            }
                        }
                        0xe2 if self.grammar.json5
                            && matches!(
                                source.bytes().get(state.cursor..state.cursor + 3),
                                Some([0x80, 0xa8 | 0xa9, _])
                            ) =>
                        {
                            // A backslash before U+2028/U+2029 is a JSON5 line continuation (ES5
                            // LineTerminatorSequence, which counts those separators as terminators). Consume all three
                            // bytes of the separator.
                            state.cursor += 2;
                        }
                        b'u' => {
                            // The escape disqualifier: the reference renders the \uXXXX escape as its UTF-8 character.
                            self.source_canonical = false;
                            // Four hex digits already in the window: finish the escape here instead of four more loop
                            // iterations. A short window keeps the resumable Unicode state.
                            match take_unicode_hex(source.bytes(), state.cursor, limit, 0, 0) {
                                Ok((value, 4, end)) => {
                                    state.cursor = end;
                                    state.escape =
                                        apply_unicode_scalar(staged.as_mut().ok_or_else(data_contract)?, value)?;
                                }
                                Ok((value, digits, end)) => {
                                    state.cursor = end;
                                    state.escape = EscapeState::Unicode { value, digits };
                                }
                                Err(at) => {
                                    return Err(invalid(
                                        source,
                                        at,
                                        "invalid-unicode-escape",
                                        "invalid Unicode escape",
                                    ));
                                }
                            }
                            continue;
                        }
                        _ => {
                            return Err(invalid(
                                source,
                                state.cursor - 1,
                                diag::INVALID_ESCAPE,
                                diag::MSG_INVALID_ESCAPE,
                            ));
                        }
                    }
                    state.escape = EscapeState::Plain;
                }
                EscapeState::Unicode { value, digits } => {
                    match take_unicode_hex(source.bytes(), state.cursor, limit, value, digits) {
                        Ok((value, 4, end)) => {
                            state.cursor = end;
                            state.escape = apply_unicode_scalar(staged.as_mut().ok_or_else(data_contract)?, value)?;
                        }
                        Ok((value, digits, end)) => {
                            state.cursor = end;
                            state.escape = EscapeState::Unicode { value, digits };
                        }
                        Err(at) => {
                            return Err(invalid(source, at, "invalid-unicode-escape", "invalid Unicode escape"));
                        }
                    }
                }
                EscapeState::LowBackslash { high } => {
                    state.cursor += 1;
                    if byte != b'\\' {
                        return Err(invalid(
                            source,
                            state.cursor - 1,
                            "missing-low-surrogate",
                            "high surrogate must be followed by a low surrogate",
                        ));
                    }
                    state.escape = EscapeState::LowU { high };
                }
                EscapeState::LowU { high } => {
                    state.cursor += 1;
                    if byte != b'u' {
                        return Err(invalid(
                            source,
                            state.cursor - 1,
                            "missing-low-surrogate",
                            "high surrogate must be followed by a low surrogate",
                        ));
                    }
                    state.escape = EscapeState::LowUnicode {
                        high,
                        value: 0,
                        digits: 0,
                    };
                }
                EscapeState::LowUnicode { high, value, digits } => {
                    match take_unicode_hex(source.bytes(), state.cursor, limit, value, digits) {
                        Ok((value, 4, end)) => {
                            state.cursor = end;
                            if !(0xdc00..=0xdfff).contains(&value) {
                                return Err(invalid(
                                    source,
                                    end - 4,
                                    "invalid-surrogate-pair",
                                    "invalid Unicode surrogate pair",
                                ));
                            }
                            let scalar = 0x1_0000 + ((u32::from(high) - 0xd800) << 10) + (u32::from(value) - 0xdc00);
                            push_char(
                                staged.as_mut().ok_or_else(data_contract)?,
                                char::from_u32(scalar).ok_or_else(data_contract)?,
                            );
                            state.escape = EscapeState::Plain;
                        }
                        Ok((value, digits, end)) => {
                            state.cursor = end;
                            state.escape = EscapeState::LowUnicode { high, value, digits };
                        }
                        Err(at) => {
                            return Err(invalid(source, at, "invalid-unicode-escape", "invalid Unicode escape"));
                        }
                    }
                }
                EscapeState::Hex { mut value, mut digits } => {
                    state.cursor += 1;
                    let digit = hex(byte).ok_or_else(|| {
                        invalid(
                            source,
                            state.cursor - 1,
                            diag::INVALID_ESCAPE,
                            "invalid JSON5 \\x escape",
                        )
                    })?;
                    value = (value << 4) | digit;
                    digits += 1;
                    if digits == 2 {
                        push_char(staged.as_mut().ok_or_else(data_contract)?, char::from(value));
                        state.escape = EscapeState::Plain;
                    } else {
                        state.escape = EscapeState::Hex { value, digits };
                    }
                }
            }
        }
        Ok(StringWindowExit::Suspended)
    }

    fn number_step(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        // The number automaton's state is hoisted OUT of `self.phase` into a local for the whole byte run, the same
        // shape the scoped validator's number loop uses: the phase variant is a ~100-byte struct, and a loop that
        // read/wrote the state through that path made LLVM spill narrow fields (the one-byte `lex` discriminator, the
        // bools) and reload them wide at the loop top — a narrow-store→wide-load pair that does not store-forward
        // in the emulated container. The state is written back to the phase slot only at the run boundary (or handed
        // back through `finish_number`), so the loop-carried cursor and lexeme live in registers.
        let phase = mem::replace(&mut self.phase, ParsePhase::Value);
        let ParsePhase::Number(mut state) = phase else {
            unreachable!()
        };
        let remaining = source.bytes().len().saturating_sub(state.cursor_usize());
        let Some(granted) = admit_bytes(resources, remaining)? else {
            self.phase = ParsePhase::Number(state);
            return Ok(false);
        };
        let limit = state.cursor_usize() + granted;
        // Decode leniency: read once for the whole run; the shared matcher accepts the reference implementation's
        // lenient number spellings under it.
        let lenient = self.grammar.lenient;
        while state.cursor_usize() < limit {
            // Long-number digit-run skip: the `Integer`/`Fraction` lexemes consume runs of `0-9` with digit bookkeeping
            // only, so the run is folded into the state in bulk and the state machine is re-entered at the first
            // non-digit. A megabyte literal costs one run scan plus one dispatch instead of a dispatch per digit.
            // `Zero` is deliberately NOT skipped — a digit there is the leading-zero error, whose position must not
            // move.
            if matches!(state.lex, NumberLex::Integer | NumberLex::Fraction) {
                let start = state.cursor_usize();
                let fractional = matches!(state.lex, NumberLex::Fraction);
                consume_digit_run(&mut state, &source.bytes()[start..limit], fractional)?;
                if state.cursor_usize() >= limit {
                    break;
                }
            }
            let byte = source.bytes()[state.cursor_usize()];
            // The exponent disqualifier: the reference renders `1e2` as `1E+2`, so an exponent number's source spelling
            // is never its render.
            if matches!(byte, b'e' | b'E') {
                self.source_canonical = false;
            }
            let consumed = match advance_number(&mut state, byte, lenient, self.grammar.json5) {
                Ok(consumed) => consumed,
                Err(error) if error.kind() == CodecFailureKind::InvalidInput => {
                    // The one number shape with a dedicated message: a digit after a leading zero (`00`, `01`, `007`)
                    // is illegal under RFC 8259, but the reference's lenient reader accepts it as `0`, `1`, `7`. Name
                    // the divergence so a reference-accepted file reads as a strictness difference, never as
                    // corruption.
                    if matches!(state.lex, NumberLex::Zero) && byte.is_ascii_digit() {
                        return Err(invalid(
                            source,
                            state.cursor_usize(),
                            diag::INVALID_NUMBER,
                            "a JSON number cannot have leading zeros (the reference accepts them leniently; \
                             RFC 8259 forbids them)",
                        ));
                    }
                    return Err(invalid(
                        source,
                        state.cursor_usize(),
                        diag::INVALID_NUMBER,
                        diag::MSG_INVALID_NUMBER,
                    ));
                }
                Err(error) => return Err(error),
            };
            if !consumed {
                if !is_value_boundary(byte) {
                    return Err(invalid(
                        source,
                        state.cursor_usize(),
                        diag::INVALID_NUMBER,
                        diag::MSG_INVALID_NUMBER,
                    ));
                }
                self.phase = ParsePhase::Number(state);
                return self.finish_number(source, resources);
            }
            state.cursor = state
                .cursor
                .checked_add(1)
                .ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?;
        }
        if state.cursor_usize() == source.bytes().len() {
            // No delimiter ended this number — the bytes did. The literal is complete as it stands and is published
            // as it stands, but a digit, a `.`, or an exponent arriving next would have made it a DIFFERENT number, so
            // the value is open-ended.
            self.open_ended = true;
            self.phase = ParsePhase::Number(state);
            return self.finish_number(source, resources);
        }
        self.phase = ParsePhase::Number(state);
        Ok(true)
    }

    fn finish_number(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        let phase = mem::replace(&mut self.phase, ParsePhase::Value);
        let ParsePhase::Number(state) = phase else {
            unreachable!()
        };
        let lenient = self.grammar.lenient;
        if !number_lex_complete(state.lex, lenient, self.grammar.json5, state.digit_count as usize) {
            return Err(invalid(
                source,
                state.cursor_usize(),
                diag::INVALID_NUMBER,
                "incomplete JSON number",
            ));
        }
        // A JSON5 hex integer bypasses the decimal render machinery entirely: the hex digits convert to exact canonical
        // decimal text and store as an `Integer` — no rounding step exists.
        if matches!(state.lex, NumberLex::Hex) {
            return self.finish_hex_number(source, &state, resources);
        }
        let Some((mut copy_start, mut copy_end, scale)) = number_finish_plan(&state, lenient)? else {
            // The exact-decimal scale does not fit i64: a decode refusal at every dial position but `lenient`.
            if !lenient {
                return Err(unsupported_number(source, state.start_usize(), state.cursor_usize()));
            }
            // Under `--strictness lenient` the literal clamps exactly where the reference implementation clamps, pinned
            // by the compat corpus's lenient rows: a NONZERO mantissa with a huge positive exponent becomes the widest
            // finite binary64, and every other out-of-scale shape is a zero at one of the reference's exponent caps
            // (`0E-1147483646` for a huge negative exponent, `0E+999999999` for a huge positive one on a zero
            // mantissa), sign preserved.
            return self.finish_number_clamped(source, state, resources);
        };
        // The S4 number canonicality: the authored spelling must BE the codec's render. Integers are verbatim by
        // construction (JSON's no-leading-zero law keeps their digits canonical); decimals render from the coefficient
        // and scale and switch to the scientific form outside the fixed range, so `0.0000001` renders `1E-7` and an
        // authored spelling that is not its own render clears the flag. Exponent spellings are already cleared by the
        // exponent hook (conservatively — `1E-7` IS its own render, but the hook fires on the lexeme). A
        // leniently-spelled number (`01`, `+1`, `1.`, `00.5`) is never its own render either, so the flag clears for it
        // too.
        //
        // The scan is gated on the echo lane's probe flag exactly like the duplicate-key fingerprint probe: the
        // `source_canonical` verdict is consumed only by the canonical-identity roundtrip gate, which is the one
        // session that arms the probe, so every other decode pays nothing for a verdict it cannot consume — the flag
        // simply stays true and is never read.
        if self.source_canonical && self.canonicality_probe && state.has_fraction_or_exponent {
            let lenient_completed =
                lenient && (state.lenient_spelling || matches!(state.lex, NumberLex::FractionStart));
            self.source_canonical = !lenient_completed && decimal_source_is_canonical(source.bytes(), &state);
        }
        // A span-bearing document publishes through an owned `Value`, which does not keep every spelling this document
        // can. The test is on EVERY number while a frontier is armed, not only on the ones inside a deferred subtree,
        // because the wholesale materialization at publication loses the spelling document-wide.
        if self.lazy_frontier.is_some() && !number_survives_owned_round_trip(&state) {
            return self.decline_frontier(source, resources);
        }
        // An omitted member's number commits nothing. Syntax and support were already checked above, so error behavior
        // is identical, and skipping the normalize/build path entirely is the point of the prune.
        if self.omitting_value(source) {
            self.set_offset(state.cursor_usize());
            self.attach(source, None, resources)?;
            if self.frames.is_empty() {
                self.phase = ParsePhase::Trailing;
            }
            return Ok(true);
        }
        // A number whose canonical spelling is already sitting in the input is committed as a span of it, never
        // rebuilt: no arena staging, no copy state machine, no ledger round trip for bytes the document does not own.
        // This is the same trade the unescaped-string path makes, and it is gated the same way — only copy mode,
        // whose product must outlive the buffer, has to rebuild the spelling it could have named.
        if self.mode.retains_source_spans() && number_is_verbatim_source(&state) {
            let span = jqf_source::Span::try_new(state.start, state.cursor)
                .ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?;
            self.set_offset(state.cursor_usize());
            return self.finish_source_integer(source, span, resources);
        }
        // Decimal numbers render from a trailing-zero-preserving coefficient and the decimal scale so the encoder
        // reproduces the reference byte for byte; the canonical range above still owns accept/reject. Integers (`scale`
        // `None`) keep their verbatim spelling.
        let scale = if scale.is_some() {
            match number_render_plan(&state)? {
                Some((render_start, render_end, render_scale)) => {
                    copy_start = render_start;
                    copy_end = render_end;
                    Some(render_scale)
                }
                None => scale,
            }
        } else {
            scale
        };
        let canonical = self.builder()?.begin_text(resources).map_err(map_data)?;
        let prefix = if copy_end == 0 {
            // An all-zero magnitude: `0`, `-0`, `0.0`, `-0e5`,... Preserve the sign in the rendered canonical text so
            // `-0` round-trips as `-0` (the signed-zero render law do). The semantic layer still collapses it to zero:
            // `Integer::parse` normalizes `-0` to `0` for the materialized Value and the semantic checksum, so only the
            // byte rendering carries the sign.
            if state.negative { Some("-0") } else { Some("0") }
        } else if state.negative {
            Some("-")
        } else {
            None
        };
        self.phase = ParsePhase::NumberNormalize(NumberNormalizeState {
            source: state,
            canonical,
            copy_end,
            copy_cursor: copy_start,
            scale,
            prefix,
        });
        Ok(true)
    }

    /// The lenient C2 clamp, entered from [`Self::finish_number`] when the exact-decimal scale does not fit i64 under
    /// `--strictness lenient`.
    ///
    /// The bytes are the reference implementation's, pinned by the compat corpus's lenient rows: a nonzero mantissa
    /// with a huge positive exponent is stored as a ±INFINITY Float — deliberately NOT the widest finite value —
    /// and the bytes come out right only because the render path prints a non-finite value as the clamped widest
    /// binary64 (`1e999999999999999999999` renders `1.7976931348623157e+308`, sign preserved); every other out-of-scale
    /// shape is a zero at one of the reference's exponent caps — a huge negative exponent renders `0E-1147483646`
    /// (`-0E-1147483646` signed), a huge positive exponent on a zero mantissa `0E+999999999`. The zero shapes are
    /// stored as exact decimals at the capped scale, which renders the same text. A clamped spelling is never its own
    /// render, so the source-canonical flag clears (the echo lane's probe or not — the clamp is a disqualifier like
    /// whitespace).
    fn finish_number_clamped(
        &mut self,
        source: ResolvedSource<'_>,
        state: NumberState,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        self.source_canonical = false;
        if state.first_nonzero.is_some() && !state.exponent_negative {
            // The DBL_MAX clamp: the Float node stores ±INFINITY, and the RENDER path clamps that to the widest finite
            // binary64 — exactly how the accepted `inf`/`Infinity` spellings render (the memo's render law already
            // exists).
            let node = if self.deferring() || self.omitting_value(source) {
                None
            } else {
                let (builder, schema) = self.prepared_builder()?;
                let kind = schema.node_kind(2).ok_or_else(data_contract)?;
                let semantic = PreparedSemanticNode::Float(jqf_data::Float::new(if state.negative {
                    f64::NEG_INFINITY
                } else {
                    f64::INFINITY
                }));
                Some(
                    builder
                        .add_prepared_node(schema, kind, semantic, resources)
                        .map_err(map_data)?,
                )
            };
            self.attach(source, node, resources)?;
            self.set_offset(state.cursor_usize());
            if self.frames.is_empty() {
                self.phase = ParsePhase::Trailing;
            }
            return Ok(true);
        }
        // The zero shapes: an exact decimal zero at the reference's exponent cap, sign preserved. The empty copy range
        // and the `0`/`-0` prefix reuse the all-zero-magnitude normalize path verbatim.
        let scale = if state.exponent_negative {
            LENIENT_UNDERFLOW_SCALE
        } else {
            LENIENT_OVERFLOW_ZERO_SCALE
        };
        if self.omitting_value(source) {
            self.set_offset(state.cursor_usize());
            self.attach(source, None, resources)?;
            if self.frames.is_empty() {
                self.phase = ParsePhase::Trailing;
            }
            return Ok(true);
        }
        let canonical = self.builder()?.begin_text(resources).map_err(map_data)?;
        let prefix = if state.negative { Some("-0") } else { Some("0") };
        self.phase = ParsePhase::NumberNormalize(NumberNormalizeState {
            source: state,
            canonical,
            copy_end: 0,
            copy_cursor: 0,
            scale: Some(scale),
            prefix,
        });
        Ok(true)
    }

    /// Materializes a JSON5 hex integer: the hex digits after `0x` convert to exact canonical decimal text and store
    /// through the ordinary `Integer` node — a hex spelling is exact, so there is no rounding step to specify. The
    /// authored `0x…` token is never its own render (the encoder writes decimal), so the source is non-canonical.
    fn finish_hex_number(
        &mut self,
        source: ResolvedSource<'_>,
        state: &NumberState,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        self.source_canonical = false;
        // The digit-count cap fires on EVERY path (deferred included): a spelling beyond the exact-conversion bound is
        // refused identically whichever route decodes it, and the conversion itself is quadratic in digit count, so an
        // unbounded run would turn one decode into seconds of CPU.
        let digits_len = state.cursor_usize() - state.start_usize() - 2 - usize::from(state.negative);
        if digits_len > MAX_HEX_DIGITS {
            return Err(crate::error::unsupported_hex_number(
                source,
                state.start_usize() + 2 + usize::from(state.negative),
                state.cursor_usize(),
            ));
        }
        let node = if self.deferring() || self.omitting_value(source) {
            None
        } else {
            // `state.start` points at the leading `0` (or the `-` for a negative hex spelling, or the digit after a
            // `+`), so the hex digits begin two bytes in, plus the sign byte. The digit-run bounds are an automaton
            // invariant, not a proof this site may index on: a broken invariant must fail the request here, not panic
            // (the seq stream's payload-range guard is the pattern).
            let digits = source
                .bytes()
                .get(state.start_usize() + 2 + usize::from(state.negative)..state.cursor_usize())
                .ok_or_else(data_contract)?;
            let decimal = hex_to_decimal(digits);
            let (builder, schema) = self.prepared_builder()?;
            let kind = schema.node_kind(2).ok_or_else(data_contract)?;
            let mut text = builder.begin_text(resources).map_err(map_data)?;
            if state.negative {
                builder.append_text(&mut text, "-", resources).map_err(map_data)?;
            }
            builder.append_text(&mut text, &decimal, resources).map_err(map_data)?;
            let canonical = builder.finish_text(text, resources).map_err(map_data)?;
            Some(
                builder
                    .add_prepared_stored_integer_node(schema, kind, canonical, resources)
                    .map_err(map_data)?,
            )
        };
        // The hex token is not a verbatim number; record its authored span out-of-band so the edit lane can patch a
        // CHANGED leaf, exactly as the lenient spellings' normalize path does.
        if let Some(node) = node {
            let span = jqf_source::Span::try_new(state.start, state.cursor)
                .ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?;
            self.record_leaf_span(node, span, resources)?;
        }
        self.attach(source, node, resources)?;
        self.set_offset(state.cursor_usize());
        if self.frames.is_empty() {
            self.phase = ParsePhase::Trailing;
        }
        Ok(true)
    }

    fn number_normalize_step(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        let phase = mem::replace(&mut self.phase, ParsePhase::Value);
        let ParsePhase::NumberNormalize(mut state) = phase else {
            unreachable!()
        };
        if let Some(prefix) = state.prefix {
            if resources.admit_work_transition()? == WorkAdmission::Pending {
                self.phase = ParsePhase::NumberNormalize(state);
                return Ok(false);
            }
            self.builder()?
                .append_text(&mut state.canonical, prefix, resources)
                .map_err(map_data)?;
            state.prefix = None;
        }
        if state.copy_cursor < state.copy_end {
            let remaining = state.copy_end - state.copy_cursor;
            let Some(granted) = admit_bytes(resources, remaining)? else {
                self.phase = ParsePhase::NumberNormalize(state);
                return Ok(false);
            };
            let end = state.copy_cursor + granted;
            let bytes = &source.bytes()[state.copy_cursor..end];
            // A completed number's copy range runs from the first non-zero digit to the last digit — digits with AT
            // MOST ONE interior `.` (a decimal's fraction point; an integer has none). A range short enough to fit the
            // stack has its coefficient built once (every `.` dropped, the exact text the run loop below appends in
            // pieces) and appended in ONE `append_text` call; long literals fall back to the piecewise loop. The chunk
            // boundary is preserved either way: a partial chunk appends its digits and `copy_cursor` lands at `end`,
            // exactly as the piecewise arm does.
            if bytes.len() <= NUMBER_COEFF_STACK_CAP {
                let mut coeff = [0u8; NUMBER_COEFF_STACK_CAP];
                let mut n = 0;
                for &byte in bytes {
                    if byte != b'.' {
                        coeff[n] = byte;
                        n += 1;
                    }
                }
                let text = core::str::from_utf8(&coeff[..n]).map_err(|_| data_contract())?;
                self.builder()?
                    .append_text(&mut state.canonical, text, resources)
                    .map_err(map_data)?;
                state.copy_cursor = end;
                self.phase = ParsePhase::NumberNormalize(state);
                return Ok(true);
            }
            let mut cursor = 0;
            while cursor < bytes.len() {
                while cursor < bytes.len() && bytes[cursor] == b'.' {
                    cursor += 1;
                }
                let start = cursor;
                while cursor < bytes.len() && bytes[cursor] != b'.' {
                    cursor += 1;
                }
                if start != cursor {
                    let text = core::str::from_utf8(&bytes[start..cursor]).map_err(|_| data_contract())?;
                    self.builder()?
                        .append_text(&mut state.canonical, text, resources)
                        .map_err(map_data)?;
                }
            }
            state.copy_cursor = end;
            self.phase = ParsePhase::NumberNormalize(state);
            return Ok(true);
        }
        let cursor = state.source.cursor_usize();
        let canonical = self
            .builder()?
            .finish_text(state.canonical, resources)
            .map_err(map_data)?;
        let node = if self.deferring() {
            None
        } else if let Some(scale) = state.scale {
            let (builder, schema) = self.prepared_builder()?;
            let kind = schema.node_kind(2).ok_or_else(data_contract)?;
            Some(
                builder
                    .add_prepared_stored_decimal_node(schema, kind, canonical, scale, resources)
                    .map_err(map_data)?,
            )
        } else {
            let (builder, schema) = self.prepared_builder()?;
            let kind = schema.node_kind(2).ok_or_else(data_contract)?;
            Some(
                builder
                    .add_prepared_stored_integer_node(schema, kind, canonical, resources)
                    .map_err(map_data)?,
            )
        };
        // A non-verbatim number (fraction/exponent/lenient spelling) is stored from its canonical render; record the
        // authored token out-of-band so the edit lane can patch a CHANGED leaf instead of flooring. The lenient `+`
        // spelling's state starts AFTER the plus (the canonical text never carries it), so the span extends back to
        // name the COMPLETE authored token.
        if let Some(node) = node {
            let mut start = state.source.start;
            if start > 0 && source.bytes().get((start - 1) as usize) == Some(&b'+') {
                start -= 1;
            }
            let span = jqf_source::Span::try_new(start, crate::storage::offset_u32(cursor)?)
                .ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?;
            self.record_leaf_span(node, span, resources)?;
        }
        self.attach(source, node, resources)?;
        self.set_offset(cursor);
        if self.frames.is_empty() {
            self.phase = ParsePhase::Trailing;
        }
        Ok(true)
    }

    fn finish_string(
        &mut self,
        source: ResolvedSource<'_>,
        target: StringTarget,
        text: DocumentTextId,
        authored: Option<jqf_source::Span>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        match target {
            StringTarget::Key => {
                // SAFETY: this parser retains the exact builder until the key is consumed by the matching object
                // occurrence insertion.
                let span =
                    unsafe { self.builder()?.consume_bound_stored_text_span(text, resources) }.map_err(map_data)?;
                let frame = self.frames.as_mut_slice().last_mut().ok_or_else(data_contract)?;
                frame.state = FrameState::ObjectColon(ObjectKey::Stored(span));
            }
            StringTarget::Value => {
                let node = if self.deferring() || self.omitting_value(source) {
                    None
                } else {
                    let (builder, schema) = self.prepared_builder()?;
                    let kind = schema.node_kind(3).ok_or_else(data_contract)?;
                    Some(
                        builder
                            .add_prepared_stored_string_node(schema, kind, text, resources)
                            .map_err(map_data)?,
                    )
                };
                // An escaped string is stored from its DECODED text, so the bound-source path is unavailable; record
                // the authored token out-of-band so the edit lane can patch a CHANGED escaped leaf instead of flooring.
                // The span is the INNER content between the quotes, the same convention the bound-source string path
                // records, so the patch machinery extends it across the quote pair.
                if let (Some(node), Some(span)) = (node, authored) {
                    self.record_leaf_span(node, span, resources)?;
                }
                self.attach(source, node, resources)?;
                if self.frames.is_empty() {
                    self.phase = ParsePhase::Trailing;
                }
            }
        }
        Ok(true)
    }

    fn finish_source_string(
        &mut self,
        source: ResolvedSource<'_>,
        target: StringTarget,
        span: jqf_source::Span,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        self.source_spans_committed = true;
        match target {
            StringTarget::Key => {
                let frame = self.frames.as_mut_slice().last_mut().ok_or_else(data_contract)?;
                frame.state = FrameState::ObjectColon(ObjectKey::Source(span));
            }
            StringTarget::Value => {
                let node = if self.deferring() || self.omitting_value(source) {
                    None
                } else {
                    let (builder, schema) = self.prepared_builder()?;
                    let kind = schema.node_kind(3).ok_or_else(data_contract)?;
                    // SAFETY: the complete capacity scan validated UTF-8, the parser produced this in-bounds string
                    // span, and codec-core owns the same immutable authority through publication.
                    Some(
                        unsafe { builder.add_prepared_bound_source_string_node(schema, kind, span, resources) }
                            .map_err(map_data)?,
                    )
                };
                self.attach(source, node, resources)?;
                if self.frames.is_empty() {
                    self.phase = ParsePhase::Trailing;
                }
            }
        }
        Ok(true)
    }

    /// Commits an integer as a span of the sealed source, the number twin of [`Self::finish_source_string`]'s value
    /// arm.
    fn finish_source_integer(
        &mut self,
        source: ResolvedSource<'_>,
        span: jqf_source::Span,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        self.source_spans_committed = true;
        let node = if self.deferring() {
            None
        } else {
            let (builder, schema) = self.prepared_builder()?;
            let kind = schema.node_kind(2).ok_or_else(data_contract)?;
            // SAFETY: `number_is_verbatim_source` proved these exact bytes are the canonical integer spelling, the
            // lexer produced this in-bounds span, and codec-core owns the same immutable authority through publication.
            Some(
                unsafe { builder.add_prepared_bound_source_integer_node(schema, kind, span, resources) }
                    .map_err(map_data)?,
            )
        };
        self.attach(source, node, resources)?;
        if self.frames.is_empty() {
            self.phase = ParsePhase::Trailing;
        }
        Ok(true)
    }

    /// Records a container's ONE-BYTE anchor span: its opening delimiter. A container's extent comes from its close
    /// scan, so only the anchor is recorded here.
    fn record_container_anchor(
        &mut self,
        node: NodeId,
        open: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        let span = jqf_source::Span::try_new(
            u32::try_from(open).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?,
            u32::try_from(open.saturating_add(1)).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?,
        )
        .ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?;
        self.record_leaf_span(node, span, resources)
    }

    /// Records one authored source span on the builder for a scalar whose retained semantic carries no span of its own
    /// (a bool, a null, a non-verbatim number, a non-finite spelling, an escaped string), so the edit lane can address
    /// the authored bytes for verbatim echo and patching. The semantic is stored by the node's own add; the span is an
    /// addressing channel, never a second value — the same out-of-band record TOML's floats and YAML's plain scalars
    /// commit.
    ///
    /// Gated on span retention like the bound-source paths: an `OwnedRun` document owns no source to name, and
    /// recording would force a seal over a source that cannot exist.
    fn record_leaf_span(
        &mut self,
        node: NodeId,
        span: jqf_source::Span,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        // Also gated on the requirement's authored-span demand: the table's only consumers are the edit, splice, and
        // lossless-encode lanes, and a drive that proved none of them can run withdraws the demand — a filter decode
        // then skips the whole table.
        if !self.mode.retains_source_spans() || !self.authored_spans {
            return Ok(());
        }
        // An out-of-band authored span is an admitted source span like any other: the seal binding is gated on
        // `source_spans_committed`, so recording one must set the flag or a document whose ONLY spans are out-of-band
        // (a bool-only JSON) would finalize without a seal covering them.
        self.source_spans_committed = true;
        // SAFETY: the span was produced by this codec's own token walk over the exact source authority bound to the
        // builder, so it names UTF-8 that re-resolves to the node's stored semantic — the `record_authored_span`
        // contract. The unsafe law is satisfied the way TOML's floats satisfy it: the span names the node's authored
        // token, not a byte copy of its semantic.
        unsafe { self.builder()?.record_authored_span(node, span, resources) }.map_err(map_data)
    }

    /// The fixed positive quiet NaN bits shared with the YAML codec's `.nan` law: one NaN bit pattern across formats,
    /// so two non-finite spellings can never be distinct values.
    pub(crate) const NONFINITE_NAN_BITS: u64 = 0x7ff8_0000_0000_0000;

    /// Whether the parser is currently inside a deferred subtree, and so must validate without committing anything.
    const fn deferring(&self) -> bool {
        self.deferred.is_some()
    }

    /// Discards everything decoded so far and re-decodes the document EAGERLY.
    ///
    /// This is the frontier's decline path (a lazy route that cannot prove byte parity for a shape declines to eager
    /// for that shape), and the decline is necessarily WHOLE-DOCUMENT rather than per-subtree. The reason is the
    /// publication seam: a located value out of a document that holds any container span is materialized through an
    /// owned value, so a spelling an owned value cannot carry is lost *anywhere* in that document, not only inside the
    /// spans. Declining just the offending subtree would leave the earlier spans standing, the document still
    /// span-bearing, and the offending number still normalized — which is a divergence the standing eager-versus-lazy
    /// differential catches (`tools/jqf-lazy-differential.py`).
    ///
    /// Restarting is also the only shape that needs no reasoning about partial state: [`Self::reset`] drops the
    /// builder, so the re-decode begins from an empty document at byte zero with the frontier off, and no arena has to
    /// be un-committed. It happens at most ONCE per session, because it is reached only while a frontier is armed and
    /// it disarms it.
    ///
    /// The cost is one extra pass over the prefix already scanned, charged again. Charging twice is conservative —
    /// never an under-charge — and it is the price of the decline.
    fn decline_frontier(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        let (diagnostics, coverage, mode) = (self.diagnostics, self.coverage, self.mode);
        self.reset(diagnostics, coverage, mode);
        self.lazy_frontier = None;
        jqf_codec_core::record_declined_deferral();
        self.initialize(source.bytes().len(), resources)?;
        Ok(true)
    }

    /// Commits the one node a completed deferred subtree publishes: a span of the validated source it occupies, charged
    /// exactly like any other node push (span bookkeeping itself is charged).
    fn commit_container_span(
        &mut self,
        source: ResolvedSource<'_>,
        deferred: &DeferredContainer,
        end: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        self.source_spans_committed = true;
        let span = jqf_source::Span::try_new(
            u32::try_from(deferred.open).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?,
            u32::try_from(end).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?,
        )
        .ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?;
        let container = deferred.container;
        let (builder, schema) = self.prepared_builder()?;
        let kind_slot = match container {
            jqf_data::ContainerSpanKind::Object => 5,
            jqf_data::ContainerSpanKind::Array => 4,
        };
        let kind = schema.node_kind(kind_slot).ok_or_else(data_contract)?;
        // SAFETY: the same validating scan that accepts the eager document just ran over `[open, end)` and proved it
        // one complete container text of this session's exact immutable source authority, which codec-core holds
        // unchanged through publication.
        let node = unsafe { builder.add_prepared_bound_container_span_node(schema, kind, span, container, resources) }
            .map_err(map_data)?;
        self.attach(source, Some(node), resources)
    }

    fn attach(
        &mut self,
        source: ResolvedSource<'_>,
        node: Option<NodeId>,
        resources: &ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        // The one occurrence law lives in `attach_hoisted`; this non-batched entry takes the top state out of the Vec
        // (current between batches), delegates, and writes it back.
        let mut top_state = self.frames.as_slice().last().map(|frame| frame.state);
        let mut dirty = false;
        let result = self.attach_hoisted(source, &mut top_state, &mut dirty, node, resources);
        if let Some(state) = top_state {
            self.frames.as_mut_slice().last_mut().ok_or_else(data_contract)?.state = state;
        }
        result
    }

    fn trailing_step(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        let offset = self.offset();
        if self.mode.ends_before_source() {
            // The root value is grammatically complete at `offset`; the caller opted into decoding this as one of
            // several adjacent values, so whatever follows (more whitespace, another value, or garbage) is the caller's
            // concern, not this session's. Record the exact consumed offset and finalize without inspecting the
            // remainder at all.
            self.consumed_offset
                .get_or_insert(u64::try_from(offset).unwrap_or(u64::MAX));
            self.sealed_len.get_or_insert(offset);
            return self.begin_finalize(resources);
        }
        let lead = source.bytes().get(offset).copied();
        if lead.is_some_and(crate::byte_scan::is_json_ws)
            || (self.grammar.comments && lead == Some(b'/'))
            || (self.grammar.json5 && matches!(lead, Some(b'\xe2' | b'\xef')))
        {
            match self.trivia_advance(source, offset, resources)? {
                TriviaAdvance::Skipped(consumed) => {
                    self.set_offset(offset + consumed);
                    return Ok(true);
                }
                // Not trivia after all: the trailing-content error below owns the byte.
                TriviaAdvance::Content => {}
                TriviaAdvance::Yield => return Ok(false),
                TriviaAdvance::Cut => {
                    // A comment marker or JSON5 separator cut by the end of the input is an incomplete candidate, not
                    // garbage: label at the end so the streaming drive holds and refills, and a whole-input decode
                    // reports there too.
                    return Err(invalid(
                        source,
                        source.bytes().len(),
                        diag::TRAILING_CONTENT,
                        diag::MSG_TRAILING_CONTENT,
                    ));
                }
            }
        }
        if offset != source.bytes().len() {
            return Err(invalid(
                source,
                offset,
                diag::TRAILING_CONTENT,
                diag::MSG_TRAILING_CONTENT,
            ));
        }
        // Whole-document decode consumes the source to its last byte, so the segment the document names is the whole of
        // it.
        self.sealed_len.get_or_insert(offset);
        self.begin_finalize(resources)
    }

    /// Attaches the pending comment run as one `jsonc.comment@1` fact on `owner` and clears the run — the TOML
    /// `attach_comment_fact` shape verbatim. `pending_comments` is empty unless the grammar armed comments AND the
    /// coverage demanded facts — the trivia scanner records under exactly that gate — so a comment-free or
    /// comment-undemanded decode never reaches the builder (the zero-cost-empty law).
    fn attach_pending_comments(&mut self, owner: NodeId, resources: &ResourceContext<'_>) -> Result<(), CodecError> {
        let comments = core::mem::take(&mut self.pending_comments);
        if comments.is_empty() {
            return Ok(());
        }
        let payload = FactPayload::List(comments.into_iter().map(FactPayload::Text).collect());
        let role = self.comment_fact_role;
        self.builder()?
            .add_fact(LocalOwnerRef::Node(owner), role, role, 1, &payload, resources)
            .map_err(map_data)?;
        Ok(())
    }

    /// Enters finalization, first sealing the segment the root value occupies when a zero-copy span named it. A
    /// document with neither an unescaped string nor a verbatim integer commits no span and is never sealed.
    fn begin_finalize(&mut self, resources: &mut ResourceContext<'_>) -> Result<bool, CodecError> {
        // The document-trailer comment run (a comment with no following value in scope) attaches to the ROOT node —
        // the TOML trailer law. The comment fact scopes a JSONC document to LEADING owners; a trailing comment is
        // invisible to programs and survives edits byte-wise either way.
        if !self.pending_comments.is_empty() {
            let root = self.root.ok_or_else(data_contract)?;
            self.attach_pending_comments(root, resources)?;
        }
        if self.source_spans_committed && self.source_binding.is_none() {
            self.phase = ParsePhase::SealSource;
            return Ok(true);
        }
        let root = self.root.ok_or_else(data_contract)?;
        self.schema.take().ok_or_else(data_contract)?;
        let mut finalizer = self
            .builder
            .take()
            .ok_or_else(data_contract)?
            .begin_finish(root, resources)
            .map_err(map_data)?;
        finalizer.install_transients(&mut self.transients);
        self.finalizer = Some(finalizer);
        self.phase = ParsePhase::Finalize;
        Ok(true)
    }

    fn finalize_step(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        let sealed = self
            .source_binding
            .is_some()
            .then(|| self.sealed_source(source))
            .transpose()?;
        let finalizer = self.finalizer.as_mut().ok_or_else(data_contract)?;
        let poll = if let Some(sealed) = sealed {
            // SAFETY: the codec-core access session owns and supplies the same immutable ResolvedSource authority for
            // every parser poll, and the sealed segment is the same narrowing of it the binding was taken over.
            unsafe { finalizer.poll_with_source(sealed, resources) }.map_err(map_data)?
        } else {
            finalizer.poll(resources).map_err(map_data)?
        };
        let DocumentFinalizationPoll::Ready(mut document) = poll else {
            return Ok(false);
        };
        document.set_source_canonical(self.source_canonical);
        // Take the scratch back before the finalizer drops: this is the one point where both its own vectors and the
        // builder scratch it parked during publication are reachable together.
        if let Some(finalizer) = self.finalizer.as_mut() {
            finalizer.reclaim_transients(&mut self.transients);
        }
        self.finalizer = None;
        self.product = Some(DocumentProduct::try_new(document, resources)?);
        self.phase = ParsePhase::Publish;
        Ok(true)
    }

    fn publish_step<'source>(
        &mut self,
        source: ResolvedSource<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<AccessResult<'source>, CodecError> {
        let product = self.product.take().ok_or_else(data_contract)?;
        let product = if self.source_binding.is_some() {
            // SAFETY: codec-core owns this exact immutable ResolvedSource for the whole access session. JSON
            // constructed the canonical seal over this narrowing of it, parsed every source ref against it, and
            // finalized with the same authority; metadata is still checked at attachment.
            let sealed = self.sealed_source(source)?;
            unsafe { product.attach_borrowed_source_from_access_session(sealed, resources) }?
        } else {
            product
        };
        // Dark-launch instrumentation, taken here rather than at each commit so a declined-and-re-decoded document does
        // not leave the abandoned attempt's spans in the count.
        jqf_codec_core::record_published_spans(u64::from(product.document().container_span_count()));
        let outcome = AccessOutcome::FullDocument(product);
        Ok(match self.consumed_offset {
            Some(consumed) => {
                let result = AccessResult::from_outcome_with_consumed_offset(outcome, consumed);
                if self.open_ended { result.open_ended() } else { result }
            }
            None => AccessResult::from_outcome(outcome),
        })
    }

    fn offset(&self) -> usize {
        match &self.phase {
            ParsePhase::String(state) => state.cursor,
            ParsePhase::Number(state) => state.cursor_usize(),
            ParsePhase::NumberNormalize(state) => state.source.cursor_usize(),
            _ => self.cursor,
        }
    }

    pub(crate) fn set_offset(&mut self, offset: usize) {
        self.cursor = offset;
    }

    /// Consumes a UTF-8 byte-order mark sitting at ABSOLUTE offset zero of `source`, before the first value (RFC 8259
    /// §8.1). `open_at` narrows every later value's source to its own first byte, so the base-zero test IS the
    /// first-value test: a mark after whitespace, or before a later value, stays a rejected `expected-value`
    /// diagnostic.
    ///
    /// The mark belongs to no value, so the echo lane must not republish it — the reference's identity output never
    /// carries one — and consuming it clears the canonicality bit for the same reason leading whitespace does.
    pub(crate) fn consume_source_bom(&mut self, source: ResolvedSource<'_>) {
        if source.base_offset() == 0 && source.bytes().starts_with(&BYTE_ORDER_MARK) {
            self.source_canonical = false;
            self.set_offset(BYTE_ORDER_MARK.len());
        }
    }
}

impl AccessSession for JsonParseState {
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
        self.parse_decode(source, context)
    }
}

/// One cooperative owned-run parse poll from [`JsonParseState::poll_owned`].
pub(crate) enum OwnedRunPoll {
    /// More cooperative work is required; resume with the same buffer source.
    Pending,
    /// The self-contained run document became visible exactly once.
    Ready(DocumentProduct<'static>),
}

/// How one `string_window` call ended: on the closing quote, or out of granted bytes (mid-string or mid-escape) with
/// the phase to be re-entered. Either way the outer `string_step` restores the detached staged-text arena before acting
/// on the exit. How one trivia-advance attempt ended: trivia consumed, content found, the meter exhausted, or an
/// incomplete candidate cut by the end of the source. See [`JsonParseState::trivia_advance`] for the zero-progress law.
enum TriviaAdvance {
    /// `n` trivia bytes consumed; resume at the previous cursor plus `n`.
    Skipped(usize),
    /// The byte is not trivia under the grammar; token dispatch owns it.
    Content,
    /// The cooperative meter is out of credits; yield the poll.
    Yield,
    /// A comment marker or JSON5 separator cut by the END of the source; the caller raises its hold-style error labeled
    /// at the source's end.
    Cut,
}

enum StringWindowExit {
    /// The closing quote was consumed; the cursor is past it.
    Closed,
    /// The window's grant ran out; suspend with the saved `StringState`.
    Suspended,
}

#[cfg(test)]
mod tests {
    use super::JsonParseState;
    use crate::storage::ParseMode;
    use crate::test_support;
    use alloc::vec;
    use alloc::vec::Vec;
    use jqf_codec_core::{AccessInput, AccessOutcome, AccessSession as _, CodecRunContext};
    use jqf_data::{BuilderCoverage, DiagnosticCoverage, NodeId};
    use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

    fn source(bytes: &[u8]) -> ResolvedSource<'_> {
        ResolvedSource::new(
            SourceRef::new(SourceId::new(91), SourceKind::Input),
            "test.json",
            bytes,
            0,
        )
    }

    /// Parses one document and collects every authored/source span it published, as the byte slice each span names.
    fn published_span_bytes(bytes: &[u8]) -> Vec<&[u8]> {
        let mut resources = test_support::resources();
        let mut state = JsonParseState::new(
            DiagnosticCoverage::NotRequested,
            BuilderCoverage::minimal_semantic(),
            ParseMode::Document,
        );
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        let result = state
            .decode(AccessInput::Source(source(bytes)), &mut run)
            .expect("parse");
        let AccessOutcome::FullDocument(product) = result.into_parts().0 else {
            panic!("expected a full document")
        };
        let document = product.document();
        (0..document.node_count())
            .filter_map(|index| {
                let node = NodeId::try_from_index(index).expect("node id");
                document.node_source_span(node).expect("span read").map(|span| {
                    bytes
                        .get(span.start() as usize..span.end() as usize)
                        .expect("span inside source")
                })
            })
            .collect()
    }

    /// The patch lane's symmetry law: every stored-not-spelled scalar — a bool, a null, and a non-finite spelling
    /// alike — publishes an authored span naming its exact token bytes. A non-finite spelling is stored as Float bits
    /// whose render is never the token, so without its record the edit lane would floor that leaf while patching its
    /// twins verbatim.
    #[test]
    fn nonfinite_scalars_record_authored_spans_like_the_other_stored_scalars() {
        let text = b"[true, null, Infinity, NaN]";
        let mut named = published_span_bytes(text);
        // Sorted both sides: node order is build order, not token order.
        named.sort_unstable();
        let mut expected: Vec<&[u8]> = vec![b"[", b"true", b"null", b"Infinity", b"NaN"];
        expected.sort_unstable();
        assert_eq!(named, expected);
    }
}
