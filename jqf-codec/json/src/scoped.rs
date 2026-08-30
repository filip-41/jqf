//! Scoped exact-path access: validate the whole input to the full parser's exact strictness, then materialize only the
//! selected exact-path subtree.
//!
//! Strict JSON, JSONC, and JSON5 share this session. The grammar field is `STRICT` plus the leniency dial for RFC 8259;
//! JSONC/JSON5 arm comments (and JSON5 the rest of its grammar) so the validate walk accepts the same bytes as
//! [`crate::parse::JsonParseState`]. Comment facts that sit outside the located value span — a member's leading
//! comment before its key — are collected during validate and seeded onto the materializer when
//! [`jqf_data::BuilderCoverage::attached_facts`] is on. Identity Exact still validates every unread byte; trailing
//! trivia is skipped, trailing values are not.
//!
//! The scoped route exists so an engine-authored `Located` requirement with an exact-path footprint stops paying
//! whole-document materialization. It runs in two cooperative phases:
//!
//! 1. **Validate** — a resumable structural scan that enforces the identical grammar, UTF-8, escape, number, depth
//!    and trailing rules as [`crate::parse::JsonParseState`], producing the identical [`CodecError`] (code, message,
//!    offset) for every invalid input. Nothing is materialized. The same walk records the exact-path observation
//!    (last-value-wins object semantics, signed array-index semantics) so a second navigate traversal is not required.
//!    Publishing still waits for validation `Done`.
//! 2. **Materialize** — a fresh [`JsonParseState`] re-parse of just the selected span into a demand-scoped
//!    [`DocumentProduct`]; retained memory is therefore proportional to the selected subtree, not the whole input.
//!    When the bound requirement carries [`jqf_codec_core::AccessRequirement::type_demand`], this phase skips the
//!    payload and builds a kind-only document (empty array/object, or a dummy scalar) from the located value's first
//!    payload byte. `PATH | type` answers array/object/string/number/boolean/null without retaining members. The
//!    validate walk still visits every unread byte.
//!
//! The published [`AccessOutcome::Located`] carries the identical [`ExactSelectionRecord`] the
//! whole-decode-then-navigate path publishes, so the SDK/CLI behaviour (located value / `null` / typed error) is
//! byte-identical.
//!
//! Input-charge asymmetry: the scoped route trades retained memory for input work. It reads the input up to two times
//! — the validate pass scans the whole input (and records the exact-path span), and the materialize pass re-parses
//! just the located span — so its work-byte charge is up to ~2x the input, versus the whole route's single pass. That
//! asymmetry is the deliberate cost of bounding *retained* memory to the selected subtree instead of the whole
//! document; the binder only prefers this route for `Located` requirements whose demand fits the scoped ceiling, where
//! the retained-memory saving is worth the extra input scan.

use alloc::string::String;
use alloc::vec::Vec;
use core::mem;

use jqf_codec_core::{
    AccessInput, AccessOutcome, AccessResult, AccessSession, CodecError, CodecFailureKind, CodecRunContext,
    DocumentProduct, ExactSelectionRecord, LocatedOutcome, PortableStep, PruneTree, SelectionOrigin,
};
use jqf_data::{
    AccountedDocumentBuilder, AccountedSemanticNode, AuthoritativeEmptyFamilies, BuilderCoverage, DataError,
    DiagnosticCoverage, DocumentCapabilityFamily, DocumentSchemaPrototype, DocumentSchemaRecipe, LazySpanMaterializer,
    ValueKind,
};
use jqf_resource::{OwnedDepthGuard, ResourceContext};
use jqf_source::ResolvedSource;

use crate::byte_scan::Utf8Carry;
use crate::error::diag;
use crate::error::{data_contract, invalid, unsupported_number};
use crate::lex::{
    BYTE_ORDER_MARK, admit_bytes, advance_number, consume_digit_run, hex, is_value_boundary, number_finish_plan,
    number_lex_complete, plain_string_run_end, scalar_width_from_lead, single_quoted_run_end,
};
use crate::parse::{JsonParseState, OwnedRunPoll, reset_reusable_frames};
use crate::storage::{EscapeState, JsonGrammar, NumberLex, NumberState, ParseMode};

/// One owned exact-path step, decoupled from the requirement's lifetime so the scoped session can live in the
/// core-owned carrier.
pub(crate) enum ScopedStep {
    Member(String),
    Index(i64),
    /// A contiguous element RANGE over an array container. Bounds are signed-or-open exactly as the portable step
    /// carries them; this route resolves them against the OBSERVED array length, which is the `SemanticIndex` precedent
    /// applied to a pair of bounds.
    ///
    /// A range step is always TERMINAL: the engine admits exactly one trailing range in a pushdown prefix, so a
    /// non-terminal one is a contract violation rather than a shape this navigator resolves.
    Range {
        start: Option<i64>,
        end: Option<i64>,
    },
}

/// The structural container a validator frame is tracking.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Container {
    Array,
    Object,
}

/// Validator frame expectation, mirroring the whole parser's `FrameState` transitions exactly so structural errors fire
/// at identical positions.
#[derive(Clone, Copy, Debug)]
enum VState {
    ArrayValueOrEnd { may_end: bool },
    ArrayCommaOrEnd,
    ObjectKeyOrEnd { may_end: bool },
    ObjectColon,
    ObjectValue,
    ObjectCommaOrEnd,
}

struct VFrame {
    state: VState,
    _depth: OwnedDepthGuard,
    /// The session path's length when this frame was pushed: the frame's component lives at `path[path_depth]`, and
    /// closing truncates there.
    path_depth: usize,
    /// How many array values this frame has validated so far (the index of the value about to be validated).
    values: usize,
    /// Byte offset of the `{` or `[` that opened this container.
    start: usize,
    /// Whether the exact-path locator pushed a scan frame for this container.
    scanning: bool,
}

/// Whether the string currently being validated is an object key or a value, which determines the frame transition on
/// the closing quote.
#[derive(Clone, Copy, Eq, PartialEq)]
enum StrTarget {
    Key,
    Value,
}

struct VString {
    target: StrTarget,
    cursor: usize,
    escape: EscapeState,
    /// The opening quote's offset, for the key-text capture at key close.
    start: usize,
    /// `b'"'` or, under JSON5, `b'\''`.
    quote: u8,
}

/// One step of the frontier path being validated: an object key or an array position. Maintained additively for the
/// `FRONTIER_VALIDATION` record — purely observational, never read by the validator's grammar decisions.
enum PathComponent {
    Key(String),
    Index(usize),
}

impl PathComponent {
    /// The JSON-path spelling (`.key` / `[i]`) the frontier record names.
    fn render(&self) -> alloc::string::String {
        match self {
            PathComponent::Key(key) => {
                alloc::format!(".{key}")
            }
            PathComponent::Index(index) => alloc::format!("[{index}]"),
        }
    }
}

/// Resumable validate-only phase, mirroring `ParsePhase` minus materialization.
enum ValidatePhase {
    /// The SIMD UTF-8 pre-scan that runs validate-everything-first so the grammar walk's per-scalar `from_utf8` can be
    /// dropped. Skipped in adjacent-value mode, where only the first value is validated.
    Utf8Scan(Utf8ScanState),
    Value,
    String(VString),
    Number(NumberState),
    Trailing,
    Done,
}

/// Windowed state of the UTF-8 pre-scan: absolute cursor plus the truncated sequence a window boundary may have to
/// carry.
struct Utf8ScanState {
    cursor: usize,
    carry: Utf8Carry,
}

/// The located exact-path observation, in raw-byte terms. Carries the located value's byte span (`Node`) or the step at
/// which navigation stopped; the materialize phase reads the span to build the sub-source and the same value is handed
/// to `publish` as the selection record.
#[derive(Clone, Copy)]
pub(crate) enum Located {
    Node {
        start: usize,
        end: usize,
    },
    Missing {
        step: usize,
    },
    TypeMismatch {
        step: usize,
        actual: ValueKind,
    },
    /// A resolved element RANGE over an array container: `[start, end)` is the byte region holding exactly the in-range
    /// elements, comma-separated and WITHOUT the array's own brackets (a range span `e,e,e` is not standalone JSON —
    /// every consumer either re-brackets it or scans it as elements).
    ///
    /// The resolver stops at the resolved end for non-negative bounds, so an element beyond the range is never scanned.
    /// An empty range reports `start == end`.
    Range {
        start: usize,
        end: usize,
    },
}

/// Exact-path observation recorded during the validate walk. Last-value-wins object members, signed array indices, and
/// trailing ranges resolve to byte spans captured during validation; the materialize phase re-parses the winning span
/// once validation reports `Done`.
struct PathLocator {
    result: Option<Located>,
    stack: Vec<LocateFrame>,
    /// Type mismatch for the child currently being validated, set when a candidate container's kind does not match the
    /// next path step.
    pending_mismatch: Option<Located>,
}

struct LocateFrame {
    step: usize,
    winner: Option<(usize, usize)>,
    nested: Option<Located>,
    count: usize,
    ring: Option<Ring>,
    range: Option<RangeTrack>,
    key_matches: bool,
}

struct RangeTrack {
    start: usize,
    end: Option<usize>,
    pending: Option<(Option<i64>, Option<i64>)>,
    region_start: Option<usize>,
    region_end: usize,
    /// Pending-path state, bounded by the AUTHORED BOUNDS rather than the container's length: the span vector this
    /// replaced grew 16 bytes per observed element of a huge array, contradicting the route's bounded-retained-memory
    /// contract. `finish` reads exactly two spans — the window's first and last — so each endpoint is captured by
    /// one fixed-index slot, one head/tail scalar, or a tail ring capped at the largest look-back a NEGATIVE bound can
    /// name.
    from_kind: RangeEndpoint,
    to_kind: RangeEndpoint,
    from_span: Option<(usize, usize)>,
    to_span: Option<(usize, usize)>,
    first_start: Option<usize>,
    last_end: Option<usize>,
    tail: Option<TailSpans>,
}

/// Which span a pending range's endpoint names. All variants are known from the authored bounds alone — a negative
/// bound's look-back distance does not depend on the container's length.
#[derive(Clone, Copy)]
enum RangeEndpoint {
    /// A non-negative bound names one fixed index.
    Fixed(usize),
    /// The window starts at the container's head.
    Head,
    /// The window runs to the container's tail.
    Tail,
    /// A negative bound names an index counted from the end; the constant is the look-back from the most recent span
    /// (`1` = the last span).
    Back(usize),
}

/// One step of the escape-family automaton (`\x`, `\uXXXX`, surrogate pairs) after the hot loop consumed `byte` at
/// position `at`. These are the string walk's rare arms, factored out so the common Plain-state loop compiles to
/// registers instead of carrying the `\u`/surrogate bookkeeping (u16 accumulation, hex digits) and their error branches
/// inline. The returned state is stored into the walk's `EscapeState` by the caller.
///
/// Deliberately NOT marked `#[cold]`: under PGO the cold weight fought the profile's own branch weights and sank the
/// whole string walk into a worse layout (measured 2x on the container count lane), so the profile owns the layout
/// here.
#[expect(
    clippy::needless_pass_by_value,
    reason = "the caller replaces its escape slot with the returned state, so the old state is owned and consumed here"
)]
fn escape_step(
    escape: EscapeState,
    byte: u8,
    source: ResolvedSource<'_>,
    at: usize,
    json5: bool,
) -> Result<EscapeState, CodecError> {
    match escape {
        EscapeState::Escape => match byte {
            b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => Ok(EscapeState::Plain),
            b'u' => Ok(EscapeState::Unicode { value: 0, digits: 0 }),
            _ => Err(invalid(source, at, diag::INVALID_ESCAPE, diag::MSG_INVALID_ESCAPE)),
        },
        EscapeState::Hex { mut value, mut digits } => {
            if !json5 {
                return Err(invalid(source, at, diag::INVALID_ESCAPE, diag::MSG_INVALID_ESCAPE));
            }
            let digit =
                hex(byte).ok_or_else(|| invalid(source, at, diag::INVALID_ESCAPE, diag::MSG_INVALID_ESCAPE_JSON5))?;
            value = (value << 4) | digit;
            digits += 1;
            if digits == 2 {
                Ok(EscapeState::Plain)
            } else {
                Ok(EscapeState::Hex { value, digits })
            }
        }
        EscapeState::Unicode { mut value, mut digits } => {
            let digit =
                hex(byte).ok_or_else(|| invalid(source, at, "invalid-unicode-escape", "invalid Unicode escape"))?;
            value = (value << 4) | u16::from(digit);
            digits += 1;
            if digits != 4 {
                Ok(EscapeState::Unicode { value, digits })
            } else if (0xd800..=0xdbff).contains(&value) {
                Ok(EscapeState::LowBackslash { high: value })
            } else {
                // A lone low surrogate decodes to U+FFFD, exactly as the whole parser decodes it; only a high surrogate
                // not followed by a valid low one raises. The scoped validator is a validity mirror of the whole parser
                // and must accept the same escapes.
                Ok(EscapeState::Plain)
            }
        }
        EscapeState::LowBackslash { high } => {
            if byte == b'\\' {
                Ok(EscapeState::LowU { high })
            } else {
                Err(invalid(
                    source,
                    at,
                    "missing-low-surrogate",
                    "high surrogate must be followed by a low surrogate",
                ))
            }
        }
        EscapeState::LowU { high } => {
            if byte == b'u' {
                Ok(EscapeState::LowUnicode {
                    high,
                    value: 0,
                    digits: 0,
                })
            } else {
                Err(invalid(
                    source,
                    at,
                    "missing-low-surrogate",
                    "high surrogate must be followed by a low surrogate",
                ))
            }
        }
        EscapeState::LowUnicode {
            high,
            mut value,
            mut digits,
        } => {
            let digit =
                hex(byte).ok_or_else(|| invalid(source, at, "invalid-unicode-escape", "invalid Unicode escape"))?;
            value = (value << 4) | u16::from(digit);
            digits += 1;
            if digits != 4 {
                Ok(EscapeState::LowUnicode { high, value, digits })
            } else if !(0xdc00..=0xdfff).contains(&value) {
                // The pair's closing `\u` started four bytes before the last hex digit, which is the label position the
                // pre-hoist code reported.
                Err(invalid(
                    source,
                    at - 3,
                    "invalid-surrogate-pair",
                    "invalid Unicode surrogate pair",
                ))
            } else {
                // Validation only proves the pair is well-formed; the decoded scalar (which would use `high`) is
                // recomputed by the materializer, so nothing is emitted here.
                Ok(EscapeState::Plain)
            }
        }
        EscapeState::Plain => unreachable!("the hot loop handles the Plain state"),
    }
}

/// Cold construction of a string-walk failure: a `CodecError` build (label spans, message) is never worth inlining into
/// the hot walk, and the cold call keeps the error arms out of the loop's layout.
fn string_fault(source: ResolvedSource<'_>, at: usize, code: &'static str, message: &'static str) -> CodecError {
    invalid(source, at, code, message)
}

/// Scoped session state stored in the core-owned carrier.
#[allow(
    clippy::struct_excessive_bools,
    reason = "frontier_active is the sink-presence gate for the diagnostics-only frontier; \
              the other three flags (root_seen, utf8_pre_scan, allow_adjacent_values) are \
              distinct validator states — grouping them would hide the gate"
)]
pub(crate) struct ScopedSession {
    steps: Vec<ScopedStep>,
    origin: SelectionOrigin,
    diagnostics: DiagnosticCoverage,
    coverage: BuilderCoverage,
    /// Kept-subtree hint for the materialize pass. Validation still visits every
    /// byte; prune only changes what the re-parse of the located span builds.
    prune: Option<PruneTree>,
    /// Kind-only materialize: after locate, emit an empty-of-children node from the first payload byte. Validation is
    /// unchanged.
    type_demand: bool,
    phase: SessionPhase,
    // Validate-phase state.
    validate: ValidatePhase,
    frames: Vec<VFrame>,
    /// The frontier path of the value currently being validated (additive state for the `FRONTIER_VALIDATION` record;
    /// the validator never reads it).
    frontier: Vec<PathComponent>,
    /// Whether the frontier is worth maintaining at all: it is read ONLY by `FRONTIER_VALIDATION`, whose emission is a
    /// no-op when no diagnostic sink is installed. With no sink the per-key `String` allocation and the per-container
    /// push/pop are dead weight on every scoped route's validate pass, so this is derived from the sink's presence at
    /// session creation.
    frontier_active: bool,
    cursor: usize,
    root_seen: bool,
    // UTF-8 pre-scan state (valid after the pre-scan completes).
    utf8_first_invalid: Option<usize>,
    utf8_pre_scan: bool,
    // Locate/materialize outputs.
    located: Option<Located>,
    locator: PathLocator,
    /// Start of the scalar/string/number currently being validated.
    pending_value_start: usize,
    schema_prototype: Option<DocumentSchemaPrototype>,
    materializer: Option<JsonParseState>,
    /// The bracket-wrapping buffer a resolved RANGE materializes out of, built once per session and never for a
    /// non-range path.
    range_buffer: Option<Vec<u8>>,
    allow_adjacent_values: bool,
    consumed_offset: Option<u64>,
    /// Whether the token just validated ran to the LAST byte the validator was given without a delimiter closing it, so
    /// more input could extend it into a different token (`1234` into `1234567`, `inf` into `infinity`). The same law
    /// the whole parser reports on [`JsonParseState::open_ended`]: the session cannot tell "the input ended" from "this
    /// window's bytes ran out", so it publishes the value it has and reports the ambiguity on
    /// [`jqf_codec_core::AccessReport::open_ended`] for the caller — who alone knows whether the source is still
    /// growing — to resolve.
    open_ended: bool,
    /// Whether this session has already applied the adjacent-value source-BOM law (shared with the whole-document
    /// route): a UTF-8 BOM at ABSOLUTE offset zero of the input is consumed before the first value, exactly as the
    /// whole-document route consumes it. The flag makes the skip idempotent across cooperative resumption and is reset
    /// per value in [`Self::try_reset`].
    bom_handled: bool,
    /// The grammar this scoped session validates: strict JSON copies `STRICT` plus the leniency dial; JSONC/JSON5 arm
    /// comments (and JSON5 the rest) so the walk matches [`JsonParseState`].
    grammar: JsonGrammar,
    /// Comment-fact materialize extras when this session serves JSONC/JSON5. Strict JSON leaves this unset.
    commented: Option<CommentedScope>,
    /// Leading comment texts awaiting the next value, collected only when the grammar arms comments and coverage
    /// demands attached facts.
    pending_comments: Vec<String>,
    /// Leading comments that belong to the located node (last-value-wins), captured at the winner's start.
    located_comments: Vec<String>,
    /// Bytes already granted toward a trivia run at the cursor, matching [`JsonParseState`]'s window law so a comment
    /// longer than one admission is not truncated.
    trivia_window: usize,
}

/// JSONC/JSON5 extras the scoped Exact materializer needs: dynamic schema (so `add_fact` is legal), the comment role,
/// and the dialect's lazy span reader. When coverage demands attached facts, the lazy reader attaches leading comments
/// on the nested document; Exact never takes that path and re-parses the located span through [`JsonParseState`].
#[derive(Clone, Copy)]
pub(crate) struct CommentedScope {
    /// Grammar this dialect accepts (comments always on; trailing commas and json5 per spec).
    pub grammar: JsonGrammar,
    /// `jsonc.comment@1` or `json5.comment@1`.
    pub comment_role: &'static str,
    /// Schema recipe that declares the comment fact role.
    pub recipe: fn(&'static str) -> Result<DocumentSchemaRecipe<'static>, DataError>,
    /// Dialect text the recipe binds.
    pub dialect: &'static str,
    /// Bound onto the Exact re-parse as unreachable insurance: the scoped
    /// materializer never commits a `ContainerSpan` child, so this reader is
    /// not invoked on this route.
    pub span_materializer: &'static dyn LazySpanMaterializer,
    /// JSONC consumes a leading BOM as a source prefix; JSON5 treats U+FEFF as whitespace instead.
    pub consume_bom: bool,
}

#[allow(
    clippy::large_enum_variant,
    reason = "one scoped session per request; boxing would add an extra allocation"
)]
enum SessionPhase {
    Validate,
    Materialize,
    Finished,
}

enum TriviaSkip {
    Skipped(usize),
    Content,
    Yield,
    Cut,
}

impl ScopedSession {
    pub(crate) fn try_new_with_schema_prototype(
        steps: &[PortableStep],
        origin: SelectionOrigin,
        diagnostics: DiagnosticCoverage,
        coverage: BuilderCoverage,
        allow_adjacent_values: bool,
        schema_prototype: DocumentSchemaPrototype,
        resources: &ResourceContext<'_>,
    ) -> Result<Self, CodecError> {
        Self::try_new_inner(
            steps,
            origin,
            diagnostics,
            coverage,
            allow_adjacent_values,
            Some(schema_prototype),
            resources,
        )
    }

    /// JSONC/JSON5 Exact session: the same two-phase walk, with this dialect's grammar and a dynamic schema so comment
    /// facts can attach.
    pub(crate) fn try_new_commented(
        steps: &[PortableStep],
        origin: SelectionOrigin,
        diagnostics: DiagnosticCoverage,
        coverage: BuilderCoverage,
        scope: CommentedScope,
        resources: &ResourceContext<'_>,
    ) -> Result<Self, CodecError> {
        let mut session = Self::try_new_inner(steps, origin, diagnostics, coverage, false, None, resources)?;
        session.grammar = scope.grammar;
        session.commented = Some(scope);
        Ok(session)
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "one scoped session owns its path, decode shape, optional shared schema, and request"
    )]
    fn try_new_inner(
        steps: &[PortableStep],
        origin: SelectionOrigin,
        diagnostics: DiagnosticCoverage,
        coverage: BuilderCoverage,
        allow_adjacent_values: bool,
        schema_prototype: Option<DocumentSchemaPrototype>,
        resources: &ResourceContext<'_>,
    ) -> Result<Self, CodecError> {
        let mut owned = {
            let mut steps_vec = Vec::new();
            steps_vec
                .try_reserve_exact(steps.len())
                .map_err(jqf_resource::ResourceError::from)?;
            steps_vec
        };
        for step in steps {
            let owned_step = match step {
                PortableStep::SemanticMember(key) => {
                    let mut stored = String::new();
                    stored
                        .try_reserve_exact(key.as_str().len())
                        .map_err(jqf_resource::ResourceError::from)?;
                    stored.push_str(key.as_str());
                    ScopedStep::Member(stored)
                }
                PortableStep::SemanticIndex(index) => ScopedStep::Index(*index),
                PortableStep::SemanticRange { start, end } => ScopedStep::Range {
                    start: *start,
                    end: *end,
                },
            };
            owned.push(owned_step);
        }
        Ok(Self {
            steps: owned,
            origin,
            diagnostics,
            coverage,
            prune: None,
            type_demand: false,
            phase: SessionPhase::Validate,
            validate: if allow_adjacent_values {
                ValidatePhase::Value
            } else {
                ValidatePhase::Utf8Scan(Utf8ScanState {
                    cursor: 0,
                    carry: Utf8Carry::EMPTY,
                })
            },
            frames: Vec::new(),
            frontier: Vec::new(),
            frontier_active: resources.diagnostics_sink().is_some(),
            cursor: 0,
            root_seen: false,
            utf8_first_invalid: None,
            utf8_pre_scan: !allow_adjacent_values,
            located: None,
            locator: PathLocator::new(),
            pending_value_start: 0,
            schema_prototype,
            materializer: None,
            range_buffer: None,
            allow_adjacent_values,
            consumed_offset: None,
            open_ended: false,
            bom_handled: false,
            // The scoped validator must accept exactly what the whole parser accepts under the resource dial, so the
            // same grammar carries the same leniency — read once at construction. JSONC/JSON5 overwrite this in
            // [`Self::try_new_commented`].
            grammar: JsonGrammar {
                lenient: resources.decode_lenient(),
                ..JsonGrammar::STRICT
            },
            commented: None,
            pending_comments: Vec::new(),
            located_comments: Vec::new(),
            trivia_window: 0,
        })
    }

    /// Arms the kept-subtree prune on the materialize pass. Validation is
    /// unchanged. The provider copies the requirement's hint here at open.
    pub(crate) fn arm_prune(&mut self, tree: PruneTree) {
        self.prune = Some(tree);
    }

    /// Arms kind-only materialize from the bound requirement's type hint.
    pub(crate) fn set_type_demand(&mut self, demand: bool) {
        self.type_demand = demand;
    }

    fn new_materializer(
        &mut self,
        mode: ParseMode,
        resources: &ResourceContext<'_>,
    ) -> Result<JsonParseState, CodecError> {
        let mut state = if let Some(scope) = self.commented {
            let mut state = JsonParseState::new(self.diagnostics, self.coverage, mode);
            let recipe = (scope.recipe)(scope.dialect).map_err(|_| data_contract())?;
            state.with_dynamic_recipe(recipe);
            state.set_comment_fact_role(scope.comment_role);
            state.set_span_materializer(scope.span_materializer);
            state
        } else {
            if self.schema_prototype.is_none() {
                self.schema_prototype = Some(crate::parse::try_schema_prototype(resources)?);
            }
            let prototype = self.schema_prototype.take().ok_or_else(data_contract)?;
            JsonParseState::with_schema_prototype(prototype, self.diagnostics, self.coverage, mode)
        };
        state.set_grammar(self.grammar);
        if self.utf8_pre_scan {
            state.set_utf8_proved();
        }
        if let Some(tree) = &self.prune {
            state.enable_prune(tree);
        }
        Ok(state)
    }

    /// Reinitializes this session for one more adjacent value over the same exact path, keeping the retained step
    /// workspace and only bounded validator/materializer frame workspaces.
    ///
    /// Declines (returns `false`) when the requested path differs from the one this session owns: rebuilding the step
    /// vector would defeat the reuse and the caller constructs a fresh session instead. Everything else — phase,
    /// cursor, locator, located record, and the materializer's document-local state — is dropped. The materializer
    /// shell and immutable schema prototype remain reusable, so a value that failed mid-parse cannot leak document
    /// state into the next one.
    pub(crate) fn try_reset(
        &mut self,
        steps: &[PortableStep],
        origin: SelectionOrigin,
        diagnostics: DiagnosticCoverage,
        coverage: BuilderCoverage,
        allow_adjacent_values: bool,
    ) -> bool {
        if !self.owns_exact_path(steps) {
            return false;
        }
        self.origin = origin;
        self.diagnostics = diagnostics;
        self.coverage = coverage;
        self.phase = SessionPhase::Validate;
        self.type_demand = false;
        self.validate = if allow_adjacent_values {
            ValidatePhase::Value
        } else {
            ValidatePhase::Utf8Scan(Utf8ScanState {
                cursor: 0,
                carry: Utf8Carry::EMPTY,
            })
        };
        reset_reusable_frames(&mut self.frames);
        // The frontier is popped per container as validation leaves it, which never runs for a value that failed INSIDE
        // one. A reopened session must start at the root, or the next value's frontier records name a path prefixed by
        // the dead value's.
        self.frontier.clear();
        self.cursor = 0;
        self.root_seen = false;
        self.utf8_first_invalid = None;
        self.utf8_pre_scan = !allow_adjacent_values;
        self.located = None;
        self.locator = PathLocator::new();
        self.pending_value_start = 0;
        // A range selection re-parses a session-owned wrap buffer, so its materializer runs in copy mode; every other
        // selection materializes a bounded span of the retained input in place.
        let materializer_mode = if matches!(self.steps.as_slice().last(), Some(ScopedStep::Range { .. })) {
            ParseMode::OwnedRun
        } else {
            ParseMode::Document
        };
        if let Some(materializer) = &mut self.materializer {
            materializer.reset(diagnostics, coverage, materializer_mode);
            if let Some(tree) = &self.prune {
                materializer.enable_prune(tree);
            }
        }
        self.range_buffer = None;
        self.allow_adjacent_values = allow_adjacent_values;
        self.consumed_offset = None;
        self.open_ended = false;
        self.bom_handled = false;
        self.pending_comments.clear();
        self.located_comments.clear();
        self.trivia_window = 0;
        true
    }

    /// Whether the retained step vector already spells exactly this path.
    fn owns_exact_path(&self, steps: &[PortableStep]) -> bool {
        let owned = self.steps.as_slice();
        owned.len() == steps.len()
            && owned
                .iter()
                .zip(steps)
                .all(|(owned, requested)| match (owned, requested) {
                    (ScopedStep::Member(owned), PortableStep::SemanticMember(requested)) => {
                        owned.as_str() == requested.as_str()
                    }
                    (ScopedStep::Index(owned), PortableStep::SemanticIndex(requested)) => owned == requested,
                    (
                        ScopedStep::Range { start, end },
                        PortableStep::SemanticRange {
                            start: requested_start,
                            end: requested_end,
                        },
                    ) => start == requested_start && end == requested_end,
                    _ => false,
                })
    }

    /// Emits the `FRONTIER_VALIDATION` record for a validate-phase failure: the corrupt byte's path and offset — the
    /// thing the program may never have read but validate-everything-first made fatal anyway. A no-op when no
    /// diagnostic sink is installed.
    fn emit_frontier_record(&self, resources: &ResourceContext<'_>) {
        // The doc-commented law: a no-op when no diagnostic sink is installed. Rendering the path allocates, so gate
        // BEFORE it.
        if !self.frontier_active {
            return;
        }
        let path = self
            .frontier
            .as_slice()
            .iter()
            .map(PathComponent::render)
            .collect::<alloc::string::String>();
        let path_text = if path.is_empty() {
            alloc::string::String::from("<root>")
        } else {
            path
        };
        let mut record = jqf_resource::diag::DiagnosticRecord::new(jqf_resource::diag::codes::FRONTIER_VALIDATION);
        record.operand = Some(&path_text);
        record.byte_offset = Some(u64::try_from(self.offset()).unwrap_or(u64::MAX));
        resources.record_diagnostic(record);
    }

    fn poll_source<'source>(
        &mut self,
        source: ResolvedSource<'source>,
        context: &mut CodecRunContext<'_, '_>,
    ) -> Result<AccessResult<'source>, CodecError> {
        if source.bytes().len() > u32::MAX as usize {
            return Err(CodecError::new(CodecFailureKind::Overflow));
        }
        loop {
            match &mut self.phase {
                SessionPhase::Validate => {
                    match self.validate_step(source, context.resources()) {
                        Ok(false) => context.replenish_work()?,
                        Ok(true) => {}
                        Err(error) => {
                            self.emit_frontier_record(context.resources());
                            return Err(error);
                        }
                    }
                    if matches!(self.validate, ValidatePhase::Done) {
                        // Whole input is valid: the exact-path span was recorded during the walk. Publish only after
                        // Done.
                        self.finish_validate(source)?;
                        self.located = Some(self.locator.take()?);
                        self.phase = SessionPhase::Materialize;
                    }
                }
                SessionPhase::Materialize => {
                    return self.materialize_step(source, context);
                }
                SessionPhase::Finished => {
                    return Err(data_contract());
                }
            }
        }
    }

    /// Materializes the located value (or a `null` placeholder for the negative observations) into a demand-scoped
    /// document and publishes the Located outcome carrying the identical `ExactSelectionRecord`.
    fn materialize_step<'source>(
        &mut self,
        source: ResolvedSource<'source>,
        context: &mut CodecRunContext<'_, '_>,
    ) -> Result<AccessResult<'source>, CodecError> {
        let record = *self.located.as_ref().ok_or_else(data_contract)?;
        if self.type_demand && matches!(record, Located::Node { .. } | Located::Range { .. }) {
            return self.materialize_kind_only(source, record, context.resources());
        }
        // A RANGE span is `e,e,e` — not standalone JSON — so it materializes through a BRACKET-WRAPPING re-parse
        // out of a session-owned buffer. The buffer cannot be borrowed by the published product, so the range arm
        // parses in COPY mode and yields a self-contained `'static` product; the ordinary arm keeps its zero-copy
        // borrow of the input.
        if let Located::Range { start, end, .. } = record {
            return self.materialize_range(source, record, start, end, context);
        }
        let span = match record {
            Located::Node { start, end } => {
                // Identity Exact with comment facts re-parses the whole source so file-level leading/trailing comments
                // attach. A subtree Exact keeps the located value span and seeds comments collected outside it.
                if self.steps.is_empty() && self.coverage.attached_facts() && self.grammar.comments {
                    let bom = self.commented.is_some_and(|scope| scope.consume_bom)
                        && source.bytes().starts_with(&BYTE_ORDER_MARK);
                    Some((if bom { BYTE_ORDER_MARK.len() } else { 0 }, source.bytes().len()))
                } else {
                    Some((start, end))
                }
            }
            Located::Missing { .. } | Located::TypeMismatch { .. } => None,
            // Handled above; the range arm never reaches the borrowed path.
            Located::Range { .. } => return Err(data_contract()),
        };
        let bytes = match span {
            Some((start, end)) => &source.bytes()[start..end],
            None => b"null",
        };
        let base = match span {
            Some((start, _)) => source.base_offset().saturating_add(start as u64),
            None => 0,
        };
        let sub = ResolvedSource::new(source.source(), source.label(), bytes, base);
        if self.materializer.is_none() {
            // The located span is exactly one bounded value with nothing after it; adjacent-value trailing tolerance is
            // never relevant here regardless of the outer session's own opt-in.
            self.materializer = Some(self.new_materializer(ParseMode::Document, context.resources())?);
        }
        let materializer = self.materializer.as_mut().ok_or_else(data_contract)?;
        if !self.located_comments.is_empty() {
            materializer.seed_pending_comments(mem::take(&mut self.located_comments));
        }
        let result = materializer.decode(AccessInput::Source(sub), context)?;
        let product = match result.into_parts().0 {
            AccessOutcome::FullDocument(product) => product,
            AccessOutcome::Located(_) => return Err(data_contract()),
        };
        self.phase = SessionPhase::Finished;
        self.publish(&product, record)
    }

    /// Materializes a resolved element RANGE by re-parsing `[` + the range's element bytes + `]` out of a session-owned
    /// buffer.
    ///
    /// The wrap is not a convenience: a range span is a comma-separated element sequence, which no JSON parser accepts
    /// as a document. Re-bracketing produces exactly the bytes the reference's own slice materialization would.
    fn materialize_range<'source>(
        &mut self,
        source: ResolvedSource<'source>,
        record: Located,
        start: usize,
        end: usize,
        context: &mut CodecRunContext<'_, '_>,
    ) -> Result<AccessResult<'source>, CodecError> {
        if self.range_buffer.is_none() {
            let mut buffer = {
                let mut bytes = Vec::new();
                bytes
                    .try_reserve_exact(end.saturating_sub(start).saturating_add(2))
                    .map_err(jqf_resource::ResourceError::from)?;
                bytes
            };
            buffer.extend_from_slice(b"[");
            if end > start {
                buffer.extend_from_slice(&source.bytes()[start..end]);
            }
            buffer.extend_from_slice(b"]");
            self.range_buffer = Some(buffer);
            // Copy mode: the product must outlive this session-owned buffer.
            self.materializer = Some(self.new_materializer(ParseMode::OwnedRun, context.resources())?);
        }
        let buffer = self.range_buffer.as_ref().ok_or_else(data_contract)?;
        // The wrapped bytes are valid by construction (the whole input already passed the complete validator and the
        // region is a structural cut), so no diagnostic ever renders this synthetic label or offset.
        let synthetic = ResolvedSource::new(source.source(), "range", buffer.as_slice(), 0);
        if !self.located_comments.is_empty()
            && let Some(materializer) = self.materializer.as_mut()
        {
            materializer.seed_pending_comments(mem::take(&mut self.located_comments));
        }
        // A LOOP, not recursion: one stack frame per Pending would scale with range_bytes / per-entry credits and
        // exhaust the stack on a large range selection. The lazy route's span materializer drives the same poll with a
        // loop (`lazy.rs`), which also keeps the walk cancellable — the replenish inside carries the control checks.
        let product = loop {
            let materializer = self.materializer.as_mut().ok_or_else(data_contract)?;
            match materializer.poll_owned(synthetic, context)? {
                OwnedRunPoll::Pending => context.replenish_work()?,
                OwnedRunPoll::Ready(product) => break product,
            }
        };
        self.phase = SessionPhase::Finished;
        self.publish(&product, record)
    }

    /// Builds a kind-only document from the located node's first payload byte. Validation already proved the span.
    fn materialize_kind_only<'source>(
        &mut self,
        source: ResolvedSource<'source>,
        record: Located,
        resources: &mut ResourceContext<'_>,
    ) -> Result<AccessResult<'source>, CodecError> {
        let kind = match record {
            Located::Node { start, .. } => {
                // `Located::Node.start` is the value's first payload byte.
                element_kind(source.bytes(), start)
            }
            Located::Range { .. } => ValueKind::Array,
            Located::Missing { .. } | Located::TypeMismatch { .. } => return Err(data_contract()),
        };
        let product = self.kind_only_product(kind, resources)?;
        self.phase = SessionPhase::Finished;
        self.publish(&product, record)
    }

    fn kind_only_product(
        &self,
        kind: ValueKind,
        resources: &mut ResourceContext<'_>,
    ) -> Result<DocumentProduct<'static>, CodecError> {
        let mut builder = match self.commented {
            Some(scope) => {
                let recipe = (scope.recipe)(scope.dialect).map_err(|_| data_contract())?;
                AccountedDocumentBuilder::try_new_with_coverage(recipe.format(), recipe.dialect(), self.coverage)
                    .map_err(crate::lex::map_data)?
            }
            None => AccountedDocumentBuilder::try_new_with_coverage(
                crate::FORMAT_ID,
                Some(crate::RFC8259_DIALECT_ID),
                self.coverage,
            )
            .map_err(crate::lex::map_data)?,
        };
        builder.set_authoritative_empty_families(AuthoritativeEmptyFamilies::from_family(
            DocumentCapabilityFamily::Attributes,
        ));
        builder.set_diagnostic_coverage(self.diagnostics);
        let (node_kind, semantic) = match kind {
            ValueKind::Null => (crate::parse::JSON_NODE_KINDS[0], AccountedSemanticNode::Null),
            ValueKind::Bool => (crate::parse::JSON_NODE_KINDS[1], AccountedSemanticNode::Bool(false)),
            ValueKind::Number => (crate::parse::JSON_NODE_KINDS[2], AccountedSemanticNode::Integer("0")),
            ValueKind::String => (crate::parse::JSON_NODE_KINDS[3], AccountedSemanticNode::String("")),
            ValueKind::Array => (
                crate::parse::JSON_NODE_KINDS[4],
                AccountedSemanticNode::Array {
                    item_role: crate::parse::JSON_OCCURRENCE_ROLES[0],
                },
            ),
            ValueKind::Object => (
                crate::parse::JSON_NODE_KINDS[5],
                AccountedSemanticNode::Object {
                    member_role: crate::parse::JSON_OCCURRENCE_ROLES[1],
                },
            ),
            _ => return Err(data_contract()),
        };
        let root = builder
            .add_node(node_kind, semantic, None, resources)
            .map_err(crate::lex::map_data)?;
        let document = builder.finish(root, resources).map_err(crate::lex::map_data)?;
        DocumentProduct::try_new(document, resources)
    }

    fn publish<'source>(
        &self,
        product: &DocumentProduct<'source>,
        record: Located,
    ) -> Result<AccessResult<'source>, CodecError> {
        let selection = match record {
            // The span is already consumed by materialize; only the record kind matters for the published selection. A
            // materialized range is an ordinary NODE selection: the published value is the fresh array the wrap
            // produced, exactly as the reference's slice materializes a new array.
            Located::Node { .. } | Located::Range { .. } => ExactSelectionRecord::Node {
                node: product.document().root_handle(),
                origin: self.origin,
            },
            Located::Missing { step } => ExactSelectionRecord::Missing {
                step_index: step,
                origin: self.origin,
            },
            Located::TypeMismatch { step, actual } => ExactSelectionRecord::TypeMismatch {
                step_index: step,
                actual_type: actual,
                origin: self.origin,
                hint: None,
            },
        };
        let outcome = LocatedOutcome::try_new(product, selection)?;
        let access_outcome = AccessOutcome::Located(outcome);
        Ok(match self.consumed_offset {
            Some(consumed) => {
                let result = AccessResult::from_outcome_with_consumed_offset(access_outcome, consumed);
                if self.open_ended { result.open_ended() } else { result }
            }
            None => AccessResult::from_outcome(access_outcome),
        })
    }
}

impl AccessSession for ScopedSession {
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
        self.poll_source(source, context)
    }
}

// =================================================================== Validate phase: a resumable validate-only mirror
// of `JsonParseState`. ===================================================================

impl ScopedSession {
    /// Best-effort byte position for progress reporting, tracking whichever phase is active. During validate it follows
    /// the resumable scanner; once materializing the whole input is already validated, so the end of the located root
    /// is the honest consumed mark.
    fn offset(&self) -> usize {
        match &self.phase {
            SessionPhase::Validate => match &self.validate {
                ValidatePhase::Utf8Scan(state) => state.cursor,
                ValidatePhase::String(state) => state.cursor,
                ValidatePhase::Number(state) => state.cursor_usize(),
                _ => self.cursor,
            },
            SessionPhase::Materialize | SessionPhase::Finished => self.cursor,
        }
    }

    /// One resumable validate step. Returns `false` to yield cooperatively.
    fn validate_step(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        // The adjacent-value source-BOM law (shared with the whole-document route's slot-0 session): a UTF-8 BOM at
        // ABSOLUTE offset zero of the input is consumed before the first value, so a member-scoped query answers
        // exactly as a whole-document one does on the same bytes. Without this, `.a` on a BOM-prefixed file fails where
        // `.` succeeds — one input answering differently by program. `open_at` narrows every later value's source to
        // its own first byte (`base_offset > 0`), so the base-zero test is exactly the first-value test, and the
        // consumed extent includes the mark exactly as the whole route's does. The whole-document route enforces the
        // same law in its own session; this one sits in `poll_source`, the single entry every scoped-route value passes
        // through, so the mark cannot slip past it.
        let consume_bom = self.allow_adjacent_values || self.commented.is_some_and(|scope| scope.consume_bom);
        if !self.bom_handled && consume_bom && source.base_offset() == 0 {
            // A mark SPLIT by a read boundary is an INCOMPLETE mark, not a wrong byte: label the cut so the drive
            // refills, exactly as a token cut by the window is labelled. The latch stays open here so the refilled
            // window re-runs the law from the start.
            if crate::lex::byte_order_mark_truncated(source.bytes()) {
                return Err(invalid(
                    source,
                    source.bytes().len(),
                    diag::EXPECTED_VALUE,
                    diag::MSG_EXPECTED_VALUE,
                ));
            }
            self.bom_handled = true;
            if source.bytes().starts_with(&crate::lex::BYTE_ORDER_MARK) {
                self.cursor = crate::lex::BYTE_ORDER_MARK.len();
            }
        } else {
            self.bom_handled = true;
        }
        if matches!(&self.validate, ValidatePhase::Utf8Scan(_)) {
            return self.validate_utf8(source, resources);
        }
        let result = match &mut self.validate {
            ValidatePhase::Value => self.validate_value(source, resources),
            ValidatePhase::String(_) => self.validate_string(source, resources),
            ValidatePhase::Number(_) => self.validate_number(source, resources),
            ValidatePhase::Trailing => self.validate_trailing(source, resources),
            // The drivers never call a step on the Done phase — they observe it and transition — so the pre-scan
            // completion check lives in [`Self::finish_validate`], at the transition site.
            ValidatePhase::Done => return Ok(true),
            ValidatePhase::Utf8Scan(_) => unreachable!("handled above"),
        };
        // First-error parity with the interleaved per-scalar validation the pre-scan replaced: an invalid UTF-8 byte
        // found strictly BEFORE the walk's grammar error is the document's first fault, reported with the invalid-utf8
        // text. At the SAME byte the walk's own text wins, which is what the pre-scan-free walk reported there (a
        // structural high byte is a grammar error; the walk never errors on a non-ASCII byte inside a string, so the
        // tie can only be structural).
        //
        // The error's own primary label is the position — NOT `self.offset()`, which reads the phase cursor and is
        // meaningless DURING a step: the walk functions `mem::replace` the phase with `Value` while they run.
        if result.is_err()
            && let Some(position) = self.utf8_first_invalid
            && position
                < result
                    .as_ref()
                    .err()
                    .and_then(|error| error.diagnostic())
                    .and_then(|diagnostic| diagnostic.labels().first())
                    .map_or(usize::MAX, |label| label.span().start() as usize)
        {
            return Err(invalid(source, position, diag::INVALID_UTF8, diag::MSG_INVALID_UTF8));
        }
        result
    }

    /// The grammar walk reports `Done`: a pre-scan hit the walk never surfaced (its own errors all carry their
    /// position, and the error-rule reports the pre-scan hit only when it is strictly earlier) is the document's only
    /// fault. The drivers call this at the Done observation, because they never run a step on the Done phase itself.
    fn finish_validate(&self, source: ResolvedSource<'_>) -> Result<(), CodecError> {
        if let Some(position) = self.utf8_first_invalid {
            return Err(invalid(source, position, diag::INVALID_UTF8, diag::MSG_INVALID_UTF8));
        }
        Ok(())
    }

    /// One windowed step of the UTF-8 pre-scan. Returns `false` to yield cooperatively; transitions to the grammar walk
    /// when the whole range is consumed or the first invalid byte is found.
    fn validate_utf8(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        let total = source.bytes().len();
        let ValidatePhase::Utf8Scan(state) = &mut self.validate else {
            unreachable!("guarded by validate_step")
        };
        let remaining = total - state.cursor;
        let Some(granted) = admit_bytes(resources, remaining)? else {
            return Ok(false);
        };
        let limit = state.cursor + granted;
        let is_last = limit == total;
        // No window is scanned as final, not even the last one: the scan sees the bytes the decoder was GIVEN and
        // cannot tell them from the bytes the source HAS. A sequence the last byte cuts is reported where the input
        // stopped rather than at its lead, which is both the position the whole parser reports and the one a streaming
        // caller reads as "would complete with more input".
        let found = crate::byte_scan::utf8_first_invalid(
            &source.bytes()[state.cursor..limit],
            state.cursor,
            &mut state.carry,
            false,
        );
        if let Some(position) = found {
            self.utf8_first_invalid = Some(position);
            self.validate = ValidatePhase::Value;
            return Ok(true);
        }
        let truncated_at_end = is_last && !state.carry.is_empty();
        state.cursor = limit;
        if truncated_at_end {
            self.utf8_first_invalid = Some(total);
        }
        if is_last {
            self.validate = ValidatePhase::Value;
        }
        Ok(true)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one value-dispatch transition table; splitting it would hide which byte selects which arm"
    )]
    #[expect(
        clippy::needless_continue,
        reason = "explicit continue after a completing scalar keeps the batch-loop control flow visible"
    )]
    #[expect(
        clippy::redundant_else,
        reason = "the else arms name the handoff versus continue choice in the batch loop"
    )]
    fn validate_value(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        // Hoist the top frame's state and the cursor across a token run, the same shape the whole-document parser's
        // batch loop uses. `self` is touched only at push/close/handoff and at the run-end sync below. Admission stays
        // per token — batched charging is declined.
        let bytes = source.bytes();
        let mut cursor = self.cursor;
        let mut top_state = self.frames.as_slice().last().map(|frame| frame.state);
        let mut dirty = false;
        let outcome = 'batch: loop {
            if bytes.get(cursor).is_some_and(|byte| {
                crate::byte_scan::is_json_ws(*byte)
                    || (self.grammar.comments && *byte == b'/')
                    || (self.grammar.json5 && matches!(*byte, 0xe2 | 0xef))
            }) {
                if self.grammar.comments || self.grammar.json5 {
                    match self.skip_trivia(source, resources, cursor)? {
                        TriviaSkip::Skipped(consumed) => {
                            cursor += consumed;
                            continue;
                        }
                        TriviaSkip::Yield => break 'batch Ok(false),
                        TriviaSkip::Cut => {
                            break 'batch Err(invalid(
                                source,
                                bytes.len(),
                                diag::EXPECTED_VALUE,
                                diag::MSG_EXPECTED_VALUE,
                            ));
                        }
                        TriviaSkip::Content => {}
                    }
                } else {
                    let Some(granted) = admit_bytes(resources, bytes.len() - cursor)? else {
                        break 'batch Ok(false);
                    };
                    let limit = cursor + granted;
                    cursor += crate::byte_scan::ws_prefix_len(&bytes[cursor..limit]);
                    continue;
                }
            }
            if resources.admit_work_transition()? == jqf_resource::WorkAdmission::Pending {
                break 'batch Ok(false);
            }
            let byte = bytes.get(cursor).copied();
            match top_state {
                Some(VState::ArrayCommaOrEnd) => match byte {
                    Some(b',') => {
                        cursor += 1;
                        top_state = Some(VState::ArrayValueOrEnd {
                            may_end: self.grammar.trailing_commas,
                        });
                        dirty = true;
                        let active = self.frontier_active;
                        let frame = self.frame_last()?;
                        frame.values = frame.values.saturating_add(1);
                        if active {
                            let depth = frame.path_depth;
                            self.frontier.as_mut_slice()[depth] = PathComponent::Index(frame.values);
                        }
                        continue;
                    }
                    Some(b']') => {
                        self.sync_validate_hoist(cursor, top_state, dirty)?;
                        let result = self.close_container(cursor + 1, Container::Array);
                        cursor = self.cursor;
                        dirty = false;
                        break 'batch result;
                    }
                    _ => {
                        break 'batch Err(invalid(source, cursor, diag::EXPECTED_COMMA, diag::MSG_COMMA_OR_ARRAY));
                    }
                },
                Some(VState::ObjectCommaOrEnd) => match byte {
                    Some(b',') => {
                        cursor += 1;
                        top_state = Some(VState::ObjectKeyOrEnd {
                            may_end: self.grammar.trailing_commas,
                        });
                        dirty = true;
                        continue;
                    }
                    Some(b'}') => {
                        self.sync_validate_hoist(cursor, top_state, dirty)?;
                        let result = self.close_container(cursor + 1, Container::Object);
                        cursor = self.cursor;
                        dirty = false;
                        break 'batch result;
                    }
                    _ => {
                        break 'batch Err(invalid(source, cursor, diag::EXPECTED_COMMA, diag::MSG_COMMA_OR_OBJECT));
                    }
                },
                Some(VState::ObjectColon) => {
                    if byte != Some(b':') {
                        break 'batch Err(invalid(source, cursor, diag::EXPECTED_COLON, diag::MSG_COLON));
                    }
                    top_state = Some(VState::ObjectValue);
                    dirty = true;
                    cursor += 1;
                    continue;
                }
                Some(VState::ObjectKeyOrEnd { may_end }) => {
                    if byte == Some(b'}') {
                        if !may_end {
                            break 'batch Err(invalid(source, cursor, diag::TRAILING_COMMA, diag::MSG_TRAILING_COMMA));
                        }
                        self.sync_validate_hoist(cursor, top_state, dirty)?;
                        let result = self.close_container(cursor + 1, Container::Object);
                        cursor = self.cursor;
                        dirty = false;
                        break 'batch result;
                    }
                    if self.grammar.json5 && matches!(byte, Some(b'\'')) {
                        self.sync_validate_hoist(cursor, top_state, dirty)?;
                        self.validate = ValidatePhase::String(VString {
                            target: StrTarget::Key,
                            cursor: cursor + 1,
                            escape: EscapeState::Plain,
                            start: cursor,
                            quote: b'\'',
                        });
                        break 'batch Ok(true);
                    }
                    if self.grammar.json5 && matches!(byte, Some(b'$' | b'_' | b'a'..=b'z' | b'A'..=b'Z')) {
                        let start = cursor;
                        let mut end = cursor + 1;
                        while matches!(
                            bytes.get(end),
                            Some(b'$' | b'_' | b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z')
                        ) {
                            end += 1;
                        }
                        self.locator
                            .on_key(bytes, start, end, self.steps.as_slice(), self.frames.len());
                        if self.frontier_active {
                            let raw = bytes.get(start..end).unwrap_or_default();
                            let lossy = alloc::string::String::from_utf8_lossy(raw);
                            let mut text = alloc::string::String::new();
                            text.try_reserve_exact(lossy.len())
                                .map_err(jqf_resource::ResourceError::from)?;
                            text.push_str(&lossy);
                            let depth = self.frame_last()?.path_depth;
                            self.frontier.as_mut_slice()[depth] = PathComponent::Key(text);
                        }
                        top_state = Some(VState::ObjectColon);
                        dirty = true;
                        cursor = end;
                        continue;
                    }
                    if byte != Some(b'"') {
                        // Same code at the same offset as the whole-document route's key refusal, so the text matches
                        // too — the JSONC hint included.
                        break 'batch Err(invalid(
                            source,
                            cursor,
                            "expected-key",
                            diag::expected_key_message(bytes, cursor),
                        ));
                    }
                    self.sync_validate_hoist(cursor, top_state, dirty)?;
                    self.validate = ValidatePhase::String(VString {
                        target: StrTarget::Key,
                        cursor: cursor + 1,
                        escape: EscapeState::Plain,
                        start: cursor,
                        quote: b'"',
                    });
                    break 'batch Ok(true);
                }
                Some(VState::ArrayValueOrEnd { may_end }) if byte == Some(b']') => {
                    if !may_end {
                        break 'batch Err(invalid(source, cursor, diag::TRAILING_COMMA, diag::MSG_TRAILING_COMMA));
                    }
                    self.sync_validate_hoist(cursor, top_state, dirty)?;
                    let result = self.close_container(cursor + 1, Container::Array);
                    cursor = self.cursor;
                    dirty = false;
                    break 'batch result;
                }
                _ => {}
            }
            match byte {
                Some(b'n') => {
                    if let Some(length) = crate::lex::nonfinite_spelling_len(source, cursor) {
                        if self.complete_validate_scalar(cursor, &mut top_state, &mut dirty, |this| {
                            this.nonfinite_produced(source, cursor + length)
                        })? {
                            cursor = self.cursor;
                            dirty = false;
                            break 'batch Ok(true);
                        }
                        cursor = self.cursor;
                        continue;
                    } else if crate::lex::nonfinite_spelling_truncated(bytes, cursor) {
                        break 'batch Err(invalid(
                            source,
                            bytes.len(),
                            diag::INVALID_LITERAL,
                            diag::MSG_INVALID_LITERAL,
                        ));
                    } else if let Some(length) = crate::lex::nonfinite_spelling_prefix_len(bytes, cursor) {
                        break 'batch Err(invalid(
                            source,
                            cursor + length,
                            diag::INVALID_LITERAL,
                            diag::MSG_INVALID_LITERAL,
                        ));
                    } else if self.complete_validate_scalar(cursor, &mut top_state, &mut dirty, |this| {
                        this.literal(source, cursor, b"null")
                    })? {
                        cursor = self.cursor;
                        dirty = false;
                        break 'batch Ok(true);
                    } else {
                        cursor = self.cursor;
                        continue;
                    }
                }
                Some(b'N' | b'i' | b'I') => {
                    if let Some(length) = crate::lex::nonfinite_spelling_len(source, cursor) {
                        if self.complete_validate_scalar(cursor, &mut top_state, &mut dirty, |this| {
                            this.nonfinite_produced(source, cursor + length)
                        })? {
                            cursor = self.cursor;
                            dirty = false;
                            break 'batch Ok(true);
                        }
                        cursor = self.cursor;
                        continue;
                    } else if crate::lex::nonfinite_spelling_truncated(bytes, cursor) {
                        break 'batch Err(invalid(
                            source,
                            bytes.len(),
                            diag::INVALID_LITERAL,
                            diag::MSG_INVALID_LITERAL,
                        ));
                    } else if let Some(length) = crate::lex::nonfinite_spelling_prefix_len(bytes, cursor) {
                        break 'batch Err(invalid(
                            source,
                            cursor + length,
                            diag::INVALID_LITERAL,
                            diag::MSG_INVALID_LITERAL,
                        ));
                    } else {
                        break 'batch Err(invalid(source, cursor, diag::EXPECTED_VALUE, diag::MSG_EXPECTED_VALUE));
                    }
                }
                Some(b's' | b'S') => {
                    if let Some(length) = crate::lex::nonfinite_spelling_len(source, cursor) {
                        if self.complete_validate_scalar(cursor, &mut top_state, &mut dirty, |this| {
                            this.nonfinite_produced(source, cursor + length)
                        })? {
                            cursor = self.cursor;
                            dirty = false;
                            break 'batch Ok(true);
                        }
                        cursor = self.cursor;
                        continue;
                    } else if crate::lex::nonfinite_spelling_truncated(bytes, cursor) {
                        break 'batch Err(invalid(
                            source,
                            bytes.len(),
                            diag::INVALID_LITERAL,
                            diag::MSG_INVALID_LITERAL,
                        ));
                    } else if let Some(length) = crate::lex::nonfinite_spelling_prefix_len(bytes, cursor) {
                        break 'batch Err(invalid(
                            source,
                            cursor + length,
                            diag::INVALID_LITERAL,
                            diag::MSG_INVALID_LITERAL,
                        ));
                    } else {
                        break 'batch Err(invalid(source, cursor, diag::EXPECTED_VALUE, diag::MSG_EXPECTED_VALUE));
                    }
                }
                Some(b'+') => {
                    if let Some(length) = crate::lex::nonfinite_spelling_len(source, cursor + 1) {
                        if self.complete_validate_scalar(cursor, &mut top_state, &mut dirty, |this| {
                            this.nonfinite_produced(source, cursor + 1 + length)
                        })? {
                            cursor = self.cursor;
                            dirty = false;
                            break 'batch Ok(true);
                        }
                        cursor = self.cursor;
                        continue;
                    } else if crate::lex::nonfinite_spelling_truncated(bytes, cursor + 1) {
                        break 'batch Err(invalid(
                            source,
                            bytes.len(),
                            diag::INVALID_LITERAL,
                            diag::MSG_INVALID_LITERAL,
                        ));
                    } else if let Some(length) = crate::lex::nonfinite_spelling_prefix_len(bytes, cursor + 1) {
                        break 'batch Err(invalid(
                            source,
                            cursor + 1 + length,
                            diag::INVALID_LITERAL,
                            diag::MSG_INVALID_LITERAL,
                        ));
                    } else if (self.grammar.lenient || self.grammar.json5)
                        && matches!(bytes.get(cursor + 1), Some(b'0'..=b'9' | b'.'))
                    {
                        self.sync_validate_hoist(cursor, top_state, dirty)?;
                        self.pending_value_start = cursor;
                        self.validate = ValidatePhase::Number(NumberState::start_at(cursor + 1, true)?);
                        break 'batch Ok(true);
                    } else {
                        break 'batch Err(invalid(source, cursor, diag::EXPECTED_VALUE, diag::MSG_EXPECTED_VALUE));
                    }
                }
                Some(b'.') if self.grammar.lenient || self.grammar.json5 => {
                    if matches!(bytes.get(cursor + 1), Some(b'0'..=b'9')) {
                        self.sync_validate_hoist(cursor, top_state, dirty)?;
                        self.pending_value_start = cursor;
                        self.validate = ValidatePhase::Number(NumberState::start_at(cursor, true)?);
                        break 'batch Ok(true);
                    }
                    break 'batch Err(invalid(source, cursor, diag::EXPECTED_VALUE, diag::MSG_EXPECTED_VALUE));
                }
                Some(b't') => {
                    if self.complete_validate_scalar(cursor, &mut top_state, &mut dirty, |this| {
                        this.literal(source, cursor, b"true")
                    })? {
                        cursor = self.cursor;
                        dirty = false;
                        break 'batch Ok(true);
                    }
                    cursor = self.cursor;
                    continue;
                }
                Some(b'f') => {
                    if self.complete_validate_scalar(cursor, &mut top_state, &mut dirty, |this| {
                        this.literal(source, cursor, b"false")
                    })? {
                        cursor = self.cursor;
                        dirty = false;
                        break 'batch Ok(true);
                    }
                    cursor = self.cursor;
                    continue;
                }
                Some(b'"') => {
                    self.sync_validate_hoist(cursor, top_state, dirty)?;
                    self.pending_value_start = cursor;
                    self.validate = ValidatePhase::String(VString {
                        target: StrTarget::Value,
                        cursor: cursor + 1,
                        escape: EscapeState::Plain,
                        start: cursor,
                        quote: b'"',
                    });
                    break 'batch Ok(true);
                }
                Some(b'\'') if self.grammar.json5 => {
                    self.sync_validate_hoist(cursor, top_state, dirty)?;
                    self.pending_value_start = cursor;
                    self.validate = ValidatePhase::String(VString {
                        target: StrTarget::Value,
                        cursor: cursor + 1,
                        escape: EscapeState::Plain,
                        start: cursor,
                        quote: b'\'',
                    });
                    break 'batch Ok(true);
                }
                Some(b'[') => {
                    self.sync_validate_hoist(cursor, top_state, dirty)?;
                    self.open_container(cursor + 1, Container::Array, resources)?;
                    cursor = self.cursor;
                    top_state = self.frames.as_slice().last().map(|frame| frame.state);
                    dirty = false;
                    continue;
                }
                Some(b'{') => {
                    self.sync_validate_hoist(cursor, top_state, dirty)?;
                    self.open_container(cursor + 1, Container::Object, resources)?;
                    cursor = self.cursor;
                    top_state = self.frames.as_slice().last().map(|frame| frame.state);
                    dirty = false;
                    continue;
                }
                Some(b'-') => {
                    if let Some(length) = crate::lex::nonfinite_spelling_len(source, cursor + 1) {
                        if self.complete_validate_scalar(cursor, &mut top_state, &mut dirty, |this| {
                            this.nonfinite_produced(source, cursor + 1 + length)
                        })? {
                            cursor = self.cursor;
                            dirty = false;
                            break 'batch Ok(true);
                        }
                        cursor = self.cursor;
                        continue;
                    } else if crate::lex::nonfinite_spelling_truncated(bytes, cursor + 1) {
                        break 'batch Err(invalid(
                            source,
                            bytes.len(),
                            diag::INVALID_LITERAL,
                            diag::MSG_INVALID_LITERAL,
                        ));
                    } else if let Some(length) = crate::lex::nonfinite_spelling_prefix_len(bytes, cursor + 1) {
                        break 'batch Err(invalid(
                            source,
                            cursor + 1 + length,
                            diag::INVALID_LITERAL,
                            diag::MSG_INVALID_LITERAL,
                        ));
                    } else {
                        self.sync_validate_hoist(cursor, top_state, dirty)?;
                        self.pending_value_start = cursor;
                        self.validate = ValidatePhase::Number(NumberState::start_at(cursor, false)?);
                        break 'batch Ok(true);
                    }
                }
                Some(b'0'..=b'9') => {
                    self.sync_validate_hoist(cursor, top_state, dirty)?;
                    self.pending_value_start = cursor;
                    self.validate = ValidatePhase::Number(NumberState::start_at(cursor, false)?);
                    break 'batch Ok(true);
                }
                _ => {
                    // Same code at the same offset as the whole-document route's value refusal, so the text matches too
                    // — the JSONC hint included.
                    break 'batch Err(invalid(
                        source,
                        cursor,
                        diag::EXPECTED_VALUE,
                        diag::expected_value_message(bytes, cursor),
                    ));
                }
            }
        };
        self.sync_validate_hoist(cursor, top_state, dirty)?;
        outcome
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one string-walk state machine; the key-capture and locate hooks share the close"
    )]
    fn validate_string(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        let phase = mem::replace(&mut self.validate, ValidatePhase::Value);
        let ValidatePhase::String(mut state) = phase else {
            unreachable!()
        };
        let bytes = source.bytes();
        let remaining = bytes.len().saturating_sub(state.cursor);
        let Some(granted) = admit_bytes(resources, remaining)? else {
            self.validate = ValidatePhase::String(state);
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
        let limit = state.cursor + granted;
        let quote = state.quote;
        while state.cursor < limit {
            let byte = bytes[state.cursor];
            if let EscapeState::Plain = state.escape {
                let run_end = if quote == b'\'' {
                    single_quoted_run_end(bytes, state.cursor, limit)
                } else {
                    plain_string_run_end(bytes, state.cursor, limit)
                };
                if run_end != state.cursor {
                    state.cursor = run_end;
                    continue;
                }
                match byte {
                    b if b == quote => {
                        state.cursor += 1;
                        self.cursor = state.cursor;
                        if state.target == StrTarget::Key {
                            self.locator.on_key(
                                bytes,
                                state.start.saturating_add(1),
                                state.cursor.saturating_sub(1),
                                self.steps.as_slice(),
                                self.frames.len(),
                            );
                        }
                        if state.target == StrTarget::Key && self.frontier_active {
                            // The key text (raw, escapes included) is the frontier path's object step. The key's length
                            // is the input's to choose, so the copy reserves fallibly rather than aborting on a huge
                            // one.
                            let raw = bytes
                                .get(state.start.saturating_add(1)..state.cursor.saturating_sub(1))
                                .unwrap_or_default();
                            let lossy = alloc::string::String::from_utf8_lossy(raw);
                            let mut text = alloc::string::String::new();
                            text.try_reserve_exact(lossy.len())
                                .map_err(jqf_resource::ResourceError::from)?;
                            text.push_str(&lossy);
                            let depth = self.frame_last()?.path_depth;
                            self.frontier.as_mut_slice()[depth] = PathComponent::Key(text);
                        }
                        return self.finish_string(state.target);
                    }
                    b'\\' => {
                        state.cursor += 1;
                        state.escape = EscapeState::Escape;
                    }
                    0x00..=0x1f => {
                        return Err(string_fault(
                            source,
                            state.cursor,
                            "control-in-string",
                            "unescaped control byte in string",
                        ));
                    }
                    _ => {
                        let Some(width) = scalar_width_from_lead(byte) else {
                            return Err(string_fault(
                                source,
                                state.cursor,
                                diag::INVALID_UTF8,
                                diag::MSG_INVALID_UTF8,
                            ));
                        };
                        if state.cursor + width > limit {
                            if limit == bytes.len() {
                                // The scalar needs bytes past the last one the decoder was given. Report the truncation
                                // at the cut, not at the lead: end of input and end of a still-growing window are the
                                // same sight from here, and the cut is the position a streaming caller reads as "would
                                // complete with more input".
                                return Err(string_fault(
                                    source,
                                    bytes.len(),
                                    diag::INVALID_UTF8,
                                    diag::MSG_INVALID_UTF8,
                                ));
                            }
                            break;
                        }
                        // With the pre-scan active, every high byte is already proven valid UTF-8, so advancing by the
                        // width table is sufficient; adjacent-value mode skips the pre-scan and keeps the authoritative
                        // per-scalar check.
                        if !self.utf8_pre_scan
                            && core::str::from_utf8(&bytes[state.cursor..state.cursor + width]).is_err()
                        {
                            return Err(string_fault(
                                source,
                                state.cursor,
                                diag::INVALID_UTF8,
                                diag::MSG_INVALID_UTF8,
                            ));
                        }
                        state.cursor += width;
                    }
                }
            } else if matches!(state.escape, EscapeState::Escape) && self.grammar.json5 {
                match byte {
                    b'\'' | b'\n' => {
                        state.cursor += 1;
                        state.escape = EscapeState::Plain;
                    }
                    b'\r' => {
                        state.cursor += 1;
                        if bytes.get(state.cursor) == Some(&b'\n') {
                            state.cursor += 1;
                        }
                        state.escape = EscapeState::Plain;
                    }
                    b'x' => {
                        state.cursor += 1;
                        state.escape = EscapeState::Hex { value: 0, digits: 0 };
                    }
                    b'0' => {
                        if matches!(bytes.get(state.cursor + 1), Some(b'0'..=b'9')) {
                            return Err(invalid(
                                source,
                                state.cursor,
                                diag::INVALID_ESCAPE,
                                diag::MSG_INVALID_ESCAPE_JSON5,
                            ));
                        }
                        state.cursor += 1;
                        state.escape = EscapeState::Plain;
                    }
                    0xe2 if matches!(
                        bytes.get(state.cursor + 1..state.cursor + 3),
                        Some(&[0x80, 0xa8 | 0xa9])
                    ) =>
                    {
                        state.cursor += 3;
                        state.escape = EscapeState::Plain;
                    }
                    _ => {
                        state.cursor += 1;
                        state.escape = escape_step(state.escape, byte, source, state.cursor - 1, true)?;
                    }
                }
            } else {
                state.cursor += 1;
                state.escape = escape_step(state.escape, byte, source, state.cursor - 1, self.grammar.json5)?;
            }
        }
        self.validate = ValidatePhase::String(state);
        Ok(true)
    }

    fn validate_number(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        // The number automaton's state is hoisted OUT of `self.validate` into a local for the whole byte loop: the
        // phase variant is a 100+ byte struct, and a loop that read/wrote the state through that path made LLVM spill
        // narrow fields (the one-byte `lex` discriminator, bools) and reload them wide at the loop top — a
        // narrow-store→wide-load pair that does not store-forward in the emulated container. The state is written
        // back only at the window boundary, so the loop-carried cursor and lexeme live in registers.
        let phase = mem::replace(&mut self.validate, ValidatePhase::Value);
        let ValidatePhase::Number(mut state) = phase else {
            unreachable!()
        };
        let bytes = source.bytes();
        let remaining = bytes.len().saturating_sub(state.cursor_usize());
        let Some(granted) = admit_bytes(resources, remaining)? else {
            self.validate = ValidatePhase::Number(state);
            return Ok(false);
        };
        let limit = state.cursor_usize() + granted;
        // Decode leniency: the scoped validator must accept exactly what the whole parser accepts, so the same shared
        // matcher runs with the same dial.
        let lenient = self.grammar.lenient;
        while state.cursor_usize() < limit {
            // Long-number digit-run skip, mirroring the whole parser: the `Integer`/`Fraction` lexemes consume runs of
            // `0-9` with digit bookkeeping only, so the run is folded in bulk and the state machine is re-entered at
            // the first non-digit. `Zero` is NOT skipped — a digit there is the leading-zero error, whose position
            // must not move.
            if matches!(state.lex, NumberLex::Integer | NumberLex::Fraction) {
                let fractional = matches!(state.lex, NumberLex::Fraction);
                let start = state.cursor_usize();
                consume_digit_run(&mut state, &bytes[start..limit], fractional)?;
                if state.cursor_usize() >= limit {
                    break;
                }
            }
            let byte = bytes[state.cursor_usize()];
            let consumed = match advance_number(&mut state, byte, lenient, self.grammar.json5) {
                Ok(consumed) => consumed,
                Err(error) if error.kind() == CodecFailureKind::InvalidInput => {
                    // The one number shape with a dedicated message, mirroring the whole parser byte for byte: a digit
                    // after a leading zero (`00`, `01`, `007`) is illegal under RFC 8259, but the reference's lenient
                    // reader accepts it. Name the divergence so the scoped validator and the whole parser agree on
                    // stderr bytes.
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
                return self.finish_number(source, state, lenient);
            }
            state.cursor = state
                .cursor
                .checked_add(1)
                .ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?;
        }
        if state.cursor_usize() == bytes.len() {
            // No delimiter ended this number — the bytes did. It is complete as it stands and published as it stands,
            // but a digit, a `.`, or an exponent arriving next would have made it a DIFFERENT number.
            self.open_ended = true;
            return self.finish_number(source, state, lenient);
        }
        self.validate = ValidatePhase::Number(state);
        Ok(true)
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "the state left the phase variant for the byte loop; it is owned by the caller and dropped here, which is what makes the phase transition exact"
    )]
    fn finish_number(
        &mut self,
        source: ResolvedSource<'_>,
        state: NumberState,
        lenient: bool,
    ) -> Result<bool, CodecError> {
        // The caller already moved the state out of `self.validate`; the phase is back to Value before the completion
        // checks, exactly as the pre-hoist `mem::replace` at the top of this function did.
        self.validate = ValidatePhase::Value;
        if !number_lex_complete(state.lex, lenient, self.grammar.json5, state.digit_count as usize) {
            return Err(invalid(
                source,
                state.cursor_usize(),
                diag::INVALID_NUMBER,
                "incomplete JSON number",
            ));
        }
        // The scale refusal is the one place the validator and the builder disagree under leniency: the builder clamps,
        // the validator only has to accept what the builder accepts.
        if !lenient && number_finish_plan(&state, false)?.is_none() {
            return Err(unsupported_number(source, state.start_usize(), state.cursor_usize()));
        }
        // The hex cap is parity, not leniency: the builder refuses a literal past the exact-conversion bound on every
        // dial, so the validator refuses it here too — a scoped validation of bytes the whole route would reject
        // would be an accepted-surface divergence.
        //
        if matches!(state.lex, NumberLex::Hex)
            && (state.digit_count as usize).saturating_sub(1) > crate::lex::MAX_HEX_DIGITS
        {
            return Err(crate::error::unsupported_hex_number(
                source,
                state.start_usize() + 2 + usize::from(state.negative),
                state.cursor_usize(),
            ));
        }
        self.cursor = state.cursor_usize();
        self.value_produced(ValueKind::Number)
    }

    fn finish_string(&mut self, target: StrTarget) -> Result<bool, CodecError> {
        match target {
            StrTarget::Key => {
                self.frame_last()?.state = VState::ObjectColon;
                Ok(true)
            }
            StrTarget::Value => self.value_produced(ValueKind::String),
        }
    }

    fn literal(&mut self, source: ResolvedSource<'_>, offset: usize, spelling: &[u8]) -> Result<bool, CodecError> {
        if source.bytes().get(offset..offset + spelling.len()) != Some(spelling) {
            // The whole parser's cut law: a spelling cut off by the window's end is an incomplete token, labeled at the
            // end so the streaming drive can hold; a wrong byte inside the window keeps its own position.
            let at =
                if source.bytes().len() < offset + spelling.len() && spelling.starts_with(&source.bytes()[offset..]) {
                    source.bytes().len()
                } else {
                    offset
                };
            return Err(invalid(source, at, diag::INVALID_LITERAL, diag::MSG_INVALID_LITERAL));
        }
        // Mirror the whole parser: a bare-word suffix after a literal (`truex`, `nullx`) is one malformed token,
        // rejected before the value is produced.
        if let Some(&next) = source.bytes().get(offset + spelling.len())
            && !is_value_boundary(next)
        {
            return Err(invalid(
                source,
                offset + spelling.len(),
                diag::INVALID_LITERAL,
                diag::MSG_INVALID_LITERAL,
            ));
        }
        // No byte followed the spelling, so no delimiter closed it — the bytes ran out. The token is complete and
        // published, but the next byte would have decided between this literal and a malformed one.
        if offset + spelling.len() == source.bytes().len() {
            self.open_ended = true;
        }
        self.cursor = offset + spelling.len();
        let kind = match spelling {
            b"true" | b"false" => ValueKind::Bool,
            _ => ValueKind::Null,
        };
        self.value_produced(kind)
    }

    /// Accepts a complete non-finite spelling ending at `end`. `inf` is a proper prefix of `infinity`, so a spelling
    /// that runs to the last byte the validator was given is open-ended for the same reason a bare number is: no
    /// delimiter closed it.
    fn nonfinite_produced(&mut self, source: ResolvedSource<'_>, end: usize) -> Result<bool, CodecError> {
        if end == source.bytes().len() {
            self.open_ended = true;
        }
        self.cursor = end;
        self.value_produced(ValueKind::Number)
    }

    fn open_container(
        &mut self,
        next: usize,
        container: Container,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        self.flush_pending_comments();
        // Mirror the whole parser: the container value is "attached" to its parent before its own frame is pushed.
        self.attach_to_parent()?;
        let depth = resources.enter_nesting_owned()?;
        let state = match container {
            Container::Object => VState::ObjectKeyOrEnd { may_end: true },
            Container::Array => VState::ArrayValueOrEnd { may_end: true },
        };
        let path_depth = self.frontier.len();
        if self.frontier_active {
            // One entry per open container: the nesting limit bounds it, but the growth is still the input's to drive,
            // so it is fallible.
            self.frontier
                .try_reserve(1)
                .map_err(jqf_resource::ResourceError::from)?;
            self.frontier.push(match container {
                Container::Array => PathComponent::Index(0),
                Container::Object => PathComponent::Key(alloc::string::String::new()),
            });
        }
        let start = next.saturating_sub(1);
        let scanning = self
            .locator
            .on_container_open(start, container, self.steps.as_slice(), self.frames.len());
        self.frames.push(VFrame {
            state,
            _depth: depth,
            path_depth,
            values: 0,
            start,
            scanning,
        });
        self.cursor = next;
        Ok(true)
    }

    fn close_container(&mut self, next: usize, container: Container) -> Result<bool, CodecError> {
        let frame = self.frames.pop().ok_or_else(data_contract)?;
        while self.frontier.len() > frame.path_depth {
            self.frontier.pop();
        }
        let is_object = matches!(frame.state, VState::ObjectKeyOrEnd { .. } | VState::ObjectCommaOrEnd);
        if (container == Container::Object) != is_object {
            return Err(data_contract());
        }
        self.locator.finish_container(
            frame.start,
            next,
            frame.scanning,
            self.steps.as_slice(),
            self.frames.len(),
        )?;
        self.cursor = next;
        if self.frames.is_empty() {
            self.validate = ValidatePhase::Trailing;
        }
        Ok(true)
    }

    /// Transitions the parent frame to mark that its pending slot is now filled, mirroring `JsonParseState::attach`.
    fn attach_to_parent(&mut self) -> Result<(), CodecError> {
        let Some(index) = self.frames.len().checked_sub(1) else {
            if self.root_seen {
                return Err(data_contract());
            }
            self.root_seen = true;
            return Ok(());
        };
        let state = mem::replace(&mut self.frames.as_mut_slice()[index].state, VState::ArrayCommaOrEnd);
        match state {
            VState::ArrayValueOrEnd { .. } => {
                self.frames.as_mut_slice()[index].state = VState::ArrayCommaOrEnd;
                Ok(())
            }
            VState::ObjectValue => {
                self.frames.as_mut_slice()[index].state = VState::ObjectCommaOrEnd;
                Ok(())
            }
            other => {
                self.frames.as_mut_slice()[index].state = other;
                Err(data_contract())
            }
        }
    }

    /// Handles a completed scalar/string/number value: attach to parent, and move to trailing when the root value is
    /// complete.
    fn value_produced(&mut self, kind: ValueKind) -> Result<bool, CodecError> {
        self.flush_pending_comments();
        self.locator.finish_scalar(
            self.pending_value_start,
            self.cursor,
            kind,
            self.steps.as_slice(),
            self.frames.len(),
        );
        self.attach_to_parent()?;
        if self.frames.is_empty() {
            self.validate = ValidatePhase::Trailing;
        }
        Ok(true)
    }

    fn validate_trailing(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        let offset = self.cursor;
        if self.allow_adjacent_values {
            // Mirrors `JsonParseState::trailing_step`'s opt-in: the root value is grammatically complete at `offset`;
            // record it and stop validating without inspecting what follows.
            self.consumed_offset
                .get_or_insert(u64::try_from(offset).unwrap_or(u64::MAX));
            self.validate = ValidatePhase::Done;
            return Ok(true);
        }
        let lead = source.bytes().get(offset);
        if lead.is_some_and(|byte| {
            crate::byte_scan::is_json_ws(*byte)
                || (self.grammar.comments && *byte == b'/')
                || (self.grammar.json5 && matches!(*byte, 0xe2 | 0xef))
        }) {
            if self.grammar.comments || self.grammar.json5 {
                match self.skip_trivia(source, resources, offset)? {
                    TriviaSkip::Skipped(consumed) => {
                        self.cursor = offset + consumed;
                        return Ok(true);
                    }
                    TriviaSkip::Yield => return Ok(false),
                    TriviaSkip::Cut => {
                        return Err(invalid(
                            source,
                            source.bytes().len(),
                            diag::TRAILING_CONTENT,
                            diag::MSG_TRAILING_CONTENT,
                        ));
                    }
                    TriviaSkip::Content => {}
                }
            } else {
                let Some(granted) = admit_bytes(resources, source.bytes().len() - offset)? else {
                    return Ok(false);
                };
                let limit = offset + granted;
                let cursor = offset + crate::byte_scan::ws_prefix_len(&source.bytes()[offset..limit]);
                self.cursor = cursor;
                return Ok(true);
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
        self.validate = ValidatePhase::Done;
        Ok(true)
    }

    fn last_step_candidate(&self) -> bool {
        let Some(frame) = self.locator.stack.last() else {
            return false;
        };
        if self.frames.len() != self.locator.stack.len() || frame.step + 1 != self.steps.len() {
            return false;
        }
        match self.steps.get(frame.step) {
            Some(ScopedStep::Member(_)) => frame.key_matches,
            Some(ScopedStep::Index(index)) if *index >= 0 => {
                usize::try_from(*index).is_ok_and(|target| frame.count == target)
            }
            // Negative-index and range winners resolve after the array closes.
            // Seeding here would attach the last element's comments.
            Some(ScopedStep::Index(_) | ScopedStep::Range { .. }) | None => false,
        }
    }

    fn flush_pending_comments(&mut self) {
        if self.last_step_candidate() {
            self.located_comments = mem::take(&mut self.pending_comments);
        } else {
            self.pending_comments.clear();
        }
    }

    fn skip_trivia(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
        at: usize,
    ) -> Result<TriviaSkip, CodecError> {
        let bytes = source.bytes();
        if at >= bytes.len() {
            return Ok(TriviaSkip::Content);
        }
        let mut window = mem::take(&mut self.trivia_window);
        loop {
            let Some(granted) = admit_bytes(resources, bytes.len() - at)? else {
                self.trivia_window = window;
                return Ok(TriviaSkip::Yield);
            };
            window += granted;
            let limit = (at + window).min(bytes.len());
            let collect = self.grammar.comments && self.coverage.attached_facts();
            let mut comments = collect.then(Vec::new);
            let pending_base = self.pending_comments.len();
            let (consumed, open_comment) =
                crate::byte_scan::trivia_prefix_len(&bytes[at..limit], self.grammar, comments.as_mut())?;
            if let Some(comments) = comments {
                self.pending_comments
                    .try_reserve(comments.len())
                    .map_err(jqf_resource::ResourceError::from)?;
                self.pending_comments.extend(comments);
            }
            if open_comment && limit < bytes.len() {
                self.pending_comments.truncate(pending_base);
                continue;
            }
            self.trivia_window = 0;
            if consumed > 0 {
                return Ok(TriviaSkip::Skipped(consumed));
            }
            let retry =
                matches!(bytes[at], b'/' if self.grammar.comments) && matches!(bytes.get(at + 1), Some(b'/' | b'*'));
            if retry {
                continue;
            }
            match bytes.get(at).copied() {
                Some(b'/') if self.grammar.comments => match bytes.get(at + 1) {
                    None => return Ok(TriviaSkip::Cut),
                    Some(b'/' | b'*') => {}
                    Some(_) => return Ok(TriviaSkip::Content),
                },
                Some(b'\xe2' | b'\xef') if self.grammar.json5 => {
                    if bytes.len() - at < 3 {
                        return Ok(TriviaSkip::Cut);
                    }
                    return Ok(TriviaSkip::Content);
                }
                _ => return Ok(TriviaSkip::Content),
            }
        }
    }

    fn frame_last(&mut self) -> Result<&mut VFrame, CodecError> {
        self.frames.as_mut_slice().last_mut().ok_or_else(data_contract)
    }

    /// Writes the hoisted cursor back, and the top state back only when it diverged from its Vec slot.
    fn sync_validate_hoist(&mut self, cursor: usize, top_state: Option<VState>, dirty: bool) -> Result<(), CodecError> {
        self.cursor = cursor;
        if dirty && let Some(state) = top_state {
            self.frame_last()?.state = state;
        }
        Ok(())
    }

    /// Syncs the hoist, runs a completing scalar (`literal`/`nonfinite`), then re-hoists. Returns `true` when the root
    /// value finished (phase is Trailing) so the batch must stop.
    fn complete_validate_scalar(
        &mut self,
        cursor: usize,
        top_state: &mut Option<VState>,
        dirty: &mut bool,
        complete: impl FnOnce(&mut Self) -> Result<bool, CodecError>,
    ) -> Result<bool, CodecError> {
        self.sync_validate_hoist(cursor, *top_state, *dirty)?;
        self.pending_value_start = cursor;
        complete(self)?;
        *top_state = self.frames.as_slice().last().map(|frame| frame.state);
        *dirty = false;
        Ok(matches!(self.validate, ValidatePhase::Trailing))
    }
}

fn step_accepts(step: &ScopedStep, container: Container) -> bool {
    matches!(
        (step, container),
        (ScopedStep::Member(_), Container::Object)
            | (ScopedStep::Index(_) | ScopedStep::Range { .. }, Container::Array)
    )
}

fn container_kind(container: Container) -> ValueKind {
    match container {
        Container::Object => ValueKind::Object,
        Container::Array => ValueKind::Array,
    }
}

impl PathLocator {
    const fn new() -> Self {
        Self {
            result: None,
            stack: Vec::new(),
            pending_mismatch: None,
        }
    }

    fn take(&mut self) -> Result<Located, CodecError> {
        self.result.take().ok_or_else(data_contract)
    }

    fn on_key(&mut self, bytes: &[u8], start: usize, end: usize, steps: &[ScopedStep], frame_depth: usize) {
        if self.result.is_some() || frame_depth != self.stack.len() {
            return;
        }
        let Some(frame) = self.stack.last_mut() else {
            return;
        };
        let Some(ScopedStep::Member(name)) = steps.get(frame.step) else {
            return;
        };
        frame.key_matches = key_equals_inner(bytes, start, end, name);
    }

    fn on_container_open(
        &mut self,
        start: usize,
        container: Container,
        steps: &[ScopedStep],
        frame_depth: usize,
    ) -> bool {
        if self.result.is_some() {
            return false;
        }
        let kind = container_kind(container);
        if self.stack.is_empty() {
            if steps.is_empty() {
                return false;
            }
            if !step_accepts(&steps[0], container) {
                self.result = Some(Located::TypeMismatch { step: 0, actual: kind });
                return false;
            }
            self.stack.push(LocateFrame::new(0, start, container, &steps[0]));
            return true;
        }
        if frame_depth != self.stack.len() {
            return false;
        }
        let Some(frame) = self.stack.last() else {
            return false;
        };
        if matches!(steps.get(frame.step), Some(ScopedStep::Range { .. })) {
            return false;
        }
        if !Self::child_is_candidate(frame, steps) {
            return false;
        }
        let next = frame.step + 1;
        if next >= steps.len() {
            return false;
        }
        if !step_accepts(&steps[next], container) {
            self.pending_mismatch = Some(Located::TypeMismatch {
                step: next,
                actual: kind,
            });
            return false;
        }
        self.stack.push(LocateFrame::new(next, start, container, &steps[next]));
        true
    }

    fn finish_container(
        &mut self,
        start: usize,
        end: usize,
        scanning: bool,
        steps: &[ScopedStep],
        frame_depth: usize,
    ) -> Result<(), CodecError> {
        if scanning {
            let located = self.pop_and_resolve(steps)?;
            if self.stack.is_empty() {
                self.result = Some(located);
                return Ok(());
            }
            self.record_child(start, end, Some(located), steps);
            return Ok(());
        }
        if self.stack.is_empty() {
            // Empty path: only the ROOT close is the located node. An inner container closes first and must not steal
            // the observation.
            if self.result.is_none() && frame_depth == 0 {
                self.result = Some(Located::Node { start, end });
            }
            return Ok(());
        }
        if frame_depth != self.stack.len() {
            return Ok(());
        }
        let nested = self.pending_mismatch.take();
        self.record_child(start, end, nested, steps);
        Ok(())
    }

    fn finish_scalar(&mut self, start: usize, end: usize, kind: ValueKind, steps: &[ScopedStep], frame_depth: usize) {
        if self.result.is_some() {
            return;
        }
        if !self.stack.is_empty() && frame_depth != self.stack.len() {
            return;
        }
        if self.stack.is_empty() {
            // Root only. A nested scalar inside an empty-path document is not the located node — the root container
            // close is.
            if frame_depth == 0 {
                self.result = Some(if steps.is_empty() {
                    Located::Node { start, end }
                } else {
                    Located::TypeMismatch { step: 0, actual: kind }
                });
            }
            return;
        }
        let Some(frame) = self.stack.last() else {
            return;
        };
        // A Range step needs no special case here: `child_is_candidate` is always true for it, and `record_child`'s
        // Range arm ignores the nested selection — falling through lands in the same call.
        if !Self::child_is_candidate(frame, steps) {
            self.record_child(start, end, None, steps);
            return;
        }
        let next = frame.step + 1;
        // A path-exhausted candidate carries no nested selection. The container-open arm is the only producer of
        // `pending_mismatch`, and the same container's close consumes it before any same-depth scalar can run, so there
        // is nothing to take on the scalar path.
        let nested = (next < steps.len()).then_some(Located::TypeMismatch {
            step: next,
            actual: kind,
        });
        self.record_child(start, end, nested, steps);
    }

    fn child_is_candidate(frame: &LocateFrame, steps: &[ScopedStep]) -> bool {
        match steps.get(frame.step) {
            Some(ScopedStep::Member(_)) => frame.key_matches,
            Some(ScopedStep::Index(index)) if *index >= 0 => {
                usize::try_from(*index).is_ok_and(|target| frame.count == target)
            }
            Some(ScopedStep::Index(_) | ScopedStep::Range { .. }) => true,
            None => false,
        }
    }

    fn record_child(&mut self, start: usize, end: usize, nested: Option<Located>, steps: &[ScopedStep]) {
        let Some(frame) = self.stack.last_mut() else {
            return;
        };
        let candidate = Self::child_is_candidate(frame, steps);
        if candidate {
            let span = (start, end);
            match steps.get(frame.step) {
                Some(ScopedStep::Index(index)) if *index < 0 => {
                    // The ring is the ONLY thing a negative-index pop reads (`pop_and_resolve`); winner/nested stores
                    // for this arm would be dead.
                    if let Some(ring) = frame.ring.as_mut() {
                        ring.push(span, nested);
                    }
                }
                Some(ScopedStep::Range { .. }) => {
                    if let Some(range) = frame.range.as_mut() {
                        range.observe(frame.count, start, end);
                    }
                }
                Some(ScopedStep::Member(_) | ScopedStep::Index(_)) => {
                    frame.winner = Some(span);
                    frame.nested = nested;
                }
                None => {}
            }
        }
        frame.count = frame.count.saturating_add(1);
        frame.key_matches = false;
    }

    fn pop_and_resolve(&mut self, steps: &[ScopedStep]) -> Result<Located, CodecError> {
        let frame = self.stack.pop().ok_or_else(data_contract)?;
        if let Some(range) = frame.range {
            return Ok(range.finish(frame.count));
        }
        if matches!(steps.get(frame.step), Some(ScopedStep::Index(index)) if *index < 0) {
            if let Some(nested) = frame.ring.as_ref().and_then(Ring::winner_nested) {
                return Ok(nested);
            }
            return Ok(match frame.ring.as_ref().and_then(Ring::winner) {
                Some((start, end)) => Located::Node { start, end },
                None => Located::Missing { step: frame.step },
            });
        }
        if let Some(nested) = frame.nested {
            return Ok(nested);
        }
        Ok(match frame.winner {
            Some((start, end)) => Located::Node { start, end },
            None => Located::Missing { step: frame.step },
        })
    }
}

impl LocateFrame {
    fn new(step: usize, _start: usize, _container: Container, spec: &ScopedStep) -> Self {
        let ring = match spec {
            ScopedStep::Index(index) if *index < 0 => {
                Some(Ring::new(usize::try_from(index.unsigned_abs()).unwrap_or(usize::MAX)))
            }
            _ => None,
        };
        let range = match spec {
            ScopedStep::Range { start, end } => Some(RangeTrack::try_new(*start, *end)),
            _ => None,
        };
        Self {
            step,
            winner: None,
            nested: None,
            count: 0,
            ring,
            range,
            key_matches: false,
        }
    }
}

impl RangeTrack {
    fn try_new(start: Option<i64>, end: Option<i64>) -> Self {
        let pending = matches!(start, Some(value) if value < 0) || matches!(end, Some(value) if value < 0);
        if !pending {
            return Self {
                start: resolve_non_negative(start).unwrap_or(0),
                end: resolve_non_negative(end),
                pending: None,
                region_start: None,
                region_end: 0,
                from_kind: RangeEndpoint::Head,
                to_kind: RangeEndpoint::Tail,
                from_span: None,
                to_span: None,
                first_start: None,
                last_end: None,
                tail: None,
            };
        }
        // The two endpoints `finish` needs, each known from the authored bounds alone — a negative bound's look-back
        // does not depend on the container's length.
        let from_kind = match start {
            None => RangeEndpoint::Head,
            Some(value) if value >= 0 => RangeEndpoint::Fixed(usize::try_from(value).unwrap_or(usize::MAX)),
            // A look-back past usize::MAX is unreachable on 32-bit targets (the container itself could not be that
            // long); saturate.
            Some(value) => RangeEndpoint::Back(usize::try_from(value.unsigned_abs()).unwrap_or(usize::MAX)),
        };
        let to_kind = match end {
            None => RangeEndpoint::Tail,
            // `end == 0` empties the window before any endpoint is read; the impossible index keeps `observe` harmless.
            Some(0) => RangeEndpoint::Fixed(usize::MAX),
            Some(value) if value > 0 => RangeEndpoint::Fixed(usize::try_from(value).unwrap_or(usize::MAX) - 1),
            Some(value) => RangeEndpoint::Back(match usize::try_from(value.unsigned_abs()) {
                Ok(distance) => distance.saturating_add(1),
                Err(_) => usize::MAX,
            }),
        };
        let look_back = [from_kind, to_kind]
            .into_iter()
            .filter_map(|kind| match kind {
                RangeEndpoint::Back(distance) => Some(distance),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        Self {
            start: 0,
            end: None,
            pending: Some((start, end)),
            region_start: None,
            region_end: 0,
            from_kind,
            to_kind,
            from_span: None,
            to_span: None,
            first_start: None,
            last_end: None,
            tail: (look_back > 0).then(|| TailSpans::new(look_back)),
        }
    }

    fn observe(&mut self, index: usize, start: usize, end: usize) {
        if self.pending.is_some() {
            match self.from_kind {
                RangeEndpoint::Fixed(fixed) if index == fixed => {
                    self.from_span = Some((start, end));
                }
                RangeEndpoint::Head => {
                    if self.first_start.is_none() {
                        self.first_start = Some(start);
                    }
                }
                RangeEndpoint::Fixed(_) | RangeEndpoint::Tail | RangeEndpoint::Back(_) => {}
            }
            // The last observed element end is tracked UNCONDITIONALLY: a positive end bound clamped past the
            // container's length never sees its fixed index (`[0:20]` over 3 elements), and `finish` then runs the
            // window to the last element exactly as the floor's slice clamp does — but only when the fixed index is
            // at or past the count, so a genuine missed capture below the count still fails closed.
            self.last_end = Some(end);
            match self.to_kind {
                RangeEndpoint::Fixed(fixed) if index == fixed => self.to_span = Some((start, end)),
                RangeEndpoint::Fixed(_) | RangeEndpoint::Tail | RangeEndpoint::Head | RangeEndpoint::Back(_) => {}
            }
            if let Some(tail) = &mut self.tail {
                tail.push((start, end));
            }
            return;
        }
        if self.end.is_some_and(|bound| index >= bound) {
            return;
        }
        if index >= self.start && self.region_start.is_none() {
            self.region_start = Some(start);
        }
        if index >= self.start {
            self.region_end = end;
        }
    }

    fn finish(self, count: usize) -> Located {
        if let Some((start, end)) = self.pending {
            let from = resolve_against_count(start, count).unwrap_or(0);
            let to = resolve_against_count(end, count);
            if to.is_some_and(|bound| bound <= from) {
                return Located::Range { start: 0, end: 0 };
            }
            let to = to.unwrap_or(count).min(count);
            let from = from.min(count);
            if from >= to {
                return Located::Range { start: 0, end: 0 };
            }
            // Endpoint resolution: every kind either captured its span during the walk or reads the tail ring, whose
            // capacity covers the largest look-back the bounds can name. The one legitimate missing capture is a
            // POSITIVE end bound clamped past the container's length — its fixed index never occurs, and the window
            // ends at the last observed element (the same clamp the floor's slice applies). A fixed end index BELOW the
            // count must have been observed, so its absence stays a contract break answering empty; `last_end` is None
            // only when no element was observed at all, which answers empty the same way.
            let first_start = match self.from_kind {
                RangeEndpoint::Fixed(_) => self.from_span.map(|(start, _)| start),
                RangeEndpoint::Head => self.first_start,
                RangeEndpoint::Back(distance) => self
                    .tail
                    .as_ref()
                    .and_then(|tail| tail.span_back(distance))
                    .map(|(start, _)| start),
                RangeEndpoint::Tail => None,
            };
            let last_end = match self.to_kind {
                RangeEndpoint::Fixed(fixed) if fixed >= count => self.to_span.map(|(_, end)| end).or(self.last_end),
                RangeEndpoint::Fixed(_) => self.to_span.map(|(_, end)| end),
                RangeEndpoint::Tail => self.last_end,
                RangeEndpoint::Back(distance) => self
                    .tail
                    .as_ref()
                    .and_then(|tail| tail.span_back(distance))
                    .map(|(_, end)| end),
                RangeEndpoint::Head => None,
            };
            let (Some(first_start), Some(last_end)) = (first_start, last_end) else {
                return Located::Range { start: 0, end: 0 };
            };
            return Located::Range {
                start: first_start,
                end: last_end,
            };
        }
        match self.region_start {
            Some(start) => Located::Range {
                start,
                end: self.region_end,
            },
            None => Located::Range { start: 0, end: 0 },
        }
    }
}

/// Fixed-capacity ring of the most recent `cap` spans, serving the negative-bound endpoints of a pending range. Unlike
/// [`Ring`] it carries no nested selections — a range publishes a byte extent, not a node.
struct TailSpans {
    cap: usize,
    spans: Vec<(usize, usize)>,
    len: usize,
    head: usize,
}

impl TailSpans {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            spans: Vec::new(),
            len: 0,
            head: 0,
        }
    }

    fn push(&mut self, span: (usize, usize)) {
        if self.cap == 0 {
            return;
        }
        if self.len < self.cap {
            self.spans.push(span);
            self.len += 1;
        } else {
            self.spans[self.head] = span;
            self.head = (self.head + 1) % self.cap;
        }
    }

    /// The span `back` positions before the most recent one (`back == 1` is the last seen span), falling back to the
    /// OLDEST seen span when fewer than `back` exist — the same clamp `resolve_against_count` applies to a negative
    /// bound past the container's start.
    fn span_back(&self, back: usize) -> Option<(usize, usize)> {
        if back == 0 || self.len == 0 {
            return None;
        }
        let distance = back.min(self.len);
        // While under capacity (`len < cap`, `head == 0`) the slots are in arrival order, so the offset is `len -
        // distance`; once wrapped, the newest sits just behind `head`. One formula covers both: adding `len` instead of
        // assuming a full ring.
        let position = (self.head + self.len + self.cap - distance) % self.cap;
        self.spans.get(position).copied()
    }
}

// =================================================================== Locate helpers kept for the fused walk (range
// bounds, skip, key match). ===================================================================

/// Reads one non-negative signed bound as a `usize` position, saturating at `usize::MAX` (a bound past the container's
/// own length clamps to an edge anyway). Anything else — the omitted bound, or a strictly-negative one, which
/// `try_new`'s two-pass gating makes unreachable here — is the container's edge (`None`).
fn resolve_non_negative(bound: Option<i64>) -> Option<usize> {
    match bound {
        Some(value) if value >= 0 => Some(usize::try_from(value).unwrap_or(usize::MAX)),
        _ => None,
    }
}

/// Resolves one signed bound against the observed element count under the slice law (`resolve_slice_bound`): a
/// strictly-negative bound counts from the end (`count + value`, clamped below at the container's start), and a
/// non-negative one clamps to the count. The bound is already INTEGRAL (the boundary law normalized it), so no rounding
/// remains.
fn resolve_against_count(bound: Option<i64>, count: usize) -> Option<usize> {
    match bound {
        None => None,
        Some(value) if value >= 0 => Some(usize::try_from(value).unwrap_or(usize::MAX).min(count)),
        Some(value) => {
            // `count + value` for a negative `value`, clamped at 0 (a bound past the start wraps to the container's
            // start).
            let magnitude = usize::try_from(value.unsigned_abs()).unwrap_or(usize::MAX);
            Some(count.saturating_sub(magnitude))
        }
    }
}

/// Semantic kind of a validated JSON value from its first payload byte. Post-validation the lead is unambiguous except
/// `n` (`null` vs `nan`).
pub(crate) fn element_kind(bytes: &[u8], start: usize) -> ValueKind {
    match bytes.get(start).copied().unwrap_or(0) {
        b'n' => {
            // `nan` (the validator's case-insensitive non-finite spelling) is a Number; `null` is the lowercase
            // literal. Post-validation no other `n`-initial token exists.
            let rest = bytes.get(start + 1..).unwrap_or(&[]);
            if rest.starts_with(b"ull") {
                ValueKind::Null
            } else {
                ValueKind::Number
            }
        }
        b't' | b'f' => ValueKind::Bool,
        b'"' | b'\'' => ValueKind::String,
        b'[' => ValueKind::Array,
        b'{' => ValueKind::Object,
        // A number, a non-finite spelling (`inf`, `NaN`, `-inf`), and the fail-closed arm all classify as numbers.
        _ => ValueKind::Number,
    }
}

/// Skips one JSON-whitespace run. Live in production (`lazy.rs`'s span walk).
pub(crate) fn skip_ws(bytes: &[u8], cursor: usize) -> usize {
    // Compact input's next byte is never whitespace; skip the prefix scan.
    match bytes.get(cursor) {
        Some(b' ' | b'\t' | b'\n' | b'\r') => cursor + crate::byte_scan::ws_prefix_len(&bytes[cursor..]),
        _ => cursor,
    }
}

#[cfg(test)]
fn classify(bytes: &[u8], pos: usize) -> ValueKind {
    let pos = skip_ws(bytes, pos);
    // The element's kind from its first significant byte — in particular the `n` arm, which peeks the following bytes
    // to tell `nan` (a number, case-insensitively) from `null`. Without the peek a `nan` container would report a
    // Null-kind mismatch, and the engine's Null-precedence law answers `null` where the reference raises "Cannot index
    // number".
    element_kind(bytes, pos)
}

/// Compares the object-key content `bytes[a..b]` (between the quotes, possibly escaped) against `member`, decoding
/// escapes and surrogate pairs identically to the full parser so the winner matches `ObjectView::get`.
///
/// # Preconditions
///
/// `bytes[a..b]` must be the inner content of an object-key string that the scoped validate phase already accepted as
/// well-formed RFC 8259: every `\` is followed by a complete valid escape (`\uXXXX` with four hex digits), and every
/// high surrogate (`\ud800`..=`\udbff`) is immediately followed by a `\u` low surrogate (`\udc00`..=`\udfff`), all
/// within `[a, b)`. Under that precondition the escape walk below never indexes past the key or underflows the
/// surrogate arithmetic; the debug assertions restate the exact invariants each unchecked step relies on. The `a <= b
/// <= bytes.len()` bounds are the precondition re-checked in release, because callers derive `a`/`b` from spans and a
/// stale span must fail closed rather than panic.
pub(crate) fn key_equals_inner(bytes: &[u8], a: usize, b: usize, member: &str) -> bool {
    if a > b || b > bytes.len() {
        return false;
    }
    let key = &bytes[a..b];
    if !key.contains(&b'\\') {
        return key == member.as_bytes();
    }
    let mbytes = member.as_bytes();
    let mut mi = 0;
    let mut i = 0;
    let mut buf = [0u8; 4];
    while i < key.len() {
        let byte = key[i];
        if byte == b'\\' {
            i += 1;
            // Precondition: a validated key never ends on a lone backslash.
            debug_assert!(
                i < key.len(),
                "validated object key: `\\` is followed by an escape byte"
            );
            let escape = key[i];
            i += 1;
            let ch = match escape {
                b'"' => '"',
                b'\\' => '\\',
                b'/' => '/',
                b'b' => '\u{8}',
                b'f' => '\u{c}',
                b'n' => '\n',
                b'r' => '\r',
                b't' => '\t',
                b'u' => {
                    let high = hex4(key, i);
                    i += 4;
                    if (0xd800..=0xdbff).contains(&high) {
                        // Surrogate pair: skip the `\u` and read the low surrogate. Precondition: the validate phase
                        // proved a `\u` low surrogate in range follows, so the reads stay in bounds and `low - 0xdc00`
                        // never underflows.
                        debug_assert!(
                            i + 6 <= key.len() && key[i] == b'\\' && key[i + 1] == b'u',
                            "validated object key: high surrogate is followed by `\\u`"
                        );
                        i += 2;
                        let low = hex4(key, i);
                        i += 4;
                        debug_assert!(
                            (0xdc00..=0xdfff).contains(&low),
                            "validated object key: low surrogate is in range"
                        );
                        let scalar = 0x1_0000 + ((u32::from(high) - 0xd800) << 10) + (u32::from(low) - 0xdc00);
                        match char::from_u32(scalar) {
                            Some(ch) => ch,
                            None => return false,
                        }
                    } else {
                        // A lone low surrogate decodes to U+FFFD, exactly as the whole parser (parse.rs) decodes it;
                        // only a high surrogate not followed by a low one is an error.
                        if (0xdc00..=0xdfff).contains(&high) {
                            '\u{fffd}'
                        } else {
                            match char::from_u32(u32::from(high)) {
                                Some(ch) => ch,
                                None => return false,
                            }
                        }
                    }
                }
                _ => return false,
            };
            let encoded = ch.encode_utf8(&mut buf).as_bytes();
            if mbytes.len() < mi + encoded.len() || &mbytes[mi..mi + encoded.len()] != encoded {
                return false;
            }
            mi += encoded.len();
        } else {
            if mi >= mbytes.len() || mbytes[mi] != byte {
                return false;
            }
            mi += 1;
            i += 1;
        }
    }
    mi == mbytes.len()
}

fn hex4(bytes: &[u8], at: usize) -> u16 {
    let mut value = 0u16;
    for offset in 0..4 {
        let digit = bytes.get(at + offset).copied().and_then(hex).unwrap_or(0);
        value = (value << 4) | u16::from(digit);
    }
    value
}

/// Fixed-capacity ring of the most recent `cap` element spans, grown lazily so its memory stays bounded by the observed
/// array length. Resolves negative array indices without materializing the array. Nested observations travel with the
/// span so `.a[-2].b` keeps the second-to-last element's `.b`.
struct Ring {
    cap: usize,
    spans: Vec<(usize, usize)>,
    nested: Vec<Option<Located>>,
    len: usize,
    head: usize,
}

impl Ring {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            spans: Vec::new(),
            nested: Vec::new(),
            len: 0,
            head: 0,
        }
    }

    fn push(&mut self, span: (usize, usize), nested: Option<Located>) {
        if self.cap == 0 {
            return;
        }
        if self.len < self.cap {
            self.spans.push(span);
            self.nested.push(nested);
            self.len += 1;
        } else {
            self.spans.as_mut_slice()[self.head] = span;
            self.nested.as_mut_slice()[self.head] = nested;
            self.head = (self.head + 1) % self.cap;
        }
    }

    fn winner(&self) -> Option<(usize, usize)> {
        if self.len == self.cap && self.cap > 0 {
            self.spans.as_slice().get(self.head).copied()
        } else {
            None
        }
    }

    fn winner_nested(&self) -> Option<Located> {
        if self.len == self.cap && self.cap > 0 {
            self.nested.as_slice().get(self.head).copied().flatten()
        } else {
            None
        }
    }
}

// Test-support oracle section — compiled ONLY under `cfg(test)`.
//
// Everything in this module is a test oracle over validated bytes: no production code calls any of it (the validate
// walk records located spans directly and never re-skips). It lives beside its only consumers, the test modules below,
// so an oracle and the tests that read it drift together.
//
// These loops deliberately RESTATE the scan laws instead of calling the production walkers, so a production grammar or
// byte-kernel change must be mirrored here by hand — and a divergence shows up as a failing test rather than as
// silently shared code.
#[cfg(test)]
pub(crate) mod skip_oracle {
    //! A resumable structural value skip over known-valid bytes — the window-cut oracle the tests drive
    //! window-by-window.
    #[derive(Clone, Copy)]
    pub(crate) struct ValueSkip {
        kind: Option<SkipKind>,
        depth: usize,
        in_string: bool,
        escaped: bool,
    }

    #[derive(Clone, Copy)]
    enum SkipKind {
        Scalar,
        String,
        Container,
    }

    impl ValueSkip {
        pub(crate) const fn new() -> Self {
            Self {
                kind: None,
                depth: 0,
                in_string: false,
                escaped: false,
            }
        }

        /// Advances the skip over `bytes[cursor..limit]`. Returns `Some(end)` when the value is complete (end is one
        /// past its last byte), or `None` when the skip exhausted the window and must resume from `*cursor`.
        ///
        /// The loop-carried state (kind, depth, the two string flags, and the cursor) is held in LOCALS for the whole
        /// byte walk and written back to `self`/`*cursor` only at this call's return — the run boundary. A state that
        /// stays memory-resident across the inner loop makes LLVM spill it narrow (`strb` for a bool) and reload it
        /// wide (`ldr x`/`ldp q`) at the loop top, and a narrow-store→wide-load pair does not store-forward in the
        /// emulated container (~+12.5 cycles per event). The same write-back discipline is what keeps the cooperative
        /// resumption exact: a paused window's state is indistinguishable from the pre-hoist code's.
        ///
        /// Bulk runs go through the `byte_scan` prefix kernels: plain string content to the next quote/backslash,
        /// container noise to the next structural byte, and a bare word to its terminator. Post-validation the byte
        /// sets the kernels halt on are exactly the byte sets the scalar loops used to visit one at a time, so the
        /// stops are identical.
        #[expect(
            clippy::too_many_lines,
            reason = "one resumable three-kind skip state machine; splitting it would scatter the shared write-back law across helpers"
        )]
        pub(crate) fn advance(&mut self, bytes: &[u8], cursor: &mut usize, limit: usize) -> Option<usize> {
            let mut kind = self.kind;
            let mut depth = self.depth;
            let mut in_string = self.in_string;
            let mut escaped = self.escaped;
            let mut pos = *cursor;
            let result = 'run: {
                let classified = if let Some(kind) = kind {
                    kind
                } else {
                    let Some(&byte) = bytes.get(pos) else {
                        break 'run None;
                    };
                    let value = match byte {
                        b'{' | b'[' => {
                            depth = 1;
                            SkipKind::Container
                        }
                        b'"' => {
                            in_string = true;
                            SkipKind::String
                        }
                        _ => SkipKind::Scalar,
                    };
                    // The classification is recorded immediately, exactly as the pre-hoist code stored it before any
                    // delimiter check: a paused window's `kind` must survive to the next call.
                    kind = Some(value);
                    pos += 1;
                    if matches!(value, SkipKind::Scalar) && (pos >= bytes.len() || is_delimiter_at(bytes, pos)) {
                        break 'run Some(pos);
                    }
                    value
                };
                match classified {
                    SkipKind::Scalar => {
                        // The scalar walk is straight-line: each step either ends the value or consumes a run, so a
                        // loop would never iterate more than once.
                        let complete = if pos >= limit {
                            false
                        } else if bytes[pos].is_ascii_digit() {
                            // A scalar's digit run (a long number literal) contains no delimiter, so skip it in bulk
                            // instead of probing one delimiter check per digit.
                            let digits = &bytes[pos..limit];
                            let run = digits
                                .iter()
                                .position(|byte| !byte.is_ascii_digit())
                                .unwrap_or(digits.len());
                            pos += run;
                            if pos >= limit {
                                false
                            } else {
                                pos += bare_word_run(&bytes[pos..limit]);
                                pos < limit
                            }
                        } else {
                            // The rest of a word (literal spelling, number suffix) is non-delimiter bytes: pass it
                            // whole and stop at the delimiter that ends the value.
                            pos += bare_word_run(&bytes[pos..limit]);
                            pos < limit
                        };
                        // A window-exhausted scalar is complete only when the whole input ended (a delimiter was never
                        // required at EOF).
                        if complete || pos >= bytes.len() {
                            Some(pos)
                        } else {
                            None
                        }
                    }
                    SkipKind::String => {
                        let complete = loop {
                            if pos >= limit {
                                break false;
                            }
                            if escaped {
                                // The byte after a backslash is literal: never bulk skipped, exactly one byte clears
                                // the escape.
                                pos += 1;
                                escaped = false;
                                continue;
                            }
                            let run = crate::byte_scan::plain_string_prefix_len(&bytes[pos..limit]);
                            pos += run;
                            if pos >= limit {
                                break false;
                            }
                            let byte = bytes[pos];
                            pos += 1;
                            if byte == b'\\' {
                                escaped = true;
                            } else if byte == b'"' {
                                break true;
                            }
                            // Any other byte (a UTF-8 continuation) is not a stop; the loop resumes scanning after it.
                        };
                        // A string that ends exactly at the window's end is handled by the caller's end-of-input check,
                        // exactly as before.
                        if complete { Some(pos) } else { None }
                    }
                    SkipKind::Container => {
                        let complete = loop {
                            if pos >= limit {
                                break false;
                            }
                            if in_string {
                                if escaped {
                                    pos += 1;
                                    escaped = false;
                                    continue;
                                }
                                let run = crate::byte_scan::plain_string_prefix_len(&bytes[pos..limit]);
                                pos += run;
                                if pos >= limit {
                                    break false;
                                }
                                let byte = bytes[pos];
                                pos += 1;
                                if byte == b'\\' {
                                    escaped = true;
                                } else if byte == b'"' {
                                    in_string = false;
                                }
                                continue;
                            }
                            // Outside a string every byte is either structural (`"{[}]`) or noise; skip the noise in
                            // bulk.
                            let run = crate::byte_scan::structural_prefix_len(&bytes[pos..limit]);
                            pos += run;
                            if pos >= limit {
                                break false;
                            }
                            let byte = bytes[pos];
                            pos += 1;
                            match byte {
                                b'"' => in_string = true,
                                b'{' | b'[' => depth += 1,
                                b'}' | b']' => {
                                    depth -= 1;
                                    if depth == 0 {
                                        break true;
                                    }
                                }
                                // The kernel halts only on the structural set; a byte outside it is a kernel bug,
                                // consumed rather than trusted.
                                _ => {}
                            }
                        };
                        if complete { Some(pos) } else { None }
                    }
                }
            };
            *cursor = pos;
            self.kind = kind;
            self.depth = depth;
            self.in_string = in_string;
            self.escaped = escaped;
            result
        }
    }

    fn is_delimiter_at(bytes: &[u8], pos: usize) -> bool {
        bytes.get(pos).copied().is_some_and(crate::lex::is_delimiter)
    }

    /// Longest prefix of `bytes` that a bare word (a number or a `true`/`false`/ `null`/`nan` spelling) can still
    /// occupy: the delimiter kernel's run, capped at the first byte that OPENS a self-delimiting value. Both parsers
    /// end a bare word at [`crate::lex::is_value_boundary`], which is the delimiter set plus `"`, `[`, `{` — `1"a"`
    /// is two adjacent values, not one. The kernel's stop set is the narrower one, so the cap restores the law here.
    /// Inside a container the cap never fires: validation already rejected a bare word butted against a self-delimiting
    /// value, so only a root value in adjacent mode can reach it.
    fn bare_word_run(bytes: &[u8]) -> usize {
        let run = crate::byte_scan::delimiter_prefix_len(bytes);
        bytes[..run]
            .iter()
            .position(|&byte| matches!(byte, b'"' | b'[' | b'{'))
            .unwrap_or(run)
    }
}

#[cfg(test)]
mod tests {
    use super::skip_oracle::ValueSkip;
    use super::{Ring, ScopedSession, classify, element_kind, key_equals_inner};
    use crate::lex::is_delimiter;
    use jqf_codec_core::{AccessOutcome, AccessSession, ExactPath, ExactSelectionRecord, SelectionOrigin};
    use jqf_data::{BuilderCoverage, DiagnosticCoverage, ValueKind};

    use crate::test_support;

    /// The FRONTIER receipt: a scoped validate failure over a corrupt byte in an UNREAD field emits the
    /// `FRONTIER_VALIDATION` record naming the path and offset — the validate-everything-first law's own fingerprint.
    /// The path state is purely additive, so a valid input must emit nothing.
    #[test]
    fn frontier_record_names_the_unread_field() {
        use jqf_resource::diag::DiagnosticBuffer;
        use jqf_resource::diag::codes;

        let mut resources = test_support::resources();
        let buffer = DiagnosticBuffer::with_cap(16);
        resources.set_diagnostics(&buffer);

        // The program reads only `id`; the corrupt byte sits in `name`.
        let corrupt = b"[{\"id\": 1, \"name\": \"ok\"}, {\"id\": 2, \"name\": \"bad\x01\"}]";
        let source = jqf_source::ResolvedSource::new(
            jqf_source::SourceRef::new(jqf_source::SourceId::new(1), jqf_source::SourceKind::Input),
            "<test>",
            corrupt,
            0,
        );
        let mut path = ExactPath::try_new(&resources);
        path.try_push_semantic_member("id", &resources).expect("member step");
        let schema = crate::parse::try_schema_prototype(&resources).expect("schema");
        let mut session = ScopedSession::try_new_with_schema_prototype(
            path.steps(),
            SelectionOrigin::new(0),
            DiagnosticCoverage::NotRequested,
            BuilderCoverage::complete(),
            false,
            schema,
            &resources,
        )
        .expect("session");
        let mut run = jqf_codec_core::CodecRunContext::new(&mut resources);
        assert!(
            session
                .decode(jqf_codec_core::AccessInput::Source(source), &mut run)
                .is_err(),
            "the corrupt input must fail validation"
        );
        let records = buffer.records();
        let frontier = records
            .iter()
            .find(|record| record.code == codes::FRONTIER_VALIDATION)
            .expect("a frontier record must be emitted");
        assert_eq!(frontier.operand(), Some("[1].name"));
        assert!(frontier.byte_offset.is_some());
    }

    /// A reopened session starts its frontier path at the ROOT. The per-frame pops never run for a value that failed
    /// INSIDE a container, so a session recycled after such a failure would name the next document's fields under the
    /// dead one's path.
    #[test]
    fn a_reopened_session_names_the_next_documents_path_from_the_root() {
        use jqf_resource::diag::DiagnosticBuffer;
        use jqf_resource::diag::codes;

        let mut resources = test_support::resources();
        let buffer = DiagnosticBuffer::with_cap(16);
        resources.set_diagnostics(&buffer);

        let resolved = |bytes: &'static [u8]| {
            jqf_source::ResolvedSource::new(
                jqf_source::SourceRef::new(jqf_source::SourceId::new(1), jqf_source::SourceKind::Input),
                "<test>",
                bytes,
                0,
            )
        };
        let mut path = ExactPath::try_new(&resources);
        path.try_push_semantic_member("id", &resources).expect("member step");
        let schema = crate::parse::try_schema_prototype(&resources).expect("schema");
        let mut session = ScopedSession::try_new_with_schema_prototype(
            path.steps(),
            SelectionOrigin::new(0),
            DiagnosticCoverage::NotRequested,
            BuilderCoverage::complete(),
            false,
            schema,
            &resources,
        )
        .expect("session");

        // First document: fails two containers deep, so the frontier is left holding `[0].wrap.name` when the error
        // unwinds.
        {
            let mut run = jqf_codec_core::CodecRunContext::new(&mut resources);
            assert!(
                session
                    .decode(
                        jqf_codec_core::AccessInput::Source(resolved(
                            b"[{\"id\": 1, \"wrap\": {\"name\": \"bad\x01\"}}]"
                        )),
                        &mut run,
                    )
                    .is_err(),
                "the corrupt input must fail validation"
            );
        }
        assert!(session.try_reset(
            path.steps(),
            SelectionOrigin::new(0),
            DiagnosticCoverage::NotRequested,
            BuilderCoverage::complete(),
            false,
        ));
        {
            let mut run = jqf_codec_core::CodecRunContext::new(&mut resources);
            assert!(
                session
                    .decode(
                        jqf_codec_core::AccessInput::Source(resolved(b"[{\"id\": 2, \"name\": \"bad\x01\"}]")),
                        &mut run,
                    )
                    .is_err(),
                "the second corrupt input must fail validation too"
            );
        }
        let records = buffer.records();
        let operand = records
            .iter()
            .rfind(|record| record.code == codes::FRONTIER_VALIDATION)
            .expect("a frontier record must be emitted")
            .operand();
        assert_eq!(operand, Some("[0].name"));
    }

    /// `key_equals_inner` receives the inner (between-quote) key bytes and the already-decoded member string, and must
    /// decode escapes and surrogate pairs the same way the full parser does.
    #[test]
    fn key_equals_inner_matches_plain_and_escaped_and_surrogate_keys() {
        let eq = |key: &[u8], member: &str| key_equals_inner(key, 0, key.len(), member);
        // Plain, no escapes: fast byte compare.
        assert!(eq(b"hello", "hello"));
        assert!(!eq(b"hello", "help"));
        assert!(!eq(b"hell", "hello"));
        // Simple two-character escapes decode to their control byte.
        assert!(eq(br"a\nb", "a\nb"));
        assert!(eq(br#"a\"b"#, "a\"b"));
        assert!(eq(br"a\\b", "a\\b"));
        assert!(eq(br"a\tb\r", "a\tb\r"));
        assert!(eq(br"a\/b", "a/b"));
        // BMP `\u` escapes decode to their scalar: `AB` -> "AB".
        let bmp = b"\\u0041\\u0042";
        assert!(eq(bmp, "AB"));
        // A surrogate pair `😀` decodes to U+1F600 (astral scalar).
        let pair = b"\\ud83d\\ude00";
        assert!(eq(pair, "\u{1f600}"));
        assert!(!eq(pair, "\u{1f601}"));
        // Escaped key that is a prefix of / longer than the member: reject.
        assert!(!eq(br"a\nb", "a\n"));
        assert!(!eq(br"a\n", "a\nb"));
    }

    #[test]
    fn key_equals_inner_decodes_lone_low_surrogate_to_replacement_char() {
        let eq = |key: &[u8], member: &str| key_equals_inner(key, 0, key.len(), member);
        // A lone low surrogate decodes to U+FFFD, exactly as the whole parser (parse.rs) decodes it for a
        // `\udc00`..`\udfff` escape; it is not the literal escape text.
        assert!(eq(b"\\udc00", "\u{fffd}"));
        assert!(!eq(b"\\udc00", "\\udc00"));
        // The paired case still decodes to the astral scalar.
        assert!(eq(b"\\ud83d\\ude00", "\u{1f600}"));
    }

    #[test]
    fn key_equals_inner_rejects_out_of_range_spans_without_panicking() {
        assert!(!key_equals_inner(b"abc", 2, 1, "")); // a > b
        assert!(!key_equals_inner(b"abc", 0, 4, "abc")); // b > len
    }

    /// Pins the `\uXXXX` hex-digit set as CASE-INSENSITIVE through the scoped validator: an uppercase-hex escape must
    /// validate exactly where its lowercase twin does. The whole parser and this route read one shared `hex()` LUT
    /// covering `0-9a-fA-F`, so a table narrowed to lowercase (or a case-sensitive compare introduced anywhere on this
    /// path) would reject the uppercase spelling scoped-side while plain decode still accepted it — an input
    /// answering differently by route.
    #[test]
    fn unicode_escapes_accept_uppercase_hex_digits_like_lowercase() {
        let decodes = |bytes: &'static [u8]| {
            let mut resources = test_support::resources();
            let schema = crate::parse::try_schema_prototype(&resources).expect("schema");
            let mut session = ScopedSession::try_new_with_schema_prototype(
                &[],
                SelectionOrigin::new(0),
                DiagnosticCoverage::NotRequested,
                BuilderCoverage::complete(),
                false,
                schema,
                &resources,
            )
            .expect("session");
            let source = jqf_source::ResolvedSource::new(
                jqf_source::SourceRef::new(jqf_source::SourceId::new(1), jqf_source::SourceKind::Input),
                "<test>",
                bytes,
                0,
            );
            let mut run = jqf_codec_core::CodecRunContext::new(&mut resources);
            let result = session
                .decode(jqf_codec_core::AccessInput::Source(source), &mut run)
                .expect("both hex casings must validate and publish");
            matches!(result.outcome(), AccessOutcome::Located(_))
        };
        // Lowercase: the baseline spelling.
        assert!(decodes(br#""\u00e9""#));
        // Uppercase digits in every position of the same scalar.
        assert!(decodes(br#""\u00E9""#));
        // Mixed case, including across a surrogate pair's two escapes.
        assert!(decodes(br#""\uD83D\udE00""#));
        assert!(decodes(br#""\uFFFD""#));
    }

    /// The ring resolves negative array indices: with capacity `k` the winner is the element `k` positions from the
    /// end, and only once at least `k` elements have been observed.
    #[test]
    fn ring_resolves_negative_index_as_kth_from_end() {
        // capacity 2 == `.[-2]`: winner is the second-to-last element span.
        let mut ring = Ring::new(2);
        assert_eq!(ring.winner(), None, "fewer than cap elements: no winner yet");
        ring.push((0, 1), None);
        assert_eq!(ring.winner(), None);
        ring.push((2, 3), None);
        assert_eq!(ring.winner(), Some((0, 1)), "two elements: -2 is the first");
        ring.push((4, 5), None);
        assert_eq!(ring.winner(), Some((2, 3)), "three elements: -2 is the middle");
        ring.push((6, 7), None);
        assert_eq!(ring.winner(), Some((4, 5)), "four elements: -2 is the third");
    }

    #[test]
    fn ring_capacity_one_tracks_the_last_element() {
        let mut ring = Ring::new(1);
        ring.push((0, 1), None);
        assert_eq!(ring.winner(), Some((0, 1)));
        ring.push((2, 3), None);
        assert_eq!(ring.winner(), Some((2, 3)), "-1 is always the last element");
    }

    #[test]
    fn ring_capacity_zero_never_wins() {
        let mut ring = Ring::new(0);
        ring.push((0, 1), None);
        assert_eq!(ring.winner(), None);
    }

    fn skip_whole(bytes: &[u8]) -> Option<usize> {
        let mut skip = ValueSkip::new();
        let mut cursor = 0;
        skip.advance(bytes, &mut cursor, bytes.len())
    }

    #[test]
    fn value_skip_finds_the_end_of_scalars_strings_and_containers() {
        assert_eq!(skip_whole(b"123,"), Some(3), "scalar ends at the delimiter");
        assert_eq!(skip_whole(b"true"), Some(4), "scalar ends at end of input");
        assert_eq!(skip_whole(br#""a\"b""#), Some(6), "string honors escaped quote");
        assert_eq!(skip_whole(b"[1,[2],3]"), Some(9), "container tracks nested depth");
        assert_eq!(
            skip_whole(br#"{"a":"]","b":[1]}"#),
            Some(17),
            "container ignores brackets inside strings"
        );
    }

    /// The skip is resumable: exhausting the window returns `None` and the next call continues from the shared cursor.
    #[test]
    fn value_skip_resumes_across_windows() {
        let bytes = b"12345,";
        let mut skip = ValueSkip::new();
        let mut cursor = 0;
        assert_eq!(skip.advance(bytes, &mut cursor, 3), None, "window exhausted");
        assert_eq!(cursor, 3);
        assert_eq!(
            skip.advance(bytes, &mut cursor, bytes.len()),
            Some(5),
            "resumes and finds the delimiter"
        );
    }

    /// A reference implementation of the skip semantics as they stood before the forwarding-clean rewrite: same stop
    /// predicates, state kept in the struct between windows, one `advance` per window exactly like the production
    /// driver. The rewrite must be indistinguishable from this on every windowing of every input, because the skip runs
    /// over the same bytes and the resumption contract is exact.
    #[derive(Clone, Copy)]
    enum RefKind {
        Scalar,
        String,
        Container,
    }

    #[derive(Clone, Copy)]
    struct RefSkip {
        kind: Option<RefKind>,
        depth: usize,
        in_string: bool,
        escaped: bool,
    }

    impl RefSkip {
        fn new() -> Self {
            Self {
                kind: None,
                depth: 0,
                in_string: false,
                escaped: false,
            }
        }

        fn advance(&mut self, bytes: &[u8], cursor: &mut usize, limit: usize) -> Option<usize> {
            let kind = if let Some(kind) = self.kind {
                kind
            } else {
                let byte = *bytes.get(*cursor)?;
                let kind = match byte {
                    b'{' | b'[' => {
                        self.depth = 1;
                        RefKind::Container
                    }
                    b'"' => {
                        self.in_string = true;
                        RefKind::String
                    }
                    _ => RefKind::Scalar,
                };
                self.kind = Some(kind);
                *cursor += 1;
                if matches!(kind, RefKind::Scalar)
                    && (*cursor >= bytes.len() || bytes.get(*cursor).copied().is_some_and(is_delimiter))
                {
                    return Some(*cursor);
                }
                kind
            };
            match kind {
                RefKind::Scalar => {
                    while *cursor < limit {
                        if bytes[*cursor].is_ascii_digit() {
                            let digits = &bytes[*cursor..limit];
                            let run = digits
                                .iter()
                                .position(|byte| !byte.is_ascii_digit())
                                .unwrap_or(digits.len());
                            *cursor += run;
                            if *cursor >= limit {
                                break;
                            }
                        }
                        if bytes.get(*cursor).copied().is_some_and(is_delimiter) {
                            return Some(*cursor);
                        }
                        *cursor += 1;
                    }
                    if *cursor >= bytes.len() {
                        return Some(*cursor);
                    }
                    None
                }
                RefKind::String => {
                    while *cursor < limit {
                        let byte = bytes[*cursor];
                        *cursor += 1;
                        if self.escaped {
                            self.escaped = false;
                        } else if byte == b'\\' {
                            self.escaped = true;
                        } else if byte == b'"' {
                            return Some(*cursor);
                        }
                    }
                    None
                }
                RefKind::Container => {
                    while *cursor < limit {
                        let byte = bytes[*cursor];
                        *cursor += 1;
                        if self.in_string {
                            if self.escaped {
                                self.escaped = false;
                            } else if byte == b'\\' {
                                self.escaped = true;
                            } else if byte == b'"' {
                                self.in_string = false;
                            }
                            continue;
                        }
                        match byte {
                            b'"' => self.in_string = true,
                            b'{' | b'[' => self.depth += 1,
                            b'}' | b']' => {
                                self.depth -= 1;
                                if self.depth == 0 {
                                    return Some(*cursor);
                                }
                            }
                            _ => {}
                        }
                    }
                    None
                }
            }
        }
    }

    /// Every windowing of every skip-shaped input agrees with the reference skip: same completion, same cursor, byte
    /// for byte.
    #[test]
    fn skip_rewrite_is_indistinguishable_from_the_reference() {
        let inputs: &[&[u8]] = &[
            b"123,",
            b"true",
            b"null",
            b"\"a\\\"b\"",
            b"[1,[2],3]",
            b"{\"a\":\"]\",\"b\":[1]}",
            b"\"x\"",
            b"[[[[1]]]]",
            b"{\"k\":\"\\u00e9\\ud800\\udc00\"}",
            b"[0,1,2,3,4,5]",
            b"1.5e10",
        ];
        for input in inputs {
            // Every cut point: the skip is chained across `[cut, len]` — the exhaustive resumption surface. The
            // reference and the rewrite both carry state across the two calls.
            for cut in 0..=input.len() {
                let mut mine = ValueSkip::new();
                let mut mcursor = 0;
                let m1 = mine.advance(input, &mut mcursor, cut);
                let m2 = if m1.is_none() {
                    mine.advance(input, &mut mcursor, input.len())
                } else {
                    None
                };
                let mut reference = RefSkip::new();
                let mut rcursor = 0;
                let r1 = reference.advance(input, &mut rcursor, cut);
                let r2 = if r1.is_none() {
                    reference.advance(input, &mut rcursor, input.len())
                } else {
                    None
                };
                assert_eq!((m1, m2, mcursor), (r1, r2, rcursor), "window cut {cut} of {input:?}");
            }
        }
    }

    /// R1 poisoned-session guard: `try_reset` recycles a session ONLY for the exact path it already owns. A different
    /// path declines, so the caller constructs a fresh session instead of navigating with stale steps.
    #[test]
    fn try_reset_declines_a_different_exact_path() {
        let resources = test_support::resources();
        let path = |members: &[&str], index: Option<i64>| {
            let mut path = ExactPath::try_new(&resources);
            for member in members {
                path.try_push_semantic_member(member, &resources).expect("member step");
            }
            if let Some(index) = index {
                path.try_push_semantic_index(index, &resources);
            }
            path
        };
        let origin = SelectionOrigin::new(0);
        let reset = |session: &mut ScopedSession, path: &ExactPath| {
            session.try_reset(
                path.steps(),
                origin,
                DiagnosticCoverage::NotRequested,
                BuilderCoverage::complete(),
                true,
            )
        };
        let owned = path(&["a"], None);
        let schema = crate::parse::try_schema_prototype(&resources).expect("schema");
        let mut session = ScopedSession::try_new_with_schema_prototype(
            owned.steps(),
            origin,
            DiagnosticCoverage::NotRequested,
            BuilderCoverage::complete(),
            true,
            schema,
            &resources,
        )
        .expect("scoped session");
        assert!(reset(&mut session, &owned), "the same path recycles");
        assert!(
            !reset(&mut session, &path(&["b"], None)),
            "a different member must decline"
        );
        assert!(
            !reset(&mut session, &path(&["a", "b"], None)),
            "a longer path must decline"
        );
        assert!(
            !reset(&mut session, &path(&[], Some(0))),
            "a different step kind must decline"
        );
    }

    /// The `n` arm is AMBIGUOUS between `null` and the `nan` non-finite number spelling the validator accepts, so
    /// `element_kind` peeks the following bytes: `nan` (case-insensitively, as the validator reads it) classifies as a
    /// Number — never a Null — and `null` stays Null.
    #[test]
    fn element_kind_disambiguates_nan_from_null() {
        assert_eq!(element_kind(b"null", 0), ValueKind::Null);
        assert_eq!(element_kind(b"nan", 0), ValueKind::Number);
        assert_eq!(element_kind(b"nAn", 0), ValueKind::Number);
        assert_eq!(element_kind(b"nAN", 0), ValueKind::Number);
        assert_eq!(element_kind(b"nope", 0), ValueKind::Number);
        assert_eq!(element_kind(b"n", 0), ValueKind::Number);
        assert_eq!(element_kind(b"-inf", 0), ValueKind::Number);
        assert_eq!(element_kind(b"inf", 0), ValueKind::Number);
        assert_eq!(element_kind(b"NaN", 0), ValueKind::Number);
        assert_eq!(element_kind(b"true", 0), ValueKind::Bool);
        assert_eq!(element_kind(b"7", 0), ValueKind::Number);
        assert_eq!(element_kind(b"\"s\"", 0), ValueKind::String);
        assert_eq!(element_kind(b"'s'", 0), ValueKind::String);
        assert_eq!(element_kind(b"[1]", 0), ValueKind::Array);
        assert_eq!(element_kind(b"{\"a\":1}", 0), ValueKind::Object);
        assert_eq!(element_kind(b"", 0), ValueKind::Number);
    }

    /// The container-kind oracle's `n` arm must peek the following bytes exactly like the sibling `element_kind` does:
    /// `nan` (case-insensitive) is a Number, so a path step over it raises the number mismatch; `null` stays Null.
    /// Every other spelling stays first-byte-true, exactly as `element_kind` classifies it.
    #[test]
    fn classify_names_nan_a_number_and_null_null() {
        assert_eq!(classify(b"nan", 0), ValueKind::Number);
        assert_eq!(classify(b"nAn", 0), ValueKind::Number);
        assert_eq!(classify(b"null", 0), ValueKind::Null);
        // The other non-finite spellings stay first-byte-true, as element_kind names them: `inf` and uppercase `NaN`
        // are still numbers here, `-inf` starts with `-`.
        assert_eq!(classify(b"inf", 0), ValueKind::Number);
        assert_eq!(classify(b"NaN", 0), ValueKind::Number);
        assert_eq!(classify(b"-inf", 0), ValueKind::Number);
        // The remaining arms are unchanged.
        assert_eq!(classify(b"  {", 0), ValueKind::Object);
        assert_eq!(classify(b"[1]", 0), ValueKind::Array);
        assert_eq!(classify(b"\"s\"", 0), ValueKind::String);
        assert_eq!(classify(b"true", 0), ValueKind::Bool);
        assert_eq!(classify(b"7", 0), ValueKind::Number);
    }

    /// A path step over a `nan` container reports a NUMBER-kind mismatch, so the scoped route raises "Cannot index
    /// number" exactly as the whole-document floor and the reference do — never the Null-precedence `null` a
    /// Null-kind misreport would answer.
    #[test]
    fn a_nan_container_reports_a_number_type_mismatch() {
        let mut resources = test_support::resources();
        let source = jqf_source::ResolvedSource::new(
            jqf_source::SourceRef::new(jqf_source::SourceId::new(1), jqf_source::SourceKind::Input),
            "<test>",
            b"nan",
            0,
        );
        let mut path = ExactPath::try_new(&resources);
        path.try_push_semantic_member("a", &resources).expect("member step");
        let schema = crate::parse::try_schema_prototype(&resources).expect("schema");
        let mut session = ScopedSession::try_new_with_schema_prototype(
            path.steps(),
            SelectionOrigin::new(0),
            DiagnosticCoverage::NotRequested,
            BuilderCoverage::complete(),
            false,
            schema,
            &resources,
        )
        .expect("session");
        let mut run = jqf_codec_core::CodecRunContext::new(&mut resources);
        let result = session
            .decode(jqf_codec_core::AccessInput::Source(source), &mut run)
            .expect("a lone nan must validate and locate");
        let AccessOutcome::Located(located) = result.outcome() else {
            panic!("expected a located outcome")
        };
        match located.result() {
            ExactSelectionRecord::TypeMismatch {
                step_index: 0,
                actual_type: ValueKind::Number,
                ..
            } => {}
            other => panic!("expected a Number type mismatch, got {other:?}"),
        }
    }

    /// The validator's non-finite boundary mirror: a complete spelling butted against a non-boundary byte (`nanx`)
    /// fails with the `invalid-literal` diagnostic labeled at the OFFENDING byte, the same position the whole parser
    /// labels (the smoke's `assert_malformed_nonfinite_boundary` pins the whole-parser side).
    #[test]
    fn a_boundary_violated_nonfinite_spelling_fails_at_the_offending_byte() {
        let mut resources = test_support::resources();
        let source = jqf_source::ResolvedSource::new(
            jqf_source::SourceRef::new(jqf_source::SourceId::new(1), jqf_source::SourceKind::Input),
            "<test>",
            b"nanx",
            0,
        );
        let schema = crate::parse::try_schema_prototype(&resources).expect("schema");
        let mut session = ScopedSession::try_new_with_schema_prototype(
            &[],
            SelectionOrigin::new(0),
            DiagnosticCoverage::NotRequested,
            BuilderCoverage::complete(),
            false,
            schema,
            &resources,
        )
        .expect("session");
        let mut run = jqf_codec_core::CodecRunContext::new(&mut resources);
        let Err(error) = session.decode(jqf_codec_core::AccessInput::Source(source), &mut run) else {
            panic!("nanx must fail validation");
        };
        let span = error
            .diagnostic()
            .and_then(|diagnostic| diagnostic.labels().first())
            .map(jqf_source::Label::span);
        assert_eq!(
            span,
            Some(jqf_source::Span::try_new(3, 4).expect("span")),
            "nanx must be labeled at the offending byte, not at the token's start"
        );
    }
}

#[cfg(test)]
mod range_track_tests {
    use super::{Located, RangeTrack};

    /// Asserts the range's published byte extent without needing Debug/Eq on the whole `Located` enum.
    fn assert_range(track: RangeTrack, count: usize, expected: (usize, usize)) {
        match track.finish(count) {
            Located::Range { start, end } => assert_eq!((start, end), expected),
            Located::Node { start, end } => {
                panic!("expected a range, got a node at {start}..{end}")
            }
            _ => panic!("expected a range, got a missing/mismatched selection"),
        }
    }

    /// A negative bound whose look-back EXCEEDS the observed element count clamps to the container's first element, and
    /// the tail ring serves the right span both under capacity (slots in arrival order) and after the ring has wrapped
    /// (slots rotated). The wrap formula once read spans as if always full and answered the wrong element for short
    /// containers.
    #[test]
    fn negative_range_bounds_survive_short_containers_and_ring_wrap() {
        // Short container: `[-5:]` over 3 elements is the WHOLE container.
        let mut track = RangeTrack::try_new(Some(-5), None);
        for (index, span) in [(0usize, (10usize, 11usize)), (1, (20, 21)), (2, (30, 31))] {
            track.observe(index, span.0, span.1);
        }
        assert_range(track, 3, (10, 31));

        // Wrapped ring: `[-2:]` over exactly cap+1 elements still names the last two.
        let mut track = RangeTrack::try_new(Some(-2), None);
        for index in 0usize..6 {
            track.observe(index, index * 10 + 1, index * 10 + 9);
        }
        assert_range(track, 6, (41, 59));

        // Mixed signs: `[2:-1]` skips the head, stops one before the tail.
        let mut track = RangeTrack::try_new(Some(2), Some(-1));
        for index in 0usize..5 {
            track.observe(index, index * 10 + 1, index * 10 + 9);
        }
        assert_range(track, 5, (21, 39));
    }

    /// A positive end bound clamped past the container's length must slice exactly as the whole-document route does —
    /// the window runs to the last element, never empty. The clamped fixed end index never occurs, so `finish` falls
    /// back to the last observed span; a missing capture with NO observed element stays a contract break answering
    /// empty.
    #[test]
    fn clamped_positive_end_slices_to_container_end() {
        // `[-5:20]` over 3 elements is the WHOLE container.
        let mut track = RangeTrack::try_new(Some(-5), Some(20));
        for (index, span) in [(0usize, (10usize, 11usize)), (1, (20, 21)), (2, (30, 31))] {
            track.observe(index, span.0, span.1);
        }
        assert_range(track, 3, (10, 31));

        // `[1:20]` over 3 elements keeps the head skip and still reaches the tail.
        let mut track = RangeTrack::try_new(Some(1), Some(20));
        for (index, span) in [(0usize, (10usize, 11usize)), (1, (20, 21)), (2, (30, 31))] {
            track.observe(index, span.0, span.1);
        }
        assert_range(track, 3, (20, 31));

        // Start-side overflow stays empty (`[5:10]` over 3 elements).
        let mut track = RangeTrack::try_new(Some(5), Some(10));
        for index in 0usize..3 {
            track.observe(index, index * 10, index * 10 + 1);
        }
        assert_range(track, 3, (0, 0));
    }

    /// No observed element at all answers empty even when the resolved bounds name a nonempty window — the
    /// contract-break arm the clamp fallback must not mask.
    #[test]
    fn unobserved_window_answers_empty() {
        let track = RangeTrack::try_new(Some(-5), Some(20));
        assert_range(track, 3, (0, 0));
    }

    /// A fixed end index BELOW the count must have been observed; its missing capture is a genuine contract break and
    /// answers empty even though other elements were seen — the clamp fallback is reserved for indexes at or past the
    /// count.
    #[test]
    fn missed_capture_below_count_stays_fail_closed() {
        // `[-2:3]` over 4 elements names index 2; the walker skipped it.
        let mut track = RangeTrack::try_new(Some(-2), Some(3));
        for index in [0usize, 1usize, 3usize] {
            track.observe(index, index * 10, index * 10 + 1);
        }
        assert_range(track, 4, (0, 0));
    }
}
