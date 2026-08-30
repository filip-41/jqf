//! Byte-level validate and navigate for a non-empty exact path.
//!
//! Validates the whole input to the parser's strictness while resolving the target, without building the table tree:
//!
//! - the shared [`Lexer`] enforces every byte rule (keys, strings, numbers, temporals, trivia, comments) in Skip mode,
//!   so the walk cannot drift from   the parser on any of it;
//! - the shared flat table-definition state ([`Doc`]) enforces the   table-definition rules, again identically;
//! - only the CONTAINER framing of the navigated descent (inline-table entries, array elements, with the 1.0/1.1
//!   separator laws) is walk-specific, and the standing grammar-drift fence guards it;
//! - the target path is resolved incrementally as statements arrive: headers and dotted keys resolve member steps as
//!   tables, array-of-tables headers resolve index steps, and an assignment whose value is a navigation candidate
//!   descends into the value region (inline tables and arrays are contiguous, so their entries/elements are
//!   byte-addressable).
//!
//! The located answer is either a contiguous VALUE region (re-parsed by wrapping `x = <span>`, the lazy.rs mechanism),
//! a table/array-of-tables whose subtree STATEMENT spans were collected during the walk (re-parsed by concatenation), a
//! range's in-range region or element spans, or a negative observation with the exact step semantics of the tree
//! navigator.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use jqf_codec_core::CodecError;
use jqf_data::ValueKind;
use jqf_resource::ResourceContext;
use jqf_source::{ResolvedSource, Span};

use crate::grammar::{Doc, Key, KeySeen, Lexer, Path, ValueMode};
use crate::locate::{ScopedStep, resolve_bound, resolve_range, tree_kind};
use crate::provider::DialectKind;

/// The located answer of the walk.
#[derive(Debug)]
pub(crate) enum LocatedWalk {
    /// One contiguous VALUE region (a scalar, an array, or an inline table), with the owning statement's comments —
    /// the leading set (trivia above the line plus a multi-line value's INTERIOR comments) and the inline set (the
    /// own-line trailing comment) kept SEPARATE — so the scoped materializer can attach the `toml.comment@1` and
    /// `toml.comment_inline@1` facts even though it re-parses only the value region.
    Value {
        start: usize,
        end: usize,
        leading: Vec<String>,
        inline: Vec<String>,
    },
    /// A table or array-of-tables: its subtree's STATEMENT spans in source order (each span is one complete header or
    /// assignment statement), plus any comment run between its last statement and the next `[header]` — the section
    /// foot, which the re-parse cannot see because the run sits outside every collected span.
    ///
    /// `key_depth` and `element` are the walk's exact target: how many named header components to honor after the spans
    /// re-parse, and whether that target is one array-of-tables ELEMENT rather than the array. The materializer must
    /// select that target — the re-parsed root's first child is the outermost ancestor, not the answer.
    Table {
        spans: Vec<Span>,
        foot: Vec<String>,
        key_depth: usize,
        element: bool,
    },
    /// A table that exists only as the target of a dotted key inside ONE inline table: no contiguous source region (its
    /// members are entries written with longer dotted paths) and no statement spans, so the materializer rebuilds `{
    /// <rest> = <value>, ... }` from the collected pieces — each contributing entry's remaining key path (decoded
    /// text, after the matched target prefix) and its value's source span.
    ImplicitTable { pieces: Vec<(Vec<String>, usize, usize)> },
    /// A resolved range over an array VALUE: the contiguous in-range region (bytes from the first in-range element to
    /// the last, separators included); `empty` for a degenerate range.
    RangeValue { start: usize, end: usize, empty: bool },
    /// A resolved range over an array-of-tables: each in-range element's subtree statement spans.
    RangeTables { elements: Vec<Vec<Span>> },
    /// The step at which navigation stopped: no member or position exists.
    Missing { step: usize },
    /// The step at which a kind mismatch stopped the path.
    TypeMismatch { step: usize, actual: ValueKind },
}

/// The statement-span collector armed once the answer's shape is known.
enum Collector {
    Idle,
    /// The answer is a Table at `target`; every statement whose flat path starts with it is recorded, and a comment run
    /// before a later `[header]` joins the table's FOOT set.
    Table {
        target: Path,
        spans: Vec<Span>,
        foot: Vec<String>,
    },
    /// The answer is a Range over the array-of-tables at `container`; the bounds resolve at EOF against the final
    /// element count, and statements inside an in-range element are recorded into that element's list.
    Range {
        container: Path,
        start: Option<i64>,
        end: Option<i64>,
        /// One list per element with index >= the recorded floor. The floor is the authored start when non-negative; a
        /// strictly-negative authored start cannot resolve until the element count is known, so every element records
        /// and the EOF resolution skips to the floor.
        elements: Vec<Vec<Span>>,
    },
}

/// One open negative array-of-tables index, which the single statement pass cannot resolve. Frames stack: a header
/// arming inside another frame's current element pushes onto it.
struct PendingIndex {
    /// The steps position of the Index step.
    step: usize,
    /// The authored negative index.
    index: i64,
    /// The array's flat path (without an element part).
    array: Path,
    /// One resolution per element seen, in element order.
    stash: Vec<LocatedWalk>,
    /// The current element's finished resolution when a DEEPER frame's unwind already composed it (this frame's
    /// remaining steps descended through that frame's array): finalizing prefers it over the walk's global answer
    /// state.
    resolved: Option<LocatedWalk>,
}

/// The validating validate + navigate walker over one source.
pub(crate) struct Walker<'a, 'ctx> {
    lex: Lexer<'a, 'ctx>,
    doc: Doc,
    steps: &'ctx [ScopedStep],
    /// How many target steps are resolved.
    step: usize,
    /// Open negative-array-of-tables frames, OUTERMOST first: every array-of-tables header whose Index step names a
    /// negative index arms one, and a header arming inside another frame's current element STACKS on it. A boundary or
    /// EOF resolves the INNERMOST open frame (its per-element stash selects `len + index`) and hands the result to the
    /// frame below as that frame's current-element resolution.
    pending: Vec<PendingIndex>,
    /// The flat path of the resolved table-level container (the root when nothing table-level resolved yet).
    container: Path,
    /// The determined answer, once known; the walk still validates the rest.
    answer: Option<LocatedWalk>,
    /// Whether the current answer is the STATEMENT's own value (set by `on_assignment`'s full match) rather than a
    /// descent answer: only the statement's value receives its own trailing comment.
    statement_value_answer: bool,
    /// The deepest target-step prefix any dotted-key assignment matched, even when that assignment diverged afterwards:
    /// the tree navigator's Missing step for a dotted key is the first unmatched component, not the table-level step
    /// count.
    deepest_cover: usize,
    /// The CURRENT statement's LEADING comments (consumed by the `skip_trivia` at the top of the statement loop); the
    /// statement's own inline (trailing) comment joins the value answer's INLINE set during the assignment walk. A
    /// comment run before a `[header]` never lands here: the loop diverts it to the closing table's foot.
    statement_comments: Vec<String>,
    collector: Collector,
}

impl<'a, 'ctx> Walker<'a, 'ctx> {
    pub(crate) fn try_new(
        source: ResolvedSource<'a>,
        dialect: DialectKind,
        steps: &'ctx [ScopedStep],
        resources: &'ctx ResourceContext<'ctx>,
        collect_comments: bool,
    ) -> Self {
        let doc = Doc::default();
        Self {
            lex: Lexer {
                source,
                bytes: source.bytes(),
                offset: 0,
                dialect,
                resources,
                mode: ValueMode::Skip,
                comments: Vec::new(),
                names: Vec::new(),
                name_ids: BTreeMap::new(),
                collect_comments,
            },
            doc,
            steps,
            step: 0,
            pending: Vec::new(),
            statement_value_answer: false,
            deepest_cover: 0,
            statement_comments: Vec::new(),
            container: Path::default(),
            answer: None,
            collector: Collector::Idle,
        }
    }

    /// Runs the walk: validates the whole input and resolves the target.
    pub(crate) fn walk(mut self) -> Result<LocatedWalk, CodecError> {
        // Whole-input prevalidation, exactly as the grammar performs it.
        if self.lex.bytes.len() > u32::MAX as usize {
            return Err(jqf_codec_core::CodecError::new(
                jqf_codec_core::CodecFailureKind::Overflow,
            ));
        }
        if let Err(error) = core::str::from_utf8(self.lex.bytes) {
            return Err(crate::error::invalid(
                self.lex.source,
                error.valid_up_to(),
                "invalid-utf8",
                "invalid UTF-8 sequence",
            ));
        }
        if self.lex.bytes.starts_with(b"\xEF\xBB\xBF") {
            self.lex.offset = 3;
        }
        loop {
            self.lex.skip_trivia()?;
            let comments = self.lex.take_comments();
            if self.lex.eof() {
                // The final run has no following statement to own it (the document trailer belongs to the ROOT, which a
                // scoped walk answer never is).
                break;
            }
            // A comment run whose next token is a `[header]` is the FOOT of the table that just closed — the walk's
            // armed Table collector — never the next table's leading. The grammar halves of this law make the whole
            // route answer alike.
            if self.lex.peek() == Some(b'[') && !comments.is_empty() {
                if let Collector::Table { foot, .. } = &mut self.collector {
                    foot.extend(comments);
                }
                self.statement_comments = Vec::new();
            } else {
                // The trivia just consumed belongs to THIS statement (the leading comments); the statement's own inline
                // comment joins the value answer during the assignment walk.
                self.statement_comments = comments;
            }
            let start = self.lex.offset;
            match self.lex.peek() {
                Some(b'[') => {
                    let flat = self.walk_header(start)?;
                    self.collect_statement(start, &flat);
                }
                Some(_) => {
                    let (landing, _) = self.walk_assignment(start)?;
                    self.collect_statement(start, &landing);
                }
                None => break,
            }
        }
        Ok(self.finish())
    }

    /// Records one statement into the armed collector: headers report the opened flat path, assignments their landing
    /// table path.
    fn collect_statement(&mut self, start: usize, flat: &Path) {
        let span = Span::try_from_usize(start, self.lex.offset).unwrap_or_default();
        match &mut self.collector {
            Collector::Idle => {}
            Collector::Table { target, spans, .. } => {
                if flat.starts_with(target) {
                    spans.push(span);
                }
            }
            Collector::Range {
                container,
                start: low,
                elements,
                ..
            } => {
                // The container-relative component must be an ELEMENT: a name part (an unrelated header deeper than the
                // container, or a sub-table inside an element) carries an intern id that collides with a real element
                // slot.
                if let Some(part) = flat.0.get(container.0.len()).filter(|part| part.is_elem()) {
                    let index = part.elem_index();
                    // The recording floor: the authored start clamped at the head. A strictly-negative start records
                    // from 0 (the len-relative floor resolves only at EOF); a non-negative one lets elements below it
                    // be skipped.
                    let low = clamp_bound(*low);
                    if (index as usize) >= low {
                        let slot = (index as usize) - low;
                        if elements.len() <= slot {
                            elements.resize(slot + 1, Vec::new());
                        }
                        elements[slot].push(span);
                    }
                }
            }
        }
    }

    // ---- statement framing (mirrors the grammar's parse_header / ---- parse_assignment framing exactly, on the shared
    // lexer) ----

    fn walk_header(&mut self, start: usize) -> Result<Path, CodecError> {
        self.lex.bump(); // '['
        let is_array = self.lex.peek() == Some(b'[');
        if is_array {
            self.lex.bump();
        }
        self.lex.skip_ws();
        let mut path = self.lex.parse_key_path()?;
        self.doc.intern_keys(&self.lex, &mut path);
        self.lex.check_path_depth(path.len())?;
        self.lex.skip_ws();
        if self.lex.bump() != Some(b']') {
            return Err(self.lex.syntax(start, "invalid-header", "unterminated table header"));
        }
        if is_array && self.lex.bump() != Some(b']') {
            return Err(self
                .lex
                .syntax(start, "invalid-header", "unterminated array-of-tables header"));
        }
        self.lex.require_statement_end(start)?;
        // Drain the header's own trailing comment here — it is inside the collected statement span (the re-parse
        // re-attaches it as the table's leading), and a leftover would be claimed as the next statement's leading set.
        self.lex.take_comments();
        let flat = if is_array {
            self.doc
                .open_array_of_tables(&path, start)
                .map_err(|error| self.lex.syntax(error.offset, error.code, error.message))?
        } else {
            self.doc
                .open_table(&path, start)
                .map_err(|error| self.lex.syntax(error.offset, error.code, error.message))?
        };
        self.on_header(&path, &flat, is_array);
        Ok(flat)
    }

    fn walk_assignment(&mut self, start: usize) -> Result<(Path, Key), CodecError> {
        let mut path = self.lex.parse_key_path()?;
        self.doc.intern_keys(&self.lex, &mut path);
        self.lex.check_path_depth(self.doc.current.key_depth() + path.len())?;
        self.lex.skip_ws();
        if self.lex.bump() != Some(b'=') {
            return Err(self.lex.syntax(
                self.lex.offset.saturating_sub(1),
                "expected-key-value",
                "expected '=' after key",
            ));
        }
        self.lex.skip_ws();
        let value_start = self.lex.offset;
        let (_, landing, key) = self
            .doc
            .resolve_assignment(&path, start)
            .map_err(|error| self.lex.syntax(error.offset, error.code, error.message))?;
        // Whether the answer is already resolved before this statement. The walk continues validating the whole input
        // AFTER the target is found, and `on_assignment` short-circuits once the answer exists — so the trailing
        // comment of every LATER statement must not be appended to the already-resolved Value answer.
        let answered_before = self.answer.is_some();
        let statement_value_before = self.statement_value_answer;
        self.on_assignment(&path, &landing, value_start)?;
        // Comments buffered DURING the value parse — the interior lines of a multi-line array or inline table —
        // belong to the value's COMMENT fact, not its INLINE fact: split them off before the statement-end check, which
        // owns only the own-line trailing comment (the floor's Commented wrapper draws the same boundary).
        let interior = self.lex.take_comments();
        self.lex.require_statement_end(start)?;
        // Drain the comment buffer at EVERY statement end, not only the answer's — a trailing comment left in the
        // buffer would be claimed as the NEXT statement's leading set. The answer statement's own trailing comment is
        // the value's INLINE set (its leading set was captured at the loop top); a descent answer (a member of an
        // inline table or an array element) carries no comments at all, matching the floor.
        let trailing = self.lex.take_comments();
        if !answered_before
            && self.statement_value_answer
            && !statement_value_before
            && let Some(LocatedWalk::Value { leading, inline, .. }) = &mut self.answer
        {
            leading.extend(interior);
            inline.extend(trailing);
        }
        Ok((landing, key))
    }

    // ---- the target tracker ----

    /// Handles one opened table. The header's flat path must extend the resolved container; its key components
    /// (relative to the container's own key components) resolve the target's leading member steps, and an
    /// array-of-tables' element index resolves a matching index step. A fully resolved target with a table at its last
    /// step is the Table answer.
    #[expect(
        clippy::too_many_lines,
        reason = "one header dispatch: the pending-index boundary, the container strip, and the element/index/range arms are one walk"
    )]
    fn on_header(&mut self, components: &[Key], flat: &Path, is_array: bool) {
        // A pending negative index resolves the remaining steps against EVERY element of its array: each `[[array]]`
        // header finalizes the previous element's resolution, resets, and begins the next element's. The LAST element's
        // resolution is the answer (or the element the negative index wraps to, at `finish`).
        //
        // Each array-of-tables header arms its OWN frame, and a header arming inside another frame's current element
        // STACKS on it. The boundary below matches the one frame whose array opens its next element, unwinds every
        // frame above it first (innermost first: each resolution becomes the frame below's current-element answer), and
        // finalizes against that frame — so an inner wrap composes into the outer element's stash instead of
        // clobbering it.
        if let Some(depth) = self
            .pending
            .iter()
            .rposition(|frame| is_element_boundary(frame, flat, is_array))
        {
            while self.pending.len() > depth + 1 {
                let resolution = self.resolve_top_frame();
                self.pending.last_mut().expect("the unwound frame's parent").resolved = Some(resolution);
            }
            // `resolved` is taken OUT of the frame first: it is the finalize input and must not read as an unset frame
            // later.
            let (index_step, handed) = {
                let frame = self.pending.last_mut().expect("armed above");
                (frame.step, frame.resolved.take())
            };
            // An element's resolution is in flight from the moment the array's first element opened — arming IS that
            // opening — until the next boundary or EOF, however far the remaining steps descended into the element's
            // own sub-tables. Every boundary therefore finalizes unconditionally: gating the stash on the container
            // still sitting exactly at the element level drops the finished answer when a deeper header widened the
            // container.
            let resolution = self.finalize_pending_element(index_step, handed);
            self.pending.last_mut().expect("armed above").stash.push(resolution);
            self.answer = None;
            self.collector = Collector::Idle;
            self.container = flat.clone();
            self.step = index_step + 1;
            if self.step == self.steps.len() {
                // No suffix: the element table itself is the answer.
                self.answer_table(flat.clone());
            }
            return;
        }
        if self.answer.is_some() {
            return;
        }
        // The header must extend the resolved container.
        if !flat.starts_with(&self.container) {
            return;
        }
        // Strip the container's key components from the header's path; a mismatch means the header is unrelated to the
        // target chain.
        let mut stripped = 0usize;
        for part in &self.container.0 {
            if !part.is_elem() {
                let name = self.doc.name_text(part.name_id());
                if components.get(stripped).map(|key| self.doc.name_text(key.id)) != Some(name) {
                    return;
                }
                stripped += 1;
            }
        }
        let relative = &components[stripped..];
        // The common prefix of the relative components and the remaining member steps.
        let mut k = 0usize;
        while k < relative.len() && self.step + k < self.steps.len() {
            match (&self.steps[self.step + k], self.doc.name_text(relative[k].id)) {
                (ScopedStep::Member(name), text) if name.as_str() == text => k += 1,
                _ => break,
            }
        }
        // A standard header whose path diverges from the target chain after the container is unrelated.
        if k == 0 && !is_array {
            return;
        }
        let mut prefix = self.container.clone();
        for component in &relative[..k] {
            prefix = prefix.push_key_id(component.id);
        }
        if k < relative.len() {
            // The header diverges from the target after k components: the k-th target step is a TABLE (the header
            // creates it), so a fully consumed target answers with that table.
            if self.step + k == self.steps.len() {
                self.answer_table(prefix);
                return;
            }
            self.header_next_step_is_not_a_table(prefix, k);
            return;
        }
        // All relative components matched.
        if is_array {
            // The last component is an array-of-tables; the opened element may resolve the next step.
            if self.step + k == self.steps.len() {
                self.answer_table(prefix);
                return;
            }
            let Some(element) = flat.last_part().filter(|part| part.is_elem()) else {
                self.set_answer(LocatedWalk::TypeMismatch {
                    step: self.step + k,
                    actual: ValueKind::Array,
                });
                return;
            };
            let element = element.elem_index();
            // The element-level match handles every step kind; returning it keeps the table-header fallthrough below
            // unreachable.
            return match &self.steps[self.step + k] {
                ScopedStep::Index(index) if u32::try_from(*index).is_ok_and(|i| i == element) => {
                    self.step += k + 1;
                    self.container = flat.clone();
                    if self.step == self.steps.len() {
                        self.answer_table(flat.clone());
                    }
                }
                ScopedStep::Index(index) if *index < 0 => {
                    // A negative index cannot resolve during the single statement pass (the element count is only known
                    // at EOF): resolve the remaining steps against every element and pick `len + index` at finish, with
                    // the tree navigator's wrapping law. The current header is the FIRST element, so its resolution
                    // starts here; later elements are started by the pending hook at their own headers. A header arming
                    // inside ANOTHER frame's current element pushes its own frame — it never overwrites it: the outer
                    // boundary unwinds the stack and hands the inner selection up as the outer element's resolution.
                    self.pending.push(PendingIndex {
                        step: self.step + k,
                        index: *index,
                        array: prefix.clone(),
                        stash: Vec::new(),
                        resolved: None,
                    });
                    self.step += k + 1;
                    self.container = flat.clone();
                    if self.step == self.steps.len() {
                        // No suffix: the element table itself is the answer.
                        self.answer_table(flat.clone());
                    }
                }
                ScopedStep::Index(_) => {
                    // A different element: the target's element may open later; the container is the ARRAY itself.
                    self.step += k;
                    self.container = prefix;
                }
                ScopedStep::Range { .. } => {
                    self.step += k;
                    self.container = prefix.clone();
                    self.arm_range(prefix);
                }
                ScopedStep::Member(_) => {
                    self.set_answer(LocatedWalk::TypeMismatch {
                        step: self.step + k,
                        actual: ValueKind::Array,
                    });
                }
            };
        }
        // A standard table header: all k components resolved as tables.
        if self.step + k == self.steps.len() {
            self.answer_table(prefix);
            return;
        }
        self.header_next_step_is_not_a_table(prefix, k);
    }

    /// The step after k resolved member steps addresses the table `prefix`, which only member steps may enter.
    fn header_next_step_is_not_a_table(&mut self, prefix: Path, k: usize) {
        match &self.steps[self.step + k] {
            ScopedStep::Index(_) | ScopedStep::Range { .. } => {
                // A header descending PAST an array-of-tables belongs to some ELEMENT's subtree — the walk may still
                // be waiting at the index step for a LATER element to open — so it proves nothing about the
                // container's kind and is ignored until the target element's own boundary arrives (the floor reads only
                // the selected element). Over a genuine table the header IS the kind proof, and the object mismatch
                // stands.
                if self.doc.array_count(&prefix).is_some() {
                    return;
                }
                self.set_answer(LocatedWalk::TypeMismatch {
                    step: self.step + k,
                    actual: ValueKind::Object,
                });
            }
            ScopedStep::Member(_) => {
                self.container = prefix;
                self.step += k;
            }
        }
    }

    /// Handles one assignment in the CURRENT table (the walk matches only assignments landing in the resolved
    /// container, exactly the members the tree navigator would see there): the key path's components match the target's
    /// leading member steps, and the value either IS the answer (full match), descends (a container the remaining steps
    /// enter), or creates the answer TABLE (a dotted path continuing past the target).
    fn on_assignment(&mut self, keypath: &[Key], landing: &Path, value_start: usize) -> Result<(), CodecError> {
        if self.answer.is_some() || self.doc.current != self.container {
            self.lex.parse_value()?;
            return Ok(());
        }
        // The common member prefix of the key path and the remaining steps.
        let mut t = 0usize;
        while t < keypath.len() && self.step + t < self.steps.len() {
            match (&self.steps[self.step + t], self.doc.name_text(keypath[t].id)) {
                (ScopedStep::Member(name), text) if name.as_str() == text => t += 1,
                _ => break,
            }
        }
        if t == 0 {
            self.lex.parse_value()?;
            return Ok(());
        }
        let remaining = self.steps.len() - self.step;
        if t == remaining && t == keypath.len() {
            // The full target matched: the located value IS this value. Its LEADING comments were captured at the loop
            // top; the inline (own-line trailing) comment joins it in `walk_assignment` after it validates the
            // statement end.
            let leading = core::mem::take(&mut self.statement_comments);
            self.statement_value_answer = true;
            self.lex.parse_value()?;
            self.set_answer(LocatedWalk::Value {
                start: value_start,
                end: self.lex.offset,
                leading,
                inline: Vec::new(),
            });
            return Ok(());
        }
        if t == keypath.len() && t < remaining {
            // The value is the member at the last matched step; the remaining steps must resolve INSIDE it.
            return self.navigate_value(value_start, &self.steps[self.step + t..], self.step + t);
        }
        if t == remaining && t < keypath.len() {
            // The dotted path continues past the target: the target's last step is a TABLE — the landing table.
            self.answer_table(landing.clone());
            self.lex.parse_value()?;
            return Ok(());
        }
        if t < remaining && t < keypath.len() && !matches!(self.steps[self.step + t], ScopedStep::Member(_)) {
            // A numeric index or range over the implicit table a dotted key synthesized: the same object mismatch a
            // `[header]` table reports. Falling through would finish as Missing (null).
            self.lex.parse_value()?;
            self.set_answer(LocatedWalk::TypeMismatch {
                step: self.step + t,
                actual: ValueKind::Object,
            });
            return Ok(());
        }
        // The key path diverges after t components; the value is unrelated, but the matched prefix still counts toward
        // a later Missing's step.
        self.deepest_cover = self.deepest_cover.max(self.step + t);
        self.lex.parse_value()?;
        Ok(())
    }

    /// Arms the Table answer: the located table's subtree statement spans are collected from this statement on.
    fn answer_table(&mut self, target: Path) {
        if matches!(self.collector, Collector::Table { .. }) {
            return;
        }
        self.collector = Collector::Table {
            target,
            spans: Vec::new(),
            foot: Vec::new(),
        };
    }

    /// Arms the Range answer over the array-of-tables at `container`.
    fn arm_range(&mut self, container: Path) {
        if matches!(self.collector, Collector::Range { .. }) {
            return;
        }
        let ScopedStep::Range { start, end } = &self.steps[self.step] else {
            unreachable!("arm_range is only reached from the Range step arm");
        };
        let (start, end) = (*start, *end);
        self.collector = Collector::Range {
            container,
            start,
            end,
            elements: Vec::new(),
        };
    }

    fn set_answer(&mut self, answer: LocatedWalk) {
        self.answer = Some(answer);
    }

    // ---- the navigated value descent ----

    /// Scans one value region (validating it with the shared lexer) while resolving the remaining steps.
    /// `self.lex.offset` must equal `start` on entry; it is left at the region's end on return.
    fn navigate_value(&mut self, start: usize, steps: &[ScopedStep], step_offset: usize) -> Result<(), CodecError> {
        debug_assert_eq!(self.lex.offset, start);
        match self.lex.peek() {
            Some(b'{') => self.navigate_inline_table(start, steps, step_offset),
            Some(b'[') => self.navigate_array(start, steps, step_offset),
            _ => {
                // A scalar: validate it; the remaining steps mismatch its kind.
                let value = self.lex.parse_value()?;
                self.set_answer(LocatedWalk::TypeMismatch {
                    step: step_offset,
                    actual: tree_kind(&value),
                });
                Ok(())
            }
        }
    }

    /// Scans one inline-table region, resolving the leading member step.
    ///
    /// Entries carry DOTTED key paths (`{ type.name = "pug" }`), exactly as the whole-document builder admits them: the
    /// shared [`Lexer::record_inline_key`] enforces the same duplicate/conflict laws, and the target's remaining steps
    /// resolve against each path — a full match answers the entry's VALUE, a longer path descends into the value, a
    /// target ending at a dotted path's synthesized ancestor collects the [`LocatedWalk::ImplicitTable`] pieces, and a
    /// divergence records the deepest covered depth for the missing-step law.
    #[expect(
        clippy::too_many_lines,
        reason = "one inline-table scan: the shared key-path validation, the five-step resolution, and the 1.0/1.1 separator laws"
    )]
    fn navigate_inline_table(
        &mut self,
        start: usize,
        steps: &[ScopedStep],
        step_offset: usize,
    ) -> Result<(), CodecError> {
        let ScopedStep::Member(_) = &steps[0] else {
            // An index or range over an object is a kind mismatch.
            self.lex.parse_value()?;
            self.set_answer(LocatedWalk::TypeMismatch {
                step: step_offset,
                actual: ValueKind::Object,
            });
            return Ok(());
        };
        self.lex.bump(); // '{'
        // The dotted-key validation state — the SAME laws the whole-document builder enforces, so the walk rejects
        // exactly what it rejects (a single key duplicated, or a dotted key extending a value).
        let mut seen: BTreeMap<u32, KeySeen> = BTreeMap::new();
        // The deepest target depth any entry's path covered (as an implicit table), for the missing-step law when
        // nothing resolves.
        let mut deepest_cover = 0usize;
        // The implicit-table answer's collected pieces, when the target ends at a dotted key's synthesized ancestor.
        let mut implicit: Vec<(Vec<String>, usize, usize)> = Vec::new();
        self.lex.skip_inline_trivia()?;
        if self.lex.peek() == Some(b'}') {
            self.lex.bump();
            self.set_answer(LocatedWalk::Missing { step: step_offset });
            return Ok(());
        }
        loop {
            let path = self.lex.parse_key_path()?;
            // The same landing-depth ceiling the whole-document builder enforces for inline-table paths: without it, a
            // deep dotted key recurses once per component in `record_inline_key` and `insert_inline_dotted` and
            // overflows the request stack.
            self.lex.check_path_depth(path.len())?;
            self.lex.record_inline_key(&mut seen, &path, start)?;
            self.lex.skip_ws();
            if self.lex.bump() != Some(b'=') {
                return Err(self
                    .lex
                    .syntax(start, "invalid-inline-table", "expected '=' in inline table"));
            }
            self.lex.skip_ws();
            let entry_value_start = self.lex.offset;
            // Resolve the target's remaining steps against this entry's dotted key path: `k` is the length of the
            // common member prefix (a non-member step stops the prefix at its position).
            let mut k = 0usize;
            while k < steps.len() && k < path.len() {
                match (&steps[k], self.lex.name_text(path[k].id)) {
                    (ScopedStep::Member(name), text) if name.as_str() == text => k += 1,
                    _ => break,
                }
            }
            // The entry's value is ALWAYS validated; a resolution only records the answer (and may descend into the
            // entry), while the scan keeps consuming the rest of the table.
            self.lex.parse_value()?;
            let entry_value_end = self.lex.offset;
            if k == path.len() {
                // The whole entry path is consumed by the target prefix: the entry's value sits at the target's depth.
                // A duplicate or value-extension conflict is already rejected above, so no later entry can re-answer
                // this position.
                if k == steps.len() {
                    // The member's value carries NO comments: the statement's comments belong to the inline table's own
                    // value, never to one of its members (the floor's attachment law).
                    self.set_answer(LocatedWalk::Value {
                        start: entry_value_start,
                        end: entry_value_end,
                        leading: Vec::new(),
                        inline: Vec::new(),
                    });
                } else {
                    self.lex.offset = entry_value_start;
                    self.navigate_value(entry_value_start, &steps[k..], step_offset + k)?;
                    // The descent may leave the offset at the end of a NESTED region (an array element); the enclosing
                    // scan reads the entry separator from the value's own end.
                    self.lex.offset = entry_value_end;
                }
            } else if k == steps.len() {
                // The target ends at the implicit table this entry's dotted path creates at depth k: collect its
                // contribution (the remaining path components and the value span).
                if self.answer.is_none() {
                    implicit.push((
                        path[k..]
                            .iter()
                            .map(|key| String::from(self.lex.name_text(key.id)))
                            .collect::<Vec<_>>(),
                        entry_value_start,
                        entry_value_end,
                    ));
                }
            } else if !matches!(steps[k], ScopedStep::Member(_)) {
                // A non-member step addresses the implicit table this entry claims at depth k: an object mismatch,
                // final (a dotted key creates only tables, and no entry can re-answer the position as a value).
                if self.answer.is_none() {
                    self.set_answer(LocatedWalk::TypeMismatch {
                        step: step_offset + k,
                        actual: ValueKind::Object,
                    });
                }
            } else {
                // A member step diverges from this entry's path at depth k; a later entry may cover the depth, so only
                // the deepest covered depth is recorded for the missing-step law.
                deepest_cover = deepest_cover.max(k);
            }
            self.lex.skip_inline_trivia()?;
            match self.lex.peek() {
                Some(b',') => {
                    self.lex.bump();
                    if self.lex.dialect == DialectKind::Toml10 {
                        self.lex.skip_ws();
                        if self.lex.peek() == Some(b'}') {
                            return Err(self.lex.syntax(
                                start,
                                "invalid-inline-table",
                                "trailing comma in inline table",
                            ));
                        }
                    } else {
                        self.lex.skip_ws_comments()?;
                        // TOML 1.1 allows a trailing comma before the close.
                        if self.lex.peek() == Some(b'}') {
                            self.lex.bump();
                            break;
                        }
                    }
                }
                Some(b'}') => {
                    self.lex.bump();
                    break;
                }
                _ => {
                    return Err(self.lex.syntax(
                        self.lex.offset,
                        "invalid-inline-table",
                        "expected ',' or '}' in inline table",
                    ));
                }
            }
        }
        if self.answer.is_none() {
            if implicit.is_empty() {
                self.set_answer(LocatedWalk::Missing {
                    step: step_offset + deepest_cover,
                });
            } else {
                self.set_answer(LocatedWalk::ImplicitTable { pieces: implicit });
            }
        }
        Ok(())
    }

    /// Scans one array region, resolving the leading index or range step.
    fn navigate_array(&mut self, start: usize, steps: &[ScopedStep], step_offset: usize) -> Result<(), CodecError> {
        match &steps[0] {
            ScopedStep::Index(_) | ScopedStep::Range { .. } => {}
            ScopedStep::Member(_) => {
                // A member over an array is a kind mismatch.
                self.lex.parse_value()?;
                self.set_answer(LocatedWalk::TypeMismatch {
                    step: step_offset,
                    actual: ValueKind::Array,
                });
                return Ok(());
            }
        }
        // Scan the array's element regions (validating each with the shared value grammar). The scan consumes the
        // closing `]`; a later descent into one element must restore that end, never the element's end (which sits
        // before the already-consumed `]` and reads as trailing content — or swallows the element's own type error).
        self.lex.bump(); // '['
        let mut regions: Vec<(usize, usize)> = Vec::new();
        loop {
            self.lex.skip_trivia()?;
            match self.lex.peek() {
                Some(b']') => {
                    self.lex.bump();
                    break;
                }
                None => {
                    return Err(self.lex.syntax(start, "invalid-array", "unterminated array"));
                }
                _ => {
                    let element_start = self.lex.offset;
                    self.lex.parse_value()?;
                    regions.push((element_start, self.lex.offset));
                    self.lex.skip_trivia()?;
                    match self.lex.peek() {
                        Some(b',') => {
                            self.lex.bump();
                        }
                        Some(b']') => {
                            self.lex.bump();
                            break;
                        }
                        _ => {
                            return Err(self.lex.syntax(
                                self.lex.offset,
                                "invalid-array",
                                "expected ',' or ']' in array",
                            ));
                        }
                    }
                }
            }
        }
        let after_array = self.lex.offset;
        match &steps[0] {
            ScopedStep::Index(index) => match jqf_data::resolve_index(regions.len(), *index) {
                Some(position) => {
                    let (element_start, element_end) = regions[position];
                    if steps.len() == 1 {
                        // The element's value carries NO comments: the statement's comments belong to the ARRAY's own
                        // value, never to one of its elements.
                        self.set_answer(LocatedWalk::Value {
                            start: element_start,
                            end: element_end,
                            leading: Vec::new(),
                            inline: Vec::new(),
                        });
                    } else {
                        // The remaining steps resolve inside the element.
                        self.lex.offset = element_start;
                        self.navigate_value(element_start, &steps[1..], step_offset + 1)?;
                        self.lex.offset = after_array;
                    }
                }
                None => {
                    self.set_answer(LocatedWalk::Missing { step: step_offset });
                }
            },
            ScopedStep::Range { start: rs, end: re } => {
                let (lo, hi) = resolve_range(regions.len(), *rs, *re);
                if lo >= hi {
                    self.set_answer(LocatedWalk::RangeValue {
                        start: 0,
                        end: 0,
                        empty: true,
                    });
                } else {
                    self.set_answer(LocatedWalk::RangeValue {
                        start: regions[lo].0,
                        end: regions[hi - 1].1,
                        empty: false,
                    });
                }
            }
            ScopedStep::Member(_) => unreachable!("handled above"),
        }
        Ok(())
    }

    // ---- EOF resolution ----

    /// Resolves and pops the INNERMOST open frame: its current element finalizes into the frame's per-element stash and
    /// `len + index` selects the wrapped element. The caller hands the result to the frame below (`resolved`) or
    /// publishes it as the walk's answer.
    fn resolve_top_frame(&mut self) -> LocatedWalk {
        let frame = self.pending.pop().expect("a pending frame is open");
        let resolution = self.finalize_pending_element(frame.step, frame.resolved);
        let mut stash = frame.stash;
        stash.push(resolution);
        match jqf_data::resolve_index(stash.len(), frame.index) {
            Some(position) => core::mem::replace(&mut stash[position], LocatedWalk::Missing { step: frame.step }),
            None => LocatedWalk::Missing { step: frame.step },
        }
    }

    /// Finalizes the current element's pending-negative resolution: a resolution HANDED UP by a deeper frame's unwind
    /// wins (this frame's remaining steps descended through that frame's array); otherwise the set answer, the armed
    /// Table collector's spans, or a Missing at the first suffix step.
    fn finalize_pending_element(&mut self, index_step: usize, handed: Option<LocatedWalk>) -> LocatedWalk {
        if let Some(resolution) = handed {
            return resolution;
        }
        if let Some(answer) = self.answer.take() {
            return answer;
        }
        if let Collector::Table {
            target, spans, foot, ..
        } = core::mem::replace(&mut self.collector, Collector::Idle)
        {
            table_answer(&target, spans, foot)
        } else {
            // The resolved element is a table and the next suffix step indexes or slices it: the same typed mismatch
            // every other table arm reports, never a Missing (the floor raises here).
            if matches!(
                self.steps.get(index_step + 1),
                Some(ScopedStep::Index(_) | ScopedStep::Range { .. })
            ) {
                return LocatedWalk::TypeMismatch {
                    step: index_step + 1,
                    actual: ValueKind::Object,
                };
            }
            LocatedWalk::Missing { step: index_step + 1 }
        }
    }

    fn finish(mut self) -> LocatedWalk {
        if !self.pending.is_empty() {
            // Every frame's resolution is the frame below's current-element answer, so the stack unwinds INNERMOST
            // first and the OUTERMOST frame's selection — the element its negative index wraps to — is the walk's
            // answer. The outermost frame opened with its arming header and its resolution stays in flight across
            // descents into its own sub-tables, so EOF finalizes it unconditionally; every earlier element was already
            // finalized at its successor's header.
            while self.pending.len() > 1 {
                let resolution = self.resolve_top_frame();
                self.pending.last_mut().expect("the unwound frame's parent").resolved = Some(resolution);
            }
            return self.resolve_top_frame();
        }
        if let Some(answer) = self.answer.take() {
            return answer;
        }
        match self.collector {
            Collector::Range {
                container,
                start,
                end,
                elements,
            } => {
                let count = self.doc.array_count(&container).unwrap_or(0);
                // See [`crate::locate::resolve_bound`].
                let lo = resolve_bound(start, count as usize);
                let hi = match end {
                    None => count as usize,
                    Some(value) => resolve_bound(Some(value), count as usize),
                };
                if lo >= hi {
                    return LocatedWalk::RangeTables { elements: Vec::new() };
                }
                // The recorded lists start at the collection floor (`clamp_bound` of the authored start: 0 for a
                // negative or open bound, the bound itself otherwise), so align before taking the selected window.
                let recorded_from = clamp_bound(start).min(lo);
                let elements = elements.into_iter().skip(lo - recorded_from).take(hi - lo).collect();
                LocatedWalk::RangeTables { elements }
            }
            Collector::Table {
                target, spans, foot, ..
            } => table_answer(&target, spans, foot),
            Collector::Idle => {
                // An unresolved Index/Range step over the resolved table is the same typed mismatch every other table
                // arm reports — unless the container is an ARRAY-OF-TABLES still waiting for its element to open: an
                // element that never opened is out of bounds, the tree navigator's Missing, never an object mismatch
                // (an array-of-tables indexes; a table does not).
                let index_step_over_a_table = matches!(
                    self.steps.get(self.step),
                    Some(ScopedStep::Index(_) | ScopedStep::Range { .. })
                ) && self.doc.array_count(&self.container).is_none();
                if index_step_over_a_table {
                    return LocatedWalk::TypeMismatch {
                        step: self.step,
                        actual: ValueKind::Object,
                    };
                }
                LocatedWalk::Missing {
                    step: self.step.max(self.deepest_cover),
                }
            }
        }
    }
}

/// Whether `flat` opens the NEXT element of a frame's array — the frame's boundary shape: an array-of-tables header
/// exactly one part deeper than the array's flat path. Frames nest at distinct depths, so at most one open frame
/// matches any header.
fn is_element_boundary(frame: &PendingIndex, flat: &Path, is_array: bool) -> bool {
    is_array && flat.starts_with(&frame.array) && flat.0.len() == frame.array.0.len() + 1
}

/// The walk's table answer carries the exact target the materializer honors.
fn table_answer(target: &Path, spans: Vec<Span>, foot: Vec<String>) -> LocatedWalk {
    LocatedWalk::Table {
        spans,
        foot,
        key_depth: target.key_depth(),
        element: target.ends_with_element(),
    }
}

/// Clamps a non-negative authored bound to `usize`.
fn clamp_bound(value: Option<i64>) -> usize {
    value.map_or(0, |value| usize::try_from(value.max(0)).unwrap_or(usize::MAX))
}
#[cfg(test)]
mod tests {
    use super::*;
    use alloc::borrow::ToOwned as _;
    use alloc::string::String;
    use alloc::vec;

    use crate::provider::DialectKind;
    use jqf_data::Value;
    use jqf_source::{SourceId, SourceKind, SourceRef};

    fn source(bytes: &[u8]) -> ResolvedSource<'_> {
        ResolvedSource::new(
            SourceRef::new(SourceId::new(98), SourceKind::Input),
            "walk.toml",
            bytes,
            0,
        )
    }

    fn member(name: &str) -> ScopedStep {
        ScopedStep::Member(String::from(name))
    }

    fn walk(bytes: &[u8], steps: &[ScopedStep]) -> LocatedWalk {
        let resources = crate::test_support::resources();
        Walker::try_new(source(bytes), DialectKind::Toml10, steps, &resources, true)
            .walk()
            .expect("walk")
    }

    fn value_of(bytes: &[u8], located: &LocatedWalk) -> String {
        let LocatedWalk::Value { start, end, .. } = located else {
            panic!("expected a value, got {located:?}");
        };
        String::from_utf8(bytes[*start..*end].to_vec()).expect("utf8")
    }

    fn piece_of(bytes: &[u8], piece: &(Vec<String>, usize, usize)) -> String {
        String::from_utf8(bytes[piece.1..piece.2].to_vec()).expect("utf8")
    }

    #[test]
    fn member_into_a_table_resolves_a_value_span() {
        let bytes = b"a = 1\n[t]\nx = 2\n[[arr]]\ny = 3\n[[arr]]\ny = 4\n";
        let located = walk(bytes, &[member("t"), member("x")]);
        assert_eq!(value_of(bytes, &located), "2");
        let located = walk(bytes, &[member("arr"), ScopedStep::Index(1), member("y")]);
        assert_eq!(value_of(bytes, &located), "4");
        // A table-typed answer collects its subtree's statement spans.
        let located = walk(bytes, &[member("t")]);
        let LocatedWalk::Table { spans, .. } = located else {
            panic!("expected a table, got {located:?}");
        };
        assert_eq!(spans.len(), 2, "the header and its assignment");
        let located = walk(bytes, &[member("arr")]);
        assert!(matches!(located, LocatedWalk::Table { .. }));
    }

    #[test]
    fn negative_index_over_an_array_of_tables_wraps_at_eof() {
        // The single statement pass cannot resolve a negative index until the element count is known: every element's
        // resolution is stashed and `len + index` picks the answer at finish.
        let bytes = b"[[arr]]\nx = 1\n[[arr]]\nx = 2\n";
        // No suffix: the last element's table.
        let located = walk(bytes, &[member("arr"), ScopedStep::Index(-1)]);
        let LocatedWalk::Table { spans, .. } = &located else {
            panic!("expected the last element table, got {located:?}");
        };
        assert_eq!(spans.len(), 2, "the last [[arr]] header and its assignment");
        let located = walk(bytes, &[member("arr"), ScopedStep::Index(-2)]);
        let LocatedWalk::Table { spans, .. } = &located else {
            panic!("expected the first element table, got {located:?}");
        };
        assert_eq!(spans.len(), 2);
        // A suffix resolves inside the LAST element: element 1's `x`.
        let located = walk(bytes, &[member("arr"), ScopedStep::Index(-1), member("x")]);
        assert_eq!(value_of(bytes, &located), "2");
        let located = walk(bytes, &[member("arr"), ScopedStep::Index(-2), member("x")]);
        assert_eq!(value_of(bytes, &located), "1");
        // Out of bounds is Missing, exactly the tree navigator's observation.
        let located = walk(bytes, &[member("arr"), ScopedStep::Index(-3)]);
        assert!(matches!(located, LocatedWalk::Missing { .. }));
        // A missing array is Missing too (a TOML source cannot express an empty array-of-tables: every `[[arr]]` header
        // creates an element).
        let bytes = b"x = 1\n";
        let located = walk(bytes, &[member("arr"), ScopedStep::Index(-1)]);
        assert!(matches!(located, LocatedWalk::Missing { .. }));
        // The last element MISSING the member answers Missing, never an earlier element's value.
        let bytes = b"[[arr]]\nx = 1\n[[arr]]\ny = 2\n";
        let located = walk(bytes, &[member("arr"), ScopedStep::Index(-1), member("x")]);
        assert!(matches!(located, LocatedWalk::Missing { .. }));
    }

    #[test]
    fn negative_index_resolution_survives_a_descent_past_the_element_level() {
        // An element's resolution stays in flight from the moment its `[[header]]` opened it until the next boundary or
        // EOF, however far the suffix descended into the element's own sub-tables: gating the boundary stash on the
        // container still sitting exactly at the element level drops the finished answer instead.
        let bytes = b"[[a]]\nx = 1\n[[a]]\n[a.sub.deep]\ny = 2\n";
        let located = walk(
            bytes,
            &[
                member("a"),
                ScopedStep::Index(-1),
                member("sub"),
                member("deep"),
                member("y"),
            ],
        );
        assert_eq!(value_of(bytes, &located), "2");
        // The same law for an EARLIER element: its descent must reach its own boundary's stash, not vanish before the
        // last element opens.
        let bytes = b"[[a]]\n[a.sub]\ny = 1\n[[a]]\nx = 2\n";
        let located = walk(bytes, &[member("a"), ScopedStep::Index(-2), member("sub"), member("y")]);
        assert_eq!(value_of(bytes, &located), "1");
    }

    #[test]
    fn nested_negative_indices_wrap_at_both_levels() {
        // TWO stacked frames: the outer `.x[-1]` wraps at EOF to the LAST `x` element, and the inner `.a[-1]` — one
        // frame per `x` element, handed up when the outer boundary fires — wraps inside THAT element. The single
        // pending slot used to be CLOBBERED when the inner `[[x.a]]` armed: the outer frame vanished, and the walk
        // published the FIRST outer element's value (7) where the floor answers the inner wrap of the LAST outer
        // element (8).
        let bytes = b"[[x]]\n[[x.a]]\ny = 7\n[[x]]\n[[x.a]]\ny = 8\n";
        let steps = |outer: i64, inner: i64| {
            vec![
                member("x"),
                ScopedStep::Index(outer),
                member("a"),
                ScopedStep::Index(inner),
                member("y"),
            ]
        };
        let located = walk(bytes, &steps(-1, -1));
        assert_eq!(
            value_of(bytes, &located),
            "8",
            "the inner wrap of the LAST outer element"
        );
        let located = walk(bytes, &steps(0, -1));
        assert_eq!(value_of(bytes, &located), "7");
        // Without the scalar suffix both wraps still select the innermost element TABLE, and it materializes as that
        // element.
        let located = walk(
            bytes,
            &[member("x"), ScopedStep::Index(-1), member("a"), ScopedStep::Index(-1)],
        );
        let Value::Object(object) = materialize_walk(bytes, &located) else {
            panic!("expected the wrapped element table, got {located:?}");
        };
        let Value::Number(y) = object.get("y").expect("y present") else {
            panic!("y is not a number");
        };
        assert_eq!(y.to_i64(), Some(8));

        // The inner frame's own multi-element stash composes per OUTER element: each `x` element's unwind picks its OWN
        // inner wrap, and the outer stash holds those compositions in element order.
        let bytes = b"[[x]]\n[[x.a]]\ny = 7\n[[x.a]]\ny = 9\n[[x]]\n[[x.a]]\ny = 8\n";
        let located = walk(bytes, &steps(-1, -1));
        assert_eq!(value_of(bytes, &located), "8", "the LAST x's only a");
        let located = walk(bytes, &steps(0, -2));
        assert_eq!(value_of(bytes, &located), "7", "first a of the FIRST x");
        let located = walk(bytes, &steps(0, -1));
        assert_eq!(value_of(bytes, &located), "9", "last a of the FIRST x");
        // The LAST `x` has ONE `a`, so its inner `-2` is out of bounds: the composed answer is Missing at the inner
        // index step even though an EARLIER outer element's composition was a value.
        let located = walk(bytes, &steps(-1, -2));
        assert!(
            matches!(located, LocatedWalk::Missing { step: 3 }),
            "expected Missing at the inner index step, got {located:?}"
        );
    }

    #[test]
    fn triple_nested_negative_indices_unwind_to_the_outermost_wrap() {
        // Three stacked frames unwind innermost-first at the outer boundary: each level's selection is the level
        // below's current-element resolution, and only the OUTERMOST wrap answers.
        let bytes = b"[[p]]\n[[p.q]]\n[[p.q.r]]\ns = 1\n[[p]]\n[[p.q]]\n[[p.q.r]]\ns = 2\n";
        let located = walk(
            bytes,
            &[
                member("p"),
                ScopedStep::Index(-1),
                member("q"),
                ScopedStep::Index(-1),
                member("r"),
                ScopedStep::Index(-1),
                member("s"),
            ],
        );
        assert_eq!(value_of(bytes, &located), "2");
    }

    #[test]
    fn a_deeper_array_header_of_an_earlier_element_never_fires_the_descent_mismatch() {
        // The descent-path defect: while the walk waits at `.x[1]` for the SECOND element of the array-of-tables to
        // open, a `[[x.a]]` header belongs to x[0]'s subtree and must be ignored until that boundary arrives — it
        // used to be read as "x continues past the matched members" and fired the descent path's object TypeMismatch,
        // where the floor reads only the selected element and answers 8. All indices positive — no negative frame is
        // involved.
        let bytes = b"[[x]]\n[[x.a]]\ny = 7\n[[x]]\n[[x.a]]\ny = 8\n";
        let steps = |outer: i64| {
            vec![
                member("x"),
                ScopedStep::Index(outer),
                member("a"),
                ScopedStep::Index(0),
                member("y"),
            ]
        };
        assert_eq!(value_of(bytes, &walk(bytes, &steps(1))), "8");
        assert_eq!(value_of(bytes, &walk(bytes, &steps(0))), "7");
    }

    #[test]
    fn triple_nested_positive_descent_skips_earlier_elements_deeper_headers() {
        // The same law three levels deep, ALL-POSITIVE: every deeper header of an earlier element (`[[p.q]]`,
        // `[[p.q.r]]`) arrives while an outer index step is still waiting, and must never arm or answer.
        let bytes = b"[[p]]\n[[p.q]]\n[[p.q.r]]\ns = 1\n[[p]]\n[[p.q]]\n[[p.q.r]]\ns = 2\n";
        let steps = |p: i64, q: i64, r: i64| {
            vec![
                member("p"),
                ScopedStep::Index(p),
                member("q"),
                ScopedStep::Index(q),
                member("r"),
                ScopedStep::Index(r),
                member("s"),
            ]
        };
        assert_eq!(value_of(bytes, &walk(bytes, &steps(1, 0, 0))), "2");
        assert_eq!(value_of(bytes, &walk(bytes, &steps(0, 0, 0))), "1");
    }

    #[test]
    fn an_element_that_never_opens_is_out_of_bounds_not_an_object_mismatch() {
        // An array-of-tables container whose target element never opens is OUT OF BOUNDS at EOF — the tree
        // navigator's Missing — never the object mismatch a table reports: an array-of-tables indexes; a table does
        // not. The deeper headers the existing elements keep declaring are ignored, so they cannot pre-empt the
        // observation.
        let bytes = b"[[x]]\n[[x.a]]\ny = 7\n";
        let located = walk(
            bytes,
            &[
                member("x"),
                ScopedStep::Index(1),
                member("a"),
                ScopedStep::Index(0),
                member("y"),
            ],
        );
        assert!(
            matches!(located, LocatedWalk::Missing { step: 1 }),
            "expected Missing at the outer index step, got {located:?}"
        );
        // The same law for a wait INSIDE a later element: p[1]'s `q` has one element, so `q[1]` is out of bounds even
        // while `q[0]`'s subtree keeps declaring deeper tables.
        let bytes = b"[[p]]\n[[p.q]]\n[[p.q.r]]\ns = 1\n[[p]]\n[[p.q]]\n[[p.q.r]]\ns = 2\n";
        let located = walk(
            bytes,
            &[
                member("p"),
                ScopedStep::Index(1),
                member("q"),
                ScopedStep::Index(1),
                member("r"),
                ScopedStep::Index(0),
                member("s"),
            ],
        );
        assert!(
            matches!(located, LocatedWalk::Missing { step: 3 }),
            "expected Missing at the inner index step, got {located:?}"
        );
    }

    #[test]
    fn a_deeper_header_over_a_genuine_table_wait_keeps_the_object_mismatch() {
        // The ignore law is scoped to ARRAY-OF-TABLES containers: a header descending past a genuine TABLE while an
        // index step addresses it IS the kind proof the floor sees too, and the object mismatch stands.
        let bytes = b"[a]\nv = 0\n[a.sub]\nw = 1\n";
        let located = walk(bytes, &[member("a"), ScopedStep::Index(0), member("x")]);
        assert!(matches!(
            located,
            LocatedWalk::TypeMismatch {
                step: 1,
                actual: ValueKind::Object
            }
        ));
    }

    #[test]
    fn a_range_over_an_array_of_tables_ignores_unrelated_name_components() {
        // The `[q.a]` header's flat is deeper than the range's container; its second component is a NAME whose intern
        // id must not index an element slot (it collides with element 1's own slot).
        let bytes = b"[[p]]\na = 0\n[q.a]\ny = 1\n[[p]]\nb = 0\n";
        let located = walk(
            bytes,
            &[
                member("p"),
                ScopedStep::Range {
                    start: Some(0),
                    end: Some(2),
                },
            ],
        );
        let LocatedWalk::RangeTables { elements } = &located else {
            panic!("expected a range of tables, got {located:?}");
        };
        assert_eq!(elements.len(), 2);
        assert_eq!(elements[0].len(), 2, "element 0: its header and `a = 0`");
        assert_eq!(elements[1].len(), 2, "element 1: its header and `b = 0`");
    }

    #[test]
    fn a_collected_table_keeps_its_body_across_subtable_headers() {
        // `answer_table` arms once: a later `[t.sub]` header (or a later `[[p]]` element) must not re-arm the collector
        // and wipe the spans collected so far.
        let bytes = b"[t]\nx = 1\n[t.sub]\ny = 2\n";
        let located = walk(bytes, &[member("t")]);
        let LocatedWalk::Table { spans, .. } = &located else {
            panic!("expected the table, got {located:?}");
        };
        assert_eq!(spans.len(), 4, "the header, `x = 1`, `[t.sub]`, and `y = 2`");
        let bytes = b"[[p]]\nx = 1\n[[p]]\nb = 0\n";
        let located = walk(bytes, &[member("p")]);
        let LocatedWalk::Table { spans, .. } = &located else {
            panic!("expected the array table, got {located:?}");
        };
        assert_eq!(spans.len(), 4, "both element headers and their assignments");
    }

    #[test]
    fn descent_answers_carry_no_outer_statement_comments() {
        // The floor attaches a statement's comments to the statement's own VALUE, never to a descended member or array
        // element: the walk's member/element answers must publish the same empty comment set.
        let bytes = b"# list comment\nl = [1, 2]\n# point comment\npoint = { x = 1 } # trailing\n";
        let located = walk(bytes, &[member("l"), ScopedStep::Index(1)]);
        let LocatedWalk::Value { leading, inline, .. } = &located else {
            panic!("expected the element value, got {located:?}");
        };
        assert!(
            leading.is_empty() && inline.is_empty(),
            "an array element carries no comments"
        );
        let located = walk(bytes, &[member("point"), member("x")]);
        let LocatedWalk::Value { leading, inline, .. } = &located else {
            panic!("expected the member value, got {located:?}");
        };
        assert!(
            leading.is_empty() && inline.is_empty(),
            "an inline-table member carries no comments"
        );
        // The statement's OWN value keeps its leading and inline comments in their separate sets.
        let bytes = b"# lead\nx = 1 # trail\n";
        let located = walk(bytes, &[member("x")]);
        let LocatedWalk::Value { leading, inline, .. } = &located else {
            panic!("expected the statement value, got {located:?}");
        };
        assert_eq!(leading, &["lead".to_owned()]);
        assert_eq!(inline, &["trail".to_owned()]);
    }

    #[test]
    fn a_collected_table_subtree_reparses_with_its_separating_newlines() {
        // The span-join defect: each collected statement span ends BEFORE the newline that separates it from the next
        // statement (require_statement_end leaves it for the next skip_trivia), so the lazy re-parse must join the
        // spans with b'\n'. Concatenated bare, `[a]` + `x = 1` re-parsed as `[a]x = 1` and failed with
        // trailing-content.
        let bytes = b"[a]\nx = 1\ny = 2\n";
        let located = walk(bytes, &[member("a")]);
        let LocatedWalk::Table {
            spans,
            key_depth,
            element,
            ..
        } = located
        else {
            panic!("expected a table, got {located:?}");
        };
        let mut resources = crate::test_support::resources();
        let (builder, root) = crate::lazy::build_statement_table(
            bytes,
            &spans,
            &[],
            key_depth,
            element,
            DialectKind::Toml10,
            jqf_data::BuilderCoverage::minimal_semantic(),
            &mut resources,
        )
        .expect("the collected table subtree must re-parse");
        let document = builder.finish(root, &resources).expect("finish");
        let value = document.materialize_root(&mut resources).expect("materialize");
        let Value::Object(object) = value else {
            panic!("expected an object table, got {value:?}");
        };
        assert_eq!(object.len(), 2);
        let Value::Number(x) = object.get("x").expect("x present") else {
            panic!("x is not a number");
        };
        assert_eq!(x.to_i64(), Some(1));
        let Value::Number(y) = object.get("y").expect("y present") else {
            panic!("y is not a number");
        };
        assert_eq!(y.to_i64(), Some(2));
    }

    #[test]
    fn an_array_of_tables_subtree_reparses_into_its_element() {
        // Same join law over an array-of-tables header: the collected spans (`[[a]]` + both assignments) re-parse into
        // one element table.
        let bytes = b"[[a]]\nx = 1\ny = 2\n";
        let located = walk(bytes, &[member("a")]);
        let LocatedWalk::Table {
            spans,
            key_depth,
            element,
            ..
        } = located
        else {
            panic!("expected a table, got {located:?}");
        };
        let mut resources = crate::test_support::resources();
        let (builder, root) = crate::lazy::build_statement_table(
            bytes,
            &spans,
            &[],
            key_depth,
            element,
            DialectKind::Toml10,
            jqf_data::BuilderCoverage::minimal_semantic(),
            &mut resources,
        )
        .expect("the collected array-of-tables subtree must re-parse");
        let document = builder.finish(root, &resources).expect("finish");
        let value = document.materialize_root(&mut resources).expect("materialize");
        let Value::Array(items) = value else {
            panic!("expected an array of tables, got {value:?}");
        };
        assert_eq!(items.len(), 1);
        let member = |name: &str| {
            let element = items.get(0).expect("one element");
            let Value::Object(object) = element else {
                panic!("element 0 is not a table");
            };
            object.get(name).and_then(|value| match value {
                Value::Number(number) => number.to_i64(),
                _ => None,
            })
        };
        assert_eq!(member("x"), Some(1));
        assert_eq!(member("y"), Some(2));
    }

    #[test]
    fn inline_table_and_array_values_descent() {
        let bytes = b"point = { x = 1, y = 2 }\nlist = [10, 20, 30]\n";
        let located = walk(bytes, &[member("point"), member("y")]);
        assert_eq!(value_of(bytes, &located), "2");
        let located = walk(bytes, &[member("list"), ScopedStep::Index(1)]);
        assert_eq!(value_of(bytes, &located), "20");
        let located = walk(bytes, &[member("list"), ScopedStep::Index(-1)]);
        assert_eq!(value_of(bytes, &located), "30");
        let located = walk(bytes, &[member("list"), ScopedStep::Index(3)]);
        assert!(matches!(located, LocatedWalk::Missing { step: 1 }));
    }

    #[test]
    fn missing_and_mismatch_carry_the_failing_step() {
        let bytes = b"a = 1\n[t]\nx = 2\n";
        assert!(matches!(walk(bytes, &[member("z")]), LocatedWalk::Missing { step: 0 }));
        assert!(matches!(
            walk(bytes, &[member("t"), member("z")]),
            LocatedWalk::Missing { step: 1 }
        ));
        assert!(matches!(
            walk(bytes, &[member("a"), member("b")]),
            LocatedWalk::TypeMismatch {
                step: 1,
                actual: ValueKind::Number,
            }
        ));
        assert!(matches!(
            walk(bytes, &[member("a"), ScopedStep::Index(0)]),
            LocatedWalk::TypeMismatch {
                step: 1,
                actual: ValueKind::Number,
            }
        ));
        // A member over an array-of-tables is an array mismatch.
        let bytes = b"[[a]]\nx = 1\n";
        assert!(matches!(
            walk(bytes, &[member("a"), member("name")]),
            LocatedWalk::TypeMismatch {
                step: 1,
                actual: ValueKind::Array,
            }
        ));
    }

    #[test]
    fn dotted_keys_and_out_of_order_tables_resolve() {
        let bytes = b"a.b = 1\n[a.c]\ny = 2\n";
        let located = walk(bytes, &[member("a"), member("b")]);
        assert_eq!(value_of(bytes, &located), "1");
        // `.a` is a table created by the dotted key.
        assert!(matches!(walk(bytes, &[member("a")]), LocatedWalk::Table { .. }));
        // `[a.c]` after the dotted key is the same table chain.
        let located = walk(bytes, &[member("a"), member("c"), member("y")]);
        assert_eq!(value_of(bytes, &located), "2");
    }

    #[test]
    fn dotted_keys_inside_inline_tables_resolve() {
        // The scoped-walk defect: the scoped walk parsed ONE key where the grammar admits a dotted path, so `{
        // type.name = "pug" }` failed with "expected '=' in inline table" while the whole-document route answered it.
        // Every resolution below mirrors the tree navigator's answer for the same path.
        let bytes = b"animal = { type.name = \"pug\" }\n";
        // Full match: the entry's value span.
        let located = walk(bytes, &[member("animal"), member("type"), member("name")]);
        assert_eq!(value_of(bytes, &located), "\"pug\"");
        // Target ends at the dotted path's implicit table: the collected pieces name the remaining path and the value
        // span.
        let located = walk(bytes, &[member("animal"), member("type")]);
        let LocatedWalk::ImplicitTable { pieces } = located else {
            panic!("expected an implicit table, got {located:?}");
        };
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0].0, ["name"]);
        assert_eq!(piece_of(bytes, &pieces[0]), "\"pug\"");
        // A missing member inside the implicit table keeps its step.
        assert!(matches!(
            walk(bytes, &[member("animal"), member("type"), member("age")]),
            LocatedWalk::Missing { step: 2 }
        ));
    }

    #[test]
    fn dotted_inline_entries_cover_depths_for_the_missing_step() {
        // `.a.b.x`: `b` is covered as an implicit table, so the missing step is inside it, not at `b`.
        let bytes = b"a = { b.c = 1 }\n";
        assert!(matches!(
            walk(bytes, &[member("a"), member("b"), member("x")]),
            LocatedWalk::Missing { step: 2 }
        ));
        // `.a.x`: the first step is uncovered.
        assert!(matches!(
            walk(bytes, &[member("a"), member("x")]),
            LocatedWalk::Missing { step: 1 }
        ));
        assert!(matches!(
            walk(bytes, &[member("a"), member("x"), member("y")]),
            LocatedWalk::Missing { step: 1 }
        ));
        // `.a.b[0]`: a non-member step over the implicit table is an object mismatch at the covered depth.
        assert!(matches!(
            walk(bytes, &[member("a"), member("b"), ScopedStep::Index(0)]),
            LocatedWalk::TypeMismatch {
                step: 2,
                actual: ValueKind::Object,
            }
        ));
        // Descending past the entry value is the scalar's mismatch.
        assert!(matches!(
            walk(bytes, &[member("a"), member("b"), member("c"), member("d")]),
            LocatedWalk::TypeMismatch {
                step: 3,
                actual: ValueKind::Number,
            }
        ));
        // A single-key value at the covered depth still descends.
        let bytes = b"a = { b = 1, c.d = 2 }\n";
        assert!(matches!(
            walk(bytes, &[member("a"), member("b"), member("x")]),
            LocatedWalk::TypeMismatch {
                step: 2,
                actual: ValueKind::Number,
            }
        ));
    }

    #[test]
    fn dotted_inline_implicit_tables_collect_every_contribution() {
        // A later entry contributes to the same implicit table; scan order is preserved (the tree navigator's authored
        // order).
        let bytes = b"a = { b.d = 2, b.c = 1 }\n";
        let located = walk(bytes, &[member("a"), member("b")]);
        let LocatedWalk::ImplicitTable { pieces } = located else {
            panic!("expected an implicit table, got {located:?}");
        };
        assert_eq!(pieces.len(), 2);
        assert_eq!(pieces[0].0, ["d"]);
        assert_eq!(piece_of(bytes, &pieces[0]), "2");
        assert_eq!(pieces[1].0, ["c"]);
        assert_eq!(piece_of(bytes, &pieces[1]), "1");
        // A nested dotted rest path stays a path in the pieces.
        let bytes = b"a = { b.c.d = 1, b.c.e = 2 }\n";
        let located = walk(bytes, &[member("a"), member("b")]);
        let LocatedWalk::ImplicitTable { pieces } = located else {
            panic!("expected an implicit table, got {located:?}");
        };
        assert_eq!(pieces.len(), 2);
        assert_eq!(pieces[0].0, ["c", "d"]);
        assert_eq!(pieces[1].0, ["c", "e"]);
        // The target may end one level deeper.
        let located = walk(bytes, &[member("a"), member("b"), member("c")]);
        let LocatedWalk::ImplicitTable { pieces } = located else {
            panic!("expected an implicit table, got {located:?}");
        };
        assert_eq!(pieces.len(), 2);
        assert_eq!(pieces[0].0, ["d"]);
        assert_eq!(pieces[1].0, ["e"]);
    }

    #[test]
    fn dotted_inline_entries_reject_like_the_parser_rejects() {
        let resources = crate::test_support::resources();
        // The walk shares `record_inline_key`, so the duplicate and value-extension conflict laws read identically to
        // the whole-document builder's.
        for corrupt in [
            b"a = { b = 1, b = 2 }\n".as_slice(),
            b"a = { b = 1, b.c = 2 }\n".as_slice(),
            b"a = { b.c = 1, b.c = 2 }\n".as_slice(),
            b"a = { b.c = 1, b.c.d = 2 }\n".as_slice(),
            b"a = { b.c.d = 1, b = 2 }\n".as_slice(),
        ] {
            let result = Walker::try_new(
                source(corrupt),
                DialectKind::Toml10,
                &[member("a"), member("b")],
                &resources,
                true,
            )
            .walk();
            assert!(result.is_err(), "walk accepted {corrupt:?}");
        }
    }

    #[test]
    fn descend_into_an_inline_table_element_restores_the_offset() {
        // The descent into an array element that is an inline table used to leave the lexer at the ELEMENT's end; the
        // enclosing scan then read the entry separator from the wrong offset and reported a bogus "expected ',' or '}'"
        // on valid input. The enclosing scan must continue from the entry value's own end.
        let bytes = b"a = { b = [ { c.d = 1 } ], e = 2 }\n";
        let located = walk(bytes, &[member("a"), member("b"), ScopedStep::Index(0), member("c")]);
        let LocatedWalk::ImplicitTable { pieces } = located else {
            panic!("expected an implicit table, got {located:?}");
        };
        assert_eq!(pieces.len(), 1);
        assert_eq!(piece_of(bytes, &pieces[0]), "1");
        // The entry AFTER the array still resolves.
        let located = walk(bytes, &[member("a"), member("e")]);
        assert_eq!(value_of(bytes, &located), "2");
    }

    #[test]
    fn ranges_resolve_over_values_and_arrays_of_tables() {
        let bytes = b"a = [10, 20, 30, 40]\n";
        let located = walk(
            bytes,
            &[
                member("a"),
                ScopedStep::Range {
                    start: Some(1),
                    end: Some(3),
                },
            ],
        );
        let LocatedWalk::RangeValue { start, end, empty } = located else {
            panic!("expected a range, got {located:?}");
        };
        assert!(!empty);
        let region = String::from_utf8(bytes[start..end].to_vec()).expect("utf8");
        assert_eq!(region, "20, 30");
        // A degenerate range is empty.
        let located = walk(
            bytes,
            &[
                member("a"),
                ScopedStep::Range {
                    start: Some(3),
                    end: Some(1),
                },
            ],
        );
        assert!(matches!(located, LocatedWalk::RangeValue { empty: true, .. }));
        // A range over an array-of-tables collects the in-range elements.
        let bytes = b"[[p]]\nx = 1\n[[p]]\nx = 2\n[[p]]\nx = 3\n";
        let located = walk(
            bytes,
            &[
                member("p"),
                ScopedStep::Range {
                    start: Some(1),
                    end: Some(3),
                },
            ],
        );
        let LocatedWalk::RangeTables { elements } = located else {
            panic!("expected a table range, got {located:?}");
        };
        assert_eq!(elements.len(), 2);
        assert!(elements[0].len() >= 2, "header + assignment");
    }

    /// A strictly-negative slice bound counts from the END of the observed container (the engine ships signed bounds;
    /// the codec resolves them len-relative, exactly as YAML's navigator and JSON's scoped route do) — never a
    /// contract refusal.
    #[test]
    fn negative_slice_bounds_resolve_len_relatively() {
        // A strictly-negative bound counts from the END of the observed container.
        let bytes = b"a = [10, 20, 30, 40]\n";
        let located = walk(
            bytes,
            &[
                member("a"),
                ScopedStep::Range {
                    start: Some(-2),
                    end: None,
                },
            ],
        );
        let LocatedWalk::RangeValue { start, end, empty } = located else {
            panic!("expected a range, got {located:?}");
        };
        assert!(!empty);
        let region = String::from_utf8(bytes[start..end].to_vec()).expect("utf8");
        assert_eq!(region, "30, 40");
        // A negative END bound stops short of the tail; a fully-negative window selects nothing.
        let located = walk(
            bytes,
            &[
                member("a"),
                ScopedStep::Range {
                    start: None,
                    end: Some(-3),
                },
            ],
        );
        let LocatedWalk::RangeValue { start, end, empty } = located else {
            panic!("expected a range, got {located:?}");
        };
        assert!(!empty);
        let region = String::from_utf8(bytes[start..end].to_vec()).expect("utf8");
        assert_eq!(region, "10");
        let located = walk(
            bytes,
            &[
                member("a"),
                ScopedStep::Range {
                    start: Some(-1),
                    end: Some(-1),
                },
            ],
        );
        assert!(matches!(located, LocatedWalk::RangeValue { empty: true, .. }));
        // The same law over an array-of-tables: the last two elements.
        let bytes = b"[[p]]\nx = 1\n[[p]]\nx = 2\n[[p]]\nx = 3\n[[p]]\nx = 4\n";
        let located = walk(
            bytes,
            &[
                member("p"),
                ScopedStep::Range {
                    start: Some(-2),
                    end: None,
                },
            ],
        );
        let LocatedWalk::RangeTables { elements } = located else {
            panic!("expected a table range, got {located:?}");
        };
        assert_eq!(elements.len(), 2);
    }

    #[test]
    fn a_value_answer_keeps_only_its_own_comments() {
        // The comment-accumulation bug: once the walker resolved a Value answer, every LATER statement's trailing
        // comment was appended to the answer's comment list (the walk keeps validating the whole input after the target
        // is found). The first value must carry only ITS leading comment, and the trailing comment of a later statement
        // must stay off it.
        let bytes = b"# doc leading\nk1 = 1\n# k2 lead\nk2 = 2 # k2 trail\n\n[t1]\n# t1 a\na = 1 # t1 a trail\n\n[t2]\n# t2 a\na = 2 # t2 a trail\n";
        let LocatedWalk::Value {
            start,
            end,
            leading,
            inline,
        } = walk(bytes, &[member("k1")])
        else {
            panic!("expected a value");
        };
        assert_eq!(String::from_utf8(bytes[start..end].to_vec()).unwrap(), "1");
        assert_eq!(leading, &["doc leading"]);
        assert!(inline.is_empty());

        // The value whose statement carries its own leading AND inline comment keeps both, in their separate sets, but
        // nothing from later statements.
        let LocatedWalk::Value { leading, inline, .. } = walk(bytes, &[member("k2")]) else {
            panic!("expected a value");
        };
        assert_eq!(leading, &["k2 lead"]);
        assert_eq!(inline, &["k2 trail"]);

        // A value in a later table carries only its own comments, and the first table's values do not absorb the later
        // table's inline comments either.
        let LocatedWalk::Value { leading, inline, .. } = walk(bytes, &[member("t1"), member("a")]) else {
            panic!("expected a value");
        };
        assert_eq!(leading, &["t1 a"]);
        assert_eq!(inline, &["t1 a trail"]);
        let LocatedWalk::Value { leading, inline, .. } = walk(bytes, &[member("t2"), member("a")]) else {
            panic!("expected a value");
        };
        assert_eq!(leading, &["t2 a"]);
        assert_eq!(inline, &["t2 a trail"]);
    }

    #[test]
    fn an_earlier_statements_trailing_comment_reaches_neither_the_answer_nor_the_next_statement() {
        // The scoped walk drained the lexer comment buffer only at the ANSWER's statement end; an EARLIER statement's
        // trailing comment stayed in the buffer and the next statement's leading set claimed it (`.b.@comment` answered
        // `["inline-a"]` where the whole route answered `null`). The buffer must drain at every statement end, and the
        // leading and inline sets must stay separate.
        let bytes = b"a = 1 # a trail\nb = 2\n";
        let LocatedWalk::Value { leading, inline, .. } = walk(bytes, &[member("b")]) else {
            panic!("expected a value");
        };
        assert!(
            leading.is_empty(),
            "the next statement's leading must not absorb a's trailing"
        );
        assert!(inline.is_empty(), "b carries no comment of its own");

        // The ANSWER's own trailing comment lands on the answer's INLINE set, never the leading set, and a later
        // statement's trailing stays off the answer entirely.
        let bytes = b"a = 1 # a trail\nb = 2 # b trail\n";
        let LocatedWalk::Value { leading, inline, .. } = walk(bytes, &[member("b")]) else {
            panic!("expected a value");
        };
        assert!(leading.is_empty());
        assert_eq!(inline, &["b trail".to_owned()]);

        // A HEADER's own trailing comment (the walk drains it at the header's statement end too) must not leak into the
        // next statement's leading set.
        let bytes = b"[t] # header note\nx = 1\n";
        let LocatedWalk::Value { leading, inline, .. } = walk(bytes, &[member("t"), member("x")]) else {
            panic!("expected a value");
        };
        assert!(leading.is_empty(), "the header note stays off x's leading");
        assert!(inline.is_empty());
    }

    #[test]
    fn interior_comments_of_a_multiline_value_join_the_leading_set_not_the_inline_set() {
        // A multi-line value buffers its INTERIOR comments during the value parse; the floor attaches them to the
        // value's COMMENT fact and keeps only the own-line trailing comment as INLINE. Draining the whole buffer at the
        // statement end would fold the interior lines into the inline set.
        let bytes = b"t = [\n  # inner\n  1,\n] # tail\n";
        let LocatedWalk::Value { leading, inline, .. } = walk(bytes, &[member("t")]) else {
            panic!("expected a value");
        };
        assert_eq!(leading, &["inner".to_owned()]);
        assert_eq!(inline, &["tail".to_owned()]);
        // The split holds with a statement-leading run in front of it.
        let bytes = b"# lead\nt = [\n  # inner\n  1,\n]\n";
        let LocatedWalk::Value { leading, inline, .. } = walk(bytes, &[member("t")]) else {
            panic!("expected a value");
        };
        assert_eq!(leading, &["lead".to_owned(), "inner".to_owned()]);
        assert!(inline.is_empty());
    }

    #[test]
    fn a_run_before_a_header_is_the_closing_tables_foot() {
        // A comment run whose next token is a `[header]` attaches as the PRECEDING table's foot, never the next table's
        // leading — the walk half of the grammar's foot law, so the scoped route answers the whole route's
        // `.a.@comment_foot`.
        let bytes = b"[a]\nx = 1\n# foot of a\n[b]\ny = 2\n";
        let LocatedWalk::Table { foot, .. } = walk(bytes, &[member("a")]) else {
            panic!("expected the table");
        };
        assert_eq!(foot, &["foot of a".to_owned()]);

        // The FOLLOWING table's answer carries no foot: the run belongs to a.
        let LocatedWalk::Table { foot, .. } = walk(bytes, &[member("b")]) else {
            panic!("expected the table");
        };
        assert!(foot.is_empty(), "b's own foot (after y = 2) is empty");

        // A run before an ASSIGNMENT is that statement's leading, not a foot.
        let bytes = b"[a]\nx = 1\n# lead of y\ny = 2\n";
        let LocatedWalk::Table { foot, .. } = walk(bytes, &[member("a")]) else {
            panic!("expected the table");
        };
        assert!(foot.is_empty());
        let LocatedWalk::Value { leading, inline, .. } = walk(bytes, &[member("a"), member("y")]) else {
            panic!("expected a value");
        };
        assert_eq!(leading, &["lead of y".to_owned()]);
        assert!(inline.is_empty());
    }

    #[test]
    fn the_walk_rejects_what_the_parser_rejects() {
        let walk_resources = crate::test_support::resources();
        for corrupt in [
            b"a = 1 garbage\n".as_slice(),
            b"a = \"unterminated\n".as_slice(),
            b"a = [1, 2\n".as_slice(),
            b"a = { x = 1, x = 2 }\n".as_slice(),
            b"[[a]]\nx = 1\n[a]\ny = 2\n".as_slice(),
            b"a = 1979-13-99\n".as_slice(),
            b"a = \"\\q\"\n".as_slice(),
        ] {
            let result = Walker::try_new(source(corrupt), DialectKind::Toml10, &[], &walk_resources, true).walk();
            assert!(result.is_err(), "walk accepted {corrupt:?}");
            let mut parser_resources = crate::test_support::resources();
            let parsed = crate::parse::parse_direct(source(corrupt), DialectKind::Toml10, &mut parser_resources);
            assert!(parsed.is_err(), "parser accepted {corrupt:?}");
        }
    }

    fn materialize_walk(bytes: &[u8], located: &LocatedWalk) -> Value {
        let mut resources = crate::test_support::resources();
        let (builder, root) = match located {
            LocatedWalk::Value {
                start,
                end,
                leading,
                inline,
            } => crate::lazy::build_wrapped_value(
                bytes,
                *start,
                *end,
                leading,
                inline,
                DialectKind::Toml10,
                jqf_data::BuilderCoverage::minimal_semantic(),
                &mut resources,
            ),
            LocatedWalk::Table {
                spans,
                foot,
                key_depth,
                element,
            } => crate::lazy::build_statement_table(
                bytes,
                spans,
                foot,
                *key_depth,
                *element,
                DialectKind::Toml10,
                jqf_data::BuilderCoverage::minimal_semantic(),
                &mut resources,
            ),
            LocatedWalk::ImplicitTable { pieces } => crate::lazy::build_implicit_table(
                bytes,
                pieces,
                DialectKind::Toml10,
                jqf_data::BuilderCoverage::minimal_semantic(),
                &mut resources,
            ),
            other => panic!("expected a materializable walk answer, got {other:?}"),
        }
        .expect("the walk answer must re-parse");
        let document = builder.finish(root, &resources).expect("finish");
        document.materialize_root(&mut resources).expect("materialize")
    }

    /// An indexed array-of-tables element is the element table, not a one-element array wrapping it.
    #[test]
    fn an_indexed_array_of_tables_element_materializes_as_the_element() {
        let bytes = b"[[bin]]\nname=\"a\"\n[[bin]]\nname=\"b\"\n";
        let located = walk(bytes, &[member("bin"), ScopedStep::Index(0)]);
        let Value::Object(object) = materialize_walk(bytes, &located) else {
            panic!("expected the element object, got {located:?}");
        };
        let Value::String(name) = object.get("name").expect("name") else {
            panic!("name is not a string");
        };
        assert_eq!(name.as_str(), "a");
        let located = walk(bytes, &[member("bin"), ScopedStep::Index(-1)]);
        let Value::Object(object) = materialize_walk(bytes, &located) else {
            panic!("expected the last element object");
        };
        let Value::String(name) = object.get("name").expect("name") else {
            panic!("name is not a string");
        };
        assert_eq!(name.as_str(), "b");
    }

    /// A nested `[a.b]` (or deeper) header answers the inner table, not the ancestor wrapper the re-parsed root's first
    /// child would build.
    #[test]
    fn a_nested_section_materializes_as_the_inner_table() {
        let bytes = b"[t]\nx=1\n[t.u]\nz=3\n";
        let located = walk(bytes, &[member("t"), member("u")]);
        let Value::Object(object) = materialize_walk(bytes, &located) else {
            panic!("expected the inner table, got {located:?}");
        };
        assert!(object.get("z").is_some(), "inner table keeps z");
        assert!(object.get("u").is_none(), "must not wrap in ancestor u");
        let bytes = b"[a.b.c]\nz=1\n";
        let located = walk(bytes, &[member("a"), member("b"), member("c")]);
        let Value::Object(object) = materialize_walk(bytes, &located) else {
            panic!("expected the deepest table");
        };
        assert!(object.get("z").is_some());
        assert!(object.get("b").is_none() && object.get("c").is_none());
    }

    /// Index-then-navigate leaves the lexer past the already-consumed `]`, so a nested index answers the element and a
    /// further step on a scalar is the element's type error — never `toml.trailing-content`.
    #[test]
    fn index_then_navigate_keeps_the_element_answer_and_its_type_error() {
        let bytes = b"b=[[1,2],[3,4]]\n";
        let located = walk(bytes, &[member("b"), ScopedStep::Index(1), ScopedStep::Index(0)]);
        assert_eq!(value_of(bytes, &located), "3");
        let bytes = b"a=[1,2]\n";
        assert!(matches!(
            walk(bytes, &[member("a"), ScopedStep::Index(0), member("b")]),
            LocatedWalk::TypeMismatch {
                step: 2,
                actual: ValueKind::Number,
            }
        ));
        assert!(matches!(
            walk(bytes, &[member("a"), ScopedStep::Index(0), ScopedStep::Index(1)]),
            LocatedWalk::TypeMismatch {
                step: 2,
                actual: ValueKind::Number,
            }
        ));
    }

    /// A numeric index over a dotted-key implicit table is an object mismatch, not a missing key (null).
    #[test]
    fn a_numeric_index_over_a_dotted_key_table_is_a_type_mismatch() {
        let bytes = b"a.b=1\n";
        assert!(matches!(
            walk(bytes, &[member("a"), ScopedStep::Index(0)]),
            LocatedWalk::TypeMismatch {
                step: 1,
                actual: ValueKind::Object,
            }
        ));
        let bytes = b"[t]\nx=1\n";
        assert!(matches!(
            walk(bytes, &[member("t"), ScopedStep::Index(0)]),
            LocatedWalk::TypeMismatch {
                step: 1,
                actual: ValueKind::Object,
            }
        ));
    }
}
