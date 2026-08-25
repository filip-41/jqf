//! The byte-level validate + navigate walk (the span-based locate).
//!
//! A specialized route that built the WHOLE document and navigated it would make retention scale with the input.
//! Instead, this walk validates the complete input to the generic dialect's exact strictness — well-formedness, text
//! UTF-8, §5.6.1 map-key uniqueness, recognized-tag content — while resolving the target path over the WIRE, without
//! building any nodes. The answer is the located value's byte span. A pending index resolves against a definite-length
//! array's declared count during the walk itself, and against an indefinite-length array's collected element spans once
//! the break byte fixes the count. A negative observation carries the exact step semantics of the document navigator.
//!
//! The walker reuses the shared structural key parser and the raw-shape tag law ([`crate::parse::parse_key_value`] and
//! [`crate::parse::validate_tag_payload`]), so it cannot drift from the decoder on map keys or recognized-tag content.
//! Tag chains are payload-transparent: an uninterpreted tag's payload is walked as the value itself, so a path into
//! `55799(34([1, 2]))` resolves.
//!
//! ## The narrow skip
//!
//! Content OFF the resolved path (a sibling the walk validates but never navigates into) takes the NARROW SKIP: every
//! byte is still validated to the same strictness as the floor — heads, length bounds, text UTF-8, chunk-type mixing,
//! recognized-tag shapes, nesting, §5.6.1 map-key uniqueness, and the text-key-only constraint — but no document
//! nodes are built. A definite-length scalar advances by its length prefix (a pointer add, never a parse); a container
//! opens a frame on the skip's explicit stack (never a native frame — document depth is not stack depth). Map keys
//! still go through [`parse_key_value`] and the per-frame [`KeySet`] so an RFC-invalid sibling cannot pass the scoped
//! route where the floor would reject it. A malformed element is caught everywhere.

use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::vec::Vec;

use jqf_codec_core::{CodecError, CodecFailureKind};
use jqf_data::ValueKind;
use jqf_resource::{DepthGuard, ResourceContext};
use jqf_source::ResolvedSource;

use crate::equality::{KeySet, KeyValue};
use crate::parse::{checked_slice_end, float_width, parse_key_value, read_error, validate_tag_payload};
use crate::read::{self, Arg, Major};
use jqf_codec_core::OwnedStep;

/// The located answer of the walk.
#[derive(Clone, Debug)]
pub(crate) enum Located {
    /// The value at the target path, as a byte span.
    Value {
        /// Offset of the value's first byte (any leading tag heads included).
        start: usize,
        /// Offset just past the value's last byte.
        end: usize,
    },
    /// The step at which navigation stopped: no member or position exists.
    Missing {
        /// Zero-based failing path step.
        step: usize,
    },
    /// The step at which a kind mismatch stopped the path.
    TypeMismatch {
        /// Zero-based failing path step.
        step: usize,
        /// The located value's payload-transparent kind.
        actual: ValueKind,
    },
}

/// The container family of an item, for navigation and the element index.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Container {
    Array,
    Object,
}

impl Container {
    fn kind(self) -> ValueKind {
        match self {
            Self::Array => ValueKind::Array,
            Self::Object => ValueKind::Object,
        }
    }
}

/// One OPEN container in the narrow skip's explicit frame stack, standing in for the native frame the skip used to
/// recurse into. The stack is bounded by the governed nesting ceiling: every frame holds the [`DepthGuard`] that
/// admitted its level, so a document deeper than the ceiling is refused before its frame is pushed.
struct SkipFrame<'a> {
    /// Items still owed at this level (a map owes a key AND a value per declared pair). Unused while `indefinite`,
    /// which ends at a break byte.
    owed: u64,
    /// An indefinite-length container: it ends at a break byte, not a count.
    indefinite: bool,
    /// Which family the level is — a map's break byte is legal only where a pair starts.
    container: Container,
    /// A map level that has taken a key and still owes its value.
    mid_pair: bool,
    /// §5.6.1 uniqueness ledger for this map. Empty for arrays.
    keys: KeySet,
    _guard: DepthGuard<'a>,
}

/// The step to apply to one item:
/// - `None`: validate-only (a sibling off the resolved path — never noted).
/// - `Some(i)` with `i < steps.len()`: navigating, resolving `steps[i]`.
/// - `Some(steps.len())`: every step resolved — the item IS the target.
type StepCtx = Option<usize>;

/// The validating walker over one source.
struct Walker<'a> {
    /// The exact immutable source authority the input was resolved from. Carried so every rejection names its byte
    /// offset, exactly as the whole-document decoder's diagnostics do.
    source: ResolvedSource<'a>,
    bytes: &'a [u8],
    steps: &'a [OwnedStep],
    pos: usize,
    outcome: Option<Located>,
    /// The adjacent-value opt-in: stop at the FIRST top-level item and let the caller decode the remainder as another
    /// item, instead of rejecting everything past it as trailing content. The consumed offset the caller advances by is
    /// the walker's `pos` after the item.
    adjacent: bool,
    resources: &'a ResourceContext<'a>,
}

/// Validates the input and resolves the exact path, returning the located span (or a negative observation) together
/// with the offset of the END of the first top-level item — the consumed prefix the adjacent-value drive advances by.
/// The two channels agree: the located span is inside the item, the item end is the whole item's end.
pub(crate) fn locate(
    source: ResolvedSource<'_>,
    steps: &[OwnedStep],
    adjacent: bool,
    resources: &ResourceContext<'_>,
) -> Result<(Located, usize), CodecError> {
    let mut walker = Walker {
        source,
        bytes: source.bytes(),
        steps,
        pos: 0,
        outcome: None,
        adjacent,
        resources,
    };
    walker.walk()?;
    let item_end = walker.pos;
    let outcome = walker.outcome.ok_or_else(|| {
        CodecError::new(CodecFailureKind::InternalContractViolation {
            contract: "CBOR walk produced no located answer",
        })
    })?;
    Ok((outcome, item_end))
}

impl<'a> Walker<'a> {
    fn walk(&mut self) -> Result<(), CodecError> {
        let _guard = self.resources.enter_nesting().map_err(CodecError::from)?;
        self.walk_item(Some(0), self.pos)?;
        // Exactly one top-level item. In adjacent mode the walk stops at the item's end and the remainder is the next
        // item, not an error.
        if !self.adjacent && self.pos != self.bytes.len() {
            return Err(read_error(self.source, self.pos, crate::read::ReadError::Eof));
        }
        Ok(())
    }

    /// Processes one complete value beginning at `self.pos` (leading tags included). `value_start` is the offset before
    /// any leading tag heads, kept so the located span includes them.
    fn walk_item(&mut self, step: StepCtx, value_start: usize) -> Result<(), CodecError> {
        // OFF the resolved path (a validate-only sibling): the narrow skip validates the item to the floor's strictness
        // and advances, building no document nodes. Every note below is a no-op for `step == None`, so no observation
        // is lost.
        if step.is_none() {
            return self.skip_item();
        }
        loop {
            let (head, next) =
                read::head(self.bytes, self.pos).map_err(|error| read_error(self.source, self.pos, error))?;
            match head.major {
                Major::Tag => {
                    let Arg::UInt(tag) = head.arg else {
                        return Err(read_error(self.source, self.pos, crate::read::ReadError::Eof));
                    };
                    self.pos = next;
                    if tag <= 5 {
                        // A recognized tag's payload is a validated scalar shape (date-time text, epoch int/float,
                        // bignum bytes, decimal/ bigfloat pairs); the raw-shape law is shared with the decoder. The
                        // payload is never a navigation target, so parsing it validates AND consumes it.
                        let (payload, after) = parse_key_value(self.source, self.pos, self.resources)?;
                        validate_tag_payload(tag, &payload)?;
                        self.pos = after;
                        let end = self.pos;
                        // A pending step cannot navigate INTO the payload: the step's domain is violated by the scalar
                        // it wraps.
                        self.note_scalar_mismatch(step, tag_payload_kind(tag));
                        self.note_value(step, value_start, end);
                        return Ok(());
                    }
                    // An uninterpreted tag (including 55799) is payload- transparent: the value it wraps is walked as
                    // the value itself. A chain of tag heads is one head per byte of untrusted input, so this is a
                    // LOOP: recursing here cost one native frame per byte and overflowed the stack.
                    continue;
                }
                Major::UInt | Major::NegInt | Major::Bytes | Major::Text | Major::Simple => {
                    let kind = self.walk_scalar(head, next)?;
                    // A pending step against a scalar can never resolve: the walker does not navigate into scalar
                    // payloads, so the step's domain is violated rather than the walk left unanswerable — this note
                    // IS the observation, on the probe path over a scalar element and the located path over a scalar
                    // document alike.
                    self.note_scalar_mismatch(step, kind);
                }
                Major::Array => {
                    self.pos = next;
                    return self.walk_container(head, step, value_start, Container::Array);
                }
                Major::Map => {
                    self.pos = next;
                    return self.walk_container(head, step, value_start, Container::Object);
                }
            }
            let end = self.pos;
            self.note_value(step, value_start, end);
            return Ok(());
        }
    }

    /// Validates one scalar item, advances `self.pos` past it, and answers the value kind a pending step earns its
    /// mismatch observation against. The kind comes from the SAME match that validated the head, so no second
    /// classification can disagree with it.
    fn walk_scalar(&mut self, head: read::Head, next: usize) -> Result<ValueKind, CodecError> {
        match (head.major, head.arg) {
            (Major::UInt | Major::NegInt, Arg::UInt(_)) => {
                self.pos = next;
                Ok(ValueKind::Number)
            }
            (Major::Bytes, Arg::UInt(length)) => {
                self.pos = checked_slice_end(self.source, next, length)?;
                Ok(ValueKind::Bytes)
            }
            (Major::Bytes, Arg::Indef) => {
                let (_, end) = crate::parse::read_string_chunks(self.source, next, false)?;
                self.pos = end;
                Ok(ValueKind::Bytes)
            }
            (Major::Text, Arg::UInt(length)) => {
                let end = checked_slice_end(self.source, next, length)?;
                let text = self
                    .bytes
                    .get(next..end)
                    .ok_or_else(|| read_error(self.source, next, crate::read::ReadError::Eof))?;
                core::str::from_utf8(text).map_err(|_| {
                    crate::error::invalid(self.source, next, "invalid-utf8", "text string is not valid UTF-8")
                })?;
                self.pos = end;
                Ok(ValueKind::String)
            }
            (Major::Text, Arg::Indef) => {
                let (_, end) = crate::parse::read_string_chunks(self.source, next, true)?;
                self.pos = end;
                Ok(ValueKind::String)
            }
            (Major::Simple, Arg::UInt(kind)) => {
                let kind = u8::try_from(kind).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?;
                if kind == 25 || kind == 26 || kind == 27 {
                    // The float payload is folded into the head's end; reading its bits validates the payload is in
                    // bounds.
                    let _ = crate::parse::read_float_bits(self.source, kind, next.saturating_sub(float_width(kind)))?;
                }
                self.pos = next;
                Ok(match kind {
                    20 | 21 => ValueKind::Bool,
                    22 | 23 => ValueKind::Null,
                    _ => ValueKind::Number,
                })
            }
            _ => Err(read_error(
                self.source,
                self.pos,
                crate::read::ReadError::ReservedArgument,
            )),
        }
    }

    /// The narrow skip: validates ONE complete item to the floor's strictness and advances `pos` past it WITHOUT
    /// building document nodes. Map keys still take [`parse_key_value`] and the per-frame [`KeySet`] so §5.6.1
    /// uniqueness and the text-key-only constraint hold off the resolved path exactly as they hold on it.
    ///
    /// The advance is a pointer add over the length prefix for a definite-length scalar; a container OPENS a
    /// [`SkipFrame`] instead of descending. The raw-shape law for recognized tags and the chunk-type law for indefinite
    /// strings are the SHARED laws, so a malformed element off the resolved path is caught exactly as before (the
    /// validation law).
    ///
    /// The descent is an explicit frame stack, NOT recursion. A container per byte (`0x81`) is one open level per byte
    /// of untrusted input, so recursing here overflowed the native stack well inside the governed nesting ceiling —
    /// and a depth guard does not fix that, because `10_000` admitted levels are still `10_000` native frames. The
    /// guard keeps its own job (bounding document depth as a governed resource) and bounds this stack with it, so the
    /// frames need no cap of their own.
    fn skip_item(&mut self) -> Result<(), CodecError> {
        // One frame per OPEN container; the root level owes the single item this call skips.
        let mut frames: Vec<SkipFrame<'_>> = Vec::new();
        let mut root_owed = true;
        loop {
            // Take one item from the innermost open level, closing levels that owe none.
            match frames.last_mut() {
                None => {
                    if !root_owed {
                        return Ok(());
                    }
                    root_owed = false;
                }
                Some(frame) => {
                    if frame.indefinite {
                        // A break byte ends the level — but only where an item STARTS a map pair, so a break in a
                        // value position stays the reserved-argument failure it always was.
                        if !frame.mid_pair {
                            let (head, _) = read::head(self.bytes, self.pos)
                                .map_err(|error| read_error(self.source, self.pos, error))?;
                            if head.major == Major::Simple && head.arg == Arg::Indef {
                                self.pos += 1;
                                frames.pop();
                                continue;
                            }
                        }
                    } else if frame.owed == 0 {
                        frames.pop();
                        continue;
                    } else {
                        frame.owed -= 1;
                    }
                    if frame.container == Container::Object {
                        frame.mid_pair = !frame.mid_pair;
                    }
                }
            }
            // A map key takes the shared structural parser and the §5.6.1 ledger — the same two checks
            // walk_container runs on-path — so an off-path sibling cannot pass where the floor rejects.
            if frames
                .last()
                .is_some_and(|frame| frame.container == Container::Object && frame.mid_pair)
            {
                self.read_skip_map_key(&mut frames)?;
                continue;
            }
            // One complete item, tag heads included.
            loop {
                let (head, next) =
                    read::head(self.bytes, self.pos).map_err(|error| read_error(self.source, self.pos, error))?;
                match (head.major, head.arg) {
                    (Major::UInt | Major::NegInt | Major::Bytes | Major::Text | Major::Simple, _) => {
                        self.walk_scalar(head, next)?;
                    }
                    (Major::Array, arg) => {
                        self.pos = next;
                        frames.push(self.open_skip_frame(arg, Container::Array)?);
                    }
                    (Major::Map, arg) => {
                        self.pos = next;
                        frames.push(self.open_skip_frame(arg, Container::Object)?);
                    }
                    (Major::Tag, Arg::UInt(tag)) => {
                        self.pos = next;
                        if tag <= 5 {
                            // A recognized tag's payload is a validated scalar shape; the raw-shape law is shared with
                            // the decoder. Parsing the payload validates AND consumes it, exactly as the full walk
                            // does.
                            let (payload, after) = parse_key_value(self.source, self.pos, self.resources)?;
                            validate_tag_payload(tag, &payload)?;
                            self.pos = after;
                        } else {
                            // An uninterpreted tag is payload-transparent: skip the value it wraps as the value itself.
                            // The head is consumed without closing the item, so the level still owes it.
                            continue;
                        }
                    }
                    (Major::Tag, Arg::Indef) => {
                        return Err(read_error(self.source, self.pos, crate::read::ReadError::Eof));
                    }
                }
                break;
            }
        }
    }

    /// Opens one container level for the narrow skip: takes the nesting level the guard governs and prices the items
    /// the level owes. A map owes a key AND a value per declared pair — a count past `u64::MAX / 2` saturates, which
    /// the input runs out of bytes long before (the same `Eof` the pair-counted loop reported). Reads one map key,
    /// keeping a definite UTF-8 text key as a source range.
    fn take_map_key(&mut self) -> Result<WalkKey<'_>, CodecError> {
        let (head, next) =
            read::head(self.bytes, self.pos).map_err(|error| read_error(self.source, self.pos, error))?;
        if let (Major::Text, Arg::UInt(length)) = (head.major, head.arg) {
            let end = crate::parse::checked_slice_end(self.source, next, length)?;
            let payload = self
                .bytes
                .get(next..end)
                .ok_or_else(|| read_error(self.source, next, crate::read::ReadError::Eof))?;
            core::str::from_utf8(payload).map_err(|_| {
                crate::error::invalid(self.source, next, "invalid-utf8", "text string is not valid UTF-8")
            })?;
            self.pos = end;
            return Ok(WalkKey::Range(next..end, payload));
        }
        let (key, after) = parse_key_value(self.source, self.pos, self.resources)?;
        self.pos = after;
        if !matches!(&key, KeyValue::Text(_)) {
            return Err(unrepresentable());
        }
        Ok(WalkKey::Owned(key))
    }

    fn read_skip_map_key(&mut self, frames: &mut [SkipFrame]) -> Result<(), CodecError> {
        let (head, next) =
            read::head(self.bytes, self.pos).map_err(|error| read_error(self.source, self.pos, error))?;
        if let (Major::Text, Arg::UInt(length)) = (head.major, head.arg) {
            let end = crate::parse::checked_slice_end(self.source, next, length)?;
            let payload = self
                .bytes
                .get(next..end)
                .ok_or_else(|| read_error(self.source, next, crate::read::ReadError::Eof))?;
            core::str::from_utf8(payload).map_err(|_| {
                crate::error::invalid(self.source, next, "invalid-utf8", "text string is not valid UTF-8")
            })?;
            let frame = frames.last_mut().ok_or_else(data_contract)?;
            if !frame.keys.try_insert_text(self.bytes, next..end) {
                return Err(crate::error::invalid(
                    self.source,
                    next,
                    "duplicate-key",
                    "a map's keys must be unique (RFC 8949 §5.6.1)",
                ));
            }
            self.pos = end;
            return Ok(());
        }
        let key_start = self.pos;
        let (key, after) = parse_key_value(self.source, self.pos, self.resources)?;
        if !matches!(&key, KeyValue::Text(_)) {
            return Err(unrepresentable());
        }
        let frame = frames.last_mut().ok_or_else(data_contract)?;
        if !frame.keys.try_insert(self.bytes, key) {
            return Err(crate::error::invalid(
                self.source,
                key_start,
                "duplicate-key",
                "a map's keys must be unique (RFC 8949 §5.6.1)",
            ));
        }
        self.pos = after;
        Ok(())
    }

    fn open_skip_frame(&self, arg: Arg, container: Container) -> Result<SkipFrame<'a>, CodecError> {
        let guard = self.resources.enter_nesting().map_err(CodecError::from)?;
        let (owed, indefinite) = match (arg, container) {
            (Arg::UInt(count), Container::Object) => (count.saturating_mul(2), false),
            (Arg::UInt(count), Container::Array) => (count, false),
            (Arg::Indef, _) => (0, true),
        };
        Ok(SkipFrame {
            owed,
            indefinite,
            container,
            mid_pair: false,
            keys: KeySet::default(),
            _guard: guard,
        })
    }

    /// Walks one container's members/elements, validating every child and navigating the target step.
    #[expect(
        clippy::too_many_lines,
        reason = "one container walk: resolve the navigated member, validate every child, and step through; splitting it would thread the step context through helpers that each read one piece"
    )]
    fn walk_container(
        &mut self,
        head: read::Head,
        step: StepCtx,
        value_start: usize,
        container: Container,
    ) -> Result<(), CodecError> {
        let _guard = self.resources.enter_nesting().map_err(CodecError::from)?;
        let (count, indefinite) = match head.arg {
            Arg::UInt(count) => (count, false),
            Arg::Indef => (u64::MAX, true),
        };
        let pending = match step {
            Some(i) if i < self.steps.len() => Some(i),
            _ => None,
        };
        // Resolve the navigated member identity up front.
        let mut target_member: Option<String> = None;
        let mut navigable = true;
        if let Some(step_index) = pending {
            match (&self.steps[step_index], container) {
                (OwnedStep::Member(member), Container::Object) => {
                    target_member = Some(member.as_str().to_owned());
                }
                (OwnedStep::Index(_), Container::Array) => {}
                (OwnedStep::Member(_), Container::Array) | (OwnedStep::Index(_), Container::Object) => {
                    self.note_mismatch(step_index, container.kind());
                    navigable = false;
                }
                (OwnedStep::Range { .. }, _) => {
                    // A range step is reachable here only if the binder's Exact-slot gate changes; decline so the
                    // demand falls to the whole-document floor, never a contract violation from a user program.
                    return Err(decline_located_range());
                }
            }
        }
        // Element spans exist only for an INDEFINITE-length array with a pending index: its count is fixed by the break
        // byte alone, so the target cannot be chosen until the loop has ended. Every other shape never consults them
        // (the only reader is the indefinite resolution below), and `Vec::new` costs nothing until the first push —
        // collecting spans for them was a dead store per element.
        let element_spans_needed =
            indefinite && navigable && matches!(pending, Some(i) if matches!(self.steps[i], OwnedStep::Index(_)));
        // A DEFINITE-length array's head carries its count, so a pending index resolves NOW and the target element
        // joins the main loop ON-PATH — no span vector, and no second pass over the target element after the loop. A
        // miss defers to the post-loop note so every element is still validated (validate-everything-first).
        let mut eager_walk: Option<(usize, usize)> = None;
        if !indefinite
            && navigable
            && let Some(step_index) = pending
            && let OwnedStep::Index(index) = self.steps[step_index]
        {
            let declared = usize::try_from(count).unwrap_or(usize::MAX);
            if let Some(ordinal) = jqf_data::resolve_index(declared, index) {
                let next_step = step_index.checked_add(1).ok_or_else(data_contract)?;
                eager_walk = Some((ordinal, next_step));
            }
        }
        let mut keys = KeySet::default();
        let mut element_spans: Vec<(usize, usize)> = Vec::new();
        let mut element_position: usize = 0;
        let mut seen = 0u64;
        loop {
            if !indefinite && seen >= count {
                break;
            }
            if indefinite {
                let (head, _) =
                    read::head(self.bytes, self.pos).map_err(|error| read_error(self.source, self.pos, error))?;
                if head.major == Major::Simple && head.arg == Arg::Indef {
                    self.pos += 1;
                    break;
                }
            }
            match container {
                Container::Array => {
                    let element_start = self.pos;
                    if let Some((ordinal, next_step)) = eager_walk
                        && ordinal == element_position
                    {
                        // The declared count fixed this ordinal as the target before any element was read, so it is
                        // walked ON-PATH here instead of being revisited after the loop.
                        self.walk_item(Some(next_step), element_start)?;
                    } else {
                        self.walk_item(StepCtx::None, element_start)?;
                    }
                    if element_spans_needed {
                        element_spans.push((element_start, self.pos));
                    }
                    element_position += 1;
                }
                Container::Object => {
                    let key_start = self.pos;
                    let walk_key = self.take_map_key()?;
                    let value_step = match (navigable, &target_member, pending) {
                        (true, Some(target), Some(index)) if walk_key.matches(target) => {
                            Some(index.checked_add(1).ok_or_else(data_contract)?)
                        }
                        _ => StepCtx::None,
                    };
                    match walk_key {
                        WalkKey::Range(range, _) => {
                            if !keys.try_insert_text(self.bytes, range) {
                                return Err(crate::error::invalid(
                                    self.source,
                                    key_start,
                                    "duplicate-key",
                                    "a map's keys must be unique (RFC 8949 §5.6.1)",
                                ));
                            }
                        }
                        WalkKey::Owned(key) => {
                            if !keys.try_insert(self.bytes, key) {
                                return Err(crate::error::invalid(
                                    self.source,
                                    key_start,
                                    "duplicate-key",
                                    "a map's keys must be unique (RFC 8949 §5.6.1)",
                                ));
                            }
                        }
                    }
                    let value_start = self.pos;
                    self.walk_item(value_step, value_start)?;
                }
            }
            seen += 1;
        }
        let end = self.pos;
        // A pending index over an INDEFINITE-length array resolves against the observed element count once the break
        // byte has fixed it, then re-navigates the target element's span (bounded by the element). A definite-length
        // array resolved eagerly during the loop above; its miss falls to the note below with nothing left to
        // re-navigate.
        if navigable
            && indefinite
            && let Some(step_index) = pending
            && let OwnedStep::Index(index) = self.steps[step_index]
            && let Some(position) = jqf_data::resolve_index(element_position, index)
            && let Some((elem_start, _elem_end)) = element_spans.get(position)
        {
            let saved = self.pos;
            self.pos = *elem_start;
            self.walk_item(Some(step_index.checked_add(1).ok_or_else(data_contract)?), *elem_start)?;
            self.pos = saved;
            debug_assert_eq!(self.pos, end);
        } else if navigable
            && let Some(step_index) = pending
            && container == Container::Array
        {
            self.note_missing(step_index);
        } else if navigable
            && let Some(step_index) = pending
            && container == Container::Object
            && target_member.is_some()
            && self.outcome.is_none()
        {
            // A pending MEMBER step whose name matched no key of the map is the reference's missing-member observation:
            // the path resolved every container but the member is absent, so the engine seeds the residual with the
            // reference's `null` — exactly what the whole-document floor answers. (A pending INDEX over an object was
            // already noted as a mismatch at the container open, which cleared `navigable`.)
            self.note_missing(step_index);
        }
        self.note_value(step, value_start, end);
        Ok(())
    }

    /// Notes the located value for a scalar (or recognized-tag) item.
    fn note_value(&mut self, step: StepCtx, start: usize, end: usize) {
        if self.outcome.is_none() && step == Some(self.steps.len()) {
            self.outcome = Some(Located::Value { start, end });
        }
    }

    fn note_missing(&mut self, step: usize) {
        if self.outcome.is_none() {
            self.outcome = Some(Located::Missing { step });
        }
    }

    fn note_mismatch(&mut self, step: usize, actual: ValueKind) {
        if self.outcome.is_none() {
            self.outcome = Some(Located::TypeMismatch { step, actual });
        }
    }

    /// Notes a type mismatch when a PENDING step lands on a scalar: a scalar payload is never a navigation target, so
    /// the step's domain is violated (the observation the walk owes instead of an unanswerable outcome).
    fn note_scalar_mismatch(&mut self, step: StepCtx, actual: ValueKind) {
        if self.outcome.is_none()
            && let Some(index) = step
            && index < self.steps.len()
        {
            self.note_mismatch(index, actual);
        }
    }
}

enum WalkKey<'a> {
    Range(core::ops::Range<usize>, &'a [u8]),
    Owned(KeyValue),
}

impl WalkKey<'_> {
    fn matches(&self, member: &str) -> bool {
        match self {
            Self::Range(_, bytes) => *bytes == member.as_bytes(),
            Self::Owned(key) => {
                matches!(key, KeyValue::Text(bytes) if bytes.as_slice() == member.as_bytes())
            }
        }
    }
}

/// The value kind a recognized tag's payload projects to (tag 0/1 project an offset date-time; 2-5 project numbers).
fn tag_payload_kind(tag: u64) -> ValueKind {
    if tag <= 1 {
        ValueKind::OffsetDateTime
    } else {
        ValueKind::Number
    }
}

fn unrepresentable() -> CodecError {
    CodecError::new(CodecFailureKind::UnsupportedRepresentation)
}

/// A located route that cannot serve the demanded step shape declines, so the binder's whole-document floor serves the
/// demand instead.
fn decline_located_range() -> CodecError {
    CodecError::new(CodecFailureKind::RequirementMismatch)
}

pub(crate) fn data_contract() -> CodecError {
    jqf_codec_core::data_contract("CBOR walk navigated an unsupported path step")
}

#[cfg(test)]
mod tests {
    use super::*;
    use jqf_resource::{ContinueControl, RequestAccount, ResourceLimits, WorkMeter};

    static CONTROL: ContinueControl = ContinueControl;

    fn source(bytes: &[u8]) -> ResolvedSource<'_> {
        ResolvedSource::new(
            jqf_source::SourceRef::new(jqf_source::SourceId::new(77), jqf_source::SourceKind::Input),
            "cbor.bin",
            bytes,
            0,
        )
    }

    fn ctx() -> ResourceContext<'static> {
        ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                .expect("account"),
            &CONTROL,
            WorkMeter::try_new_v1(4_096).expect("work"),
        )
        .expect("resources")
    }

    fn step_index(i: i64) -> OwnedStep {
        OwnedStep::Index(i)
    }

    /// Whole-input locate (adjacent off) for the walk tests' assertions.
    fn locate_whole(bytes: &[u8], steps: &[OwnedStep], resources: &ResourceContext<'_>) -> Result<Located, CodecError> {
        locate(source(bytes), steps, false, resources).map(|(located, _)| located)
    }

    /// The adjacent-value opt-in: the walk stops at the FIRST top-level item and answers its end offset, instead of
    /// rejecting the bytes after it. The item end is the consumed prefix the drive advances by; the located span sits
    /// inside the item.
    #[test]
    fn adjacent_mode_stops_at_the_item_end() {
        let resources = ctx();
        // Two adjacent arrays: [1, 2] [true].
        let bytes: &[u8] = &[0x82, 0x01, 0x02, 0x81, 0xf5];
        let (located, item_end) = locate(source(bytes), &[step_index(0)], true, &resources).expect("adjacent locate");
        assert_eq!(item_end, 3, "the first item ends at offset 3");
        assert!(matches!(located, Located::Value { end: 2, .. }));
        // The second item is a legal next decode from the item end.
        let (located, item_end) = locate(source(&bytes[3..]), &[], true, &resources).expect("second item locates");
        assert_eq!(item_end, 2);
        assert!(matches!(located, Located::Value { start: 0, .. }));
        // The same input under whole mode rejects the trailing item.
        assert!(locate_whole(bytes, &[], &resources).is_err());
    }

    /// A RANGE step reaching the native located route declines as a requirement mismatch — the binder's
    /// whole-document floor serves ranges — never an internal contract violation from a user program.
    #[test]
    fn a_range_step_declines_to_the_floor() {
        let resources = ctx();
        let bytes: &[u8] = &[0x82, 0x01, 0x02];
        let steps = alloc::vec![OwnedStep::Range {
            start: Some(0),
            end: None,
        }];
        let error =
            locate(source(bytes), &steps, false, &resources).expect_err("a range step declines the located route");
        assert_eq!(error.kind(), CodecFailureKind::RequirementMismatch);
    }

    /// An uninterpreted tag is payload-transparent, so a payload of repeated `0xc6` is one transparent head per byte.
    /// Both the navigating walk and the off-path skip run iteratively: an uninterpreted tag opens no container, so no
    /// nesting guard bounds the chain — N bytes of heads must not become N native frames.
    #[test]
    fn a_tag_chain_does_not_overflow_the_walk() {
        let resources = ctx();
        let chain = alloc::vec![0xc6_u8; 300_000];
        assert!(locate_whole(&chain, &[], &resources).is_err(), "unterminated chain");
        // The same chain as an array element takes the off-path skip.
        let mut element = alloc::vec![0x81_u8];
        element.extend_from_slice(&chain);
        assert!(locate_whole(&element, &[step_index(0)], &resources).is_err());
    }

    /// A legal container chain is DOCUMENT depth, and document depth must not become native stack depth. The off-path
    /// skip recursed once per open container, so a chain nested at the CLI's governed ceiling (`MAX_NESTING_DEPTH` =
    /// `10_000`) aborted the process with a stack overflow instead of answering. A depth guard does not fix that —
    /// `10_000` guarded frames are still `10_000` native frames — so the skip walks an explicit frame stack, and the
    /// nesting guard keeps its own job of bounding the document as a governed resource.
    #[test]
    fn a_deep_container_chain_does_not_overflow_the_walk() {
        let resources = ctx();
        // [[[…1…]]] nested 10_000 deep. The root IS the target (an empty path), so every level below it is OFF the
        // resolved path and takes the narrow skip.
        let mut bytes = alloc::vec![0x81_u8; 10_000];
        bytes.push(0x01);
        let located = locate_whole(&bytes, &[], &resources).expect("a legal deep chain locates");
        match located {
            Located::Value { start, end, .. } => assert_eq!((start, end), (0, bytes.len())),
            other => panic!("expected value, got {other:?}"),
        }
        // The same chain NAVIGATED: `.[0]` walks the root container, then re-navigates the located element — the deep
        // tail below it is still the skip's business.
        let located = locate_whole(&bytes, &[step_index(0)], &resources).expect("locate .[0]");
        match located {
            Located::Value { start, end, .. } => assert_eq!((start, end), (1, bytes.len())),
            other => panic!("expected value, got {other:?}"),
        }
        // The same depth as an indefinite-length map chain: {_ "a": {_ … }}.
        let mut bytes = Vec::new();
        for _ in 0..10_000 {
            bytes.extend_from_slice(&[0xbf, 0x61, 0x61]);
        }
        bytes.push(0x01);
        bytes.extend(core::iter::repeat_n(0xff_u8, 10_000));
        let located = locate_whole(&bytes, &[], &resources).expect("a legal deep map chain locates");
        match located {
            Located::Value { start, end, .. } => assert_eq!((start, end), (0, bytes.len())),
            other => panic!("expected value, got {other:?}"),
        }
    }

    #[test]
    fn locate_nested_array() {
        let bytes: &[u8] = &[0x81, 0x82, 0x01, 0x02];
        let resources = ctx();
        let located = locate_whole(bytes, &[step_index(0)], &resources).expect("locate");
        match located {
            Located::Value { start, end } => {
                assert_eq!(start, 1);
                assert_eq!(end, 4);
            }
            other => panic!("expected value, got {other:?}"),
        }
    }

    /// A pending index over an array answers identically for BOTH length shapes: a definite-length head resolves its
    /// target EAGERLY against the declared count during the main loop (no span vector, no second pass), while an
    /// indefinite-length array collects element spans until the break byte fixes the count. Positive,
    /// negative-from-the-end, and out-of-range observations must agree across the two encodings, and a definite-length
    /// miss must still validate every remaining sibling first (validate-everything-first).
    #[test]
    fn an_index_resolves_over_definite_and_indefinite_arrays_alike() {
        let resources = ctx();
        // [1, 2, 3] definite (0x83…) vs indefinite (0x9f… ff): the elements sit at the same offsets under both
        // heads.
        let definite: &[u8] = &[0x83, 0x01, 0x02, 0x03];
        let indefinite: &[u8] = &[0x9f, 0x01, 0x02, 0x03, 0xff];
        for bytes in [definite, indefinite] {
            // .[2] and .[-1] land on the last element.
            let located = locate_whole(bytes, &[step_index(2)], &resources).expect("index 2");
            assert!(matches!(located, Located::Value { start: 3, end: 4 }));
            let located = locate_whole(bytes, &[step_index(-1)], &resources).expect("index -1");
            assert!(matches!(located, Located::Value { start: 3, end: 4 }));
            // .[-3] lands on the first element.
            let located = locate_whole(bytes, &[step_index(-3)], &resources).expect("index -3");
            assert!(matches!(located, Located::Value { start: 1, end: 2 }));
            // Past either end is Missing, never a wrong answer.
            let located = locate_whole(bytes, &[step_index(3)], &resources).expect("index 3");
            assert!(matches!(located, Located::Missing { step: 0 }));
            let located = locate_whole(bytes, &[step_index(-4)], &resources).expect("index -4");
            assert!(matches!(located, Located::Missing { step: 0 }));
        }

        // The eager definite-length resolution descends INTO the target element on-path: [1, [2, 3], 4] — .[1][0]
        // resolves inside the nested container, exactly as the indefinite encoding does.
        let nested: &[u8] = &[0x83, 0x01, 0x82, 0x02, 0x03, 0x04];
        let located = locate_whole(nested, &[step_index(1), step_index(0)], &resources).expect("nested definite");
        assert!(matches!(located, Located::Value { start: 3, end: 4 }));
        let nested_indefinite: &[u8] = &[0x9f, 0x01, 0x82, 0x02, 0x03, 0x04, 0xff];
        let located =
            locate_whole(nested_indefinite, &[step_index(1), step_index(0)], &resources).expect("nested indefinite");
        assert!(matches!(located, Located::Value { start: 3, end: 4 }));

        // An eager hit does not stop validation: [1, [2, 3], <truncated>] fails on the truncated THIRD sibling even
        // though .[1] itself resolved during the loop.
        let truncated: &[u8] = &[0x83, 0x01, 0x82, 0x02, 0x03, 0x18];
        let error = locate_whole(truncated, &[step_index(1)], &resources)
            .expect_err("a truncated sibling after the target fails");
        assert!(matches!(error.kind(), CodecFailureKind::InvalidInput));
    }

    #[test]
    fn locate_member() {
        let bytes: &[u8] = &[0xa1, 0x61, 0x62, 0x81, 0x61, 0x63]; // {"b": ["c"]}
        let resources = ctx();
        let stored = "b".to_owned();
        let located = locate_whole(bytes, &[OwnedStep::Member(stored)], &resources).expect("locate");
        match located {
            Located::Value { start, end, .. } => {
                assert_eq!(start, 3);
                assert_eq!(end, 6);
            }
            other => panic!("expected value, got {other:?}"),
        }
    }

    /// The narrow skip still catches every malformed element OFF the resolved path — a truncated element, invalid
    /// UTF-8 in a text key, a bad chunk type, a raw-shape tag violation — exactly as the full walk does (the
    /// validation law). The walk is the routes' whole-input validation pass, so a corrupt byte anywhere must still fail
    /// the route.
    #[test]
    fn skip_still_catches_malformed_elements_off_path() {
        let resources = ctx();
        // {"target": 1, "off": [1, 2, <truncated>]}: the malformed element is in the OFF-path sibling "off", never
        // navigated. The walk must still reject it (validate-everything-first).
        let bytes: &[u8] = &[
            0xa2, 0x66, 0x74, 0x61, 0x72, 0x67, 0x65, 0x74, 0x01, 0x63, 0x6f, 0x66, 0x66, 0x83, 0x01, 0x02, 0x18,
        ]; // 0x18 needs an argument byte
        let stored = "target".to_owned();
        let error = locate_whole(bytes, &[OwnedStep::Member(stored)], &resources)
            .expect_err("truncated off-path element fails");
        assert!(matches!(error.kind(), CodecFailureKind::InvalidInput));

        // Same law for invalid UTF-8 in an off-path text key: {"target": 1, "\xff": 2}.
        let bytes: &[u8] = &[0xa2, 0x66, 0x74, 0x61, 0x72, 0x67, 0x65, 0x74, 0x01, 0x61, 0xff, 0x02];
        let stored = "target".to_owned();
        let error = locate_whole(bytes, &[OwnedStep::Member(stored)], &resources)
            .expect_err("invalid UTF-8 in an off-path key fails");
        assert!(matches!(error.kind(), CodecFailureKind::InvalidInput));
    }

    /// Scoped == floor on validity: an RFC-invalid map OFF the resolved path is rejected exactly as the floor rejects
    /// it. Duplicate text keys are `InvalidInput` (§5.6.1); a non-text key is `UnsupportedRepresentation` (the
    /// text-key-only projection). On-path rejection still holds.
    #[test]
    fn skip_enforces_map_key_laws_off_the_resolved_path() {
        let resources = ctx();
        // {"target": 1, "off": {"a": 1, "a": 2}} — the duplicate is in the OFF-path map "off".
        let dup_off: &[u8] = &[
            0xa2, 0x66, 0x74, 0x61, 0x72, 0x67, 0x65, 0x74, 0x01, 0x63, 0x6f, 0x66, 0x66, 0xa2, 0x61, 0x61, 0x01, 0x61,
            0x61, 0x02,
        ];
        let error = locate_whole(dup_off, &[OwnedStep::Member("target".to_owned())], &resources)
            .expect_err("off-path duplicate keys fail the walk");
        assert!(matches!(error.kind(), CodecFailureKind::InvalidInput));

        // {"target": 1, "off": {1: 2}} — the non-text key is in the OFF-path map "off".
        let non_text_off: &[u8] = &[
            0xa2, 0x66, 0x74, 0x61, 0x72, 0x67, 0x65, 0x74, 0x01, 0x63, 0x6f, 0x66, 0x66, 0xa1, 0x01, 0x02,
        ];
        let error = locate_whole(non_text_off, &[OwnedStep::Member("target".to_owned())], &resources)
            .expect_err("off-path non-text keys fail the walk");
        assert!(matches!(error.kind(), CodecFailureKind::UnsupportedRepresentation));

        // {"target": 1, "target": 2} — the duplicate is ON the resolved path (the ROOT map, which the walk navigates
        // into).
        let dup_on: &[u8] = &[
            0xa2, 0x66, 0x74, 0x61, 0x72, 0x67, 0x65, 0x74, 0x01, 0x66, 0x74, 0x61, 0x72, 0x67, 0x65, 0x74, 0x02,
        ];
        let error = locate_whole(dup_on, &[OwnedStep::Member("target".to_owned())], &resources)
            .expect_err("on-path duplicate keys still fail");
        assert!(matches!(error.kind(), CodecFailureKind::InvalidInput));
    }
    /// A pending MEMBER step that names no key of the map is the reference's missing-member observation (`null`), never
    /// an unanswerable walk — the same answer the whole-document floor gives for `.nope` over `{"id":1}`.
    #[test]
    fn an_object_key_miss_is_a_missing_observation() {
        let resources = ctx();
        // {"id": 1} — the navigated member "nope" is absent.
        let bytes: &[u8] = &[0xa1, 0x62, 0x69, 0x64, 0x01];
        let located = locate_whole(
            bytes,
            &[OwnedStep::Member(alloc::string::String::from("nope"))],
            &resources,
        )
        .expect("a member miss is an observation, not a contract violation");
        assert!(matches!(located, Located::Missing { step: 0 }));

        // The member that EXISTS still resolves to its span.
        let located = locate_whole(
            bytes,
            &[OwnedStep::Member(alloc::string::String::from("id"))],
            &resources,
        )
        .expect("a present member resolves");
        assert!(matches!(located, Located::Value { start: 4, end: 5 }));

        // A member step over an ARRAY is the index-class mismatch, exactly as before the fix.
        let array: &[u8] = &[0x81, 0x01];
        let located = locate_whole(
            array,
            &[OwnedStep::Member(alloc::string::String::from("id"))],
            &resources,
        )
        .expect("a member step over an array is an observation");
        assert!(matches!(located, Located::TypeMismatch { step: 0, .. }));
    }

    /// The two refusal clauses: the validate-only walk must reject what the whole-document decoder rejects. A non-text
    /// map key is the text-key-only narrowing; a tag-2 payload past the bignum ceiling is `project_or_retain`'s size
    /// refusal.
    #[test]
    fn the_walk_refuses_the_decoder_s_unrepresentable_clauses() {
        let resources = ctx();
        // Map with integer key `{1: 2}`.
        let integer_key: &[u8] = &[0xa1, 0x01, 0x02];
        let error = locate_whole(integer_key, &[], &resources).expect_err("a non-text map key is unrepresentable");
        assert_eq!(
            error.kind(),
            CodecFailureKind::UnsupportedRepresentation,
            "text-key-only narrowing"
        );
        // Tag 2 wrapping a 1025-byte byte string.
        let mut huge = alloc::vec![0xc2, 0x59, 0x04, 0x01];
        huge.extend(core::iter::repeat_n(0x00_u8, 1025));
        let error = locate_whole(&huge, &[], &resources).expect_err("an oversized bignum is unrepresentable");
        assert_eq!(
            error.kind(),
            CodecFailureKind::UnsupportedRepresentation,
            "bignum-size refusal"
        );
    }
}
