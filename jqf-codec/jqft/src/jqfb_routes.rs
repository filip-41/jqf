//! jqfb's native located route (slot 1, `Exact`/`Located`): served by a node-table walk that claims the format's
//! structural advantage — every subtree's extent is IN the image, so a subtree the route does not materialize is never
//! replayed.
//!
//! The walk is two passes over the node table:
//!
//! - `validate_table` mirrors the whole-document decode's exact strictness — every entry's extent, every container's
//!   subtree-size invariant (an explicit frame stack: document depth costs heap, never call stack), scalar leaf sizes,
//!   key positions, pool index bounds, and every pool payload's grammar — WITHOUT building anything. This is the
//!   validate-everything-first law (the json/cbor walk precedent): a corrupt entry or pool byte anywhere fails the
//!   demand routes exactly as it fails the whole-document floor, so a route can never publish where the floor rejects.
//! - `navigate` resolves the exact path over the VALIDATED table, using each skipped sibling's `subtree_size` as the
//!   cursor advance — a skipped subtree costs one 9-byte head read, never a replay. Sound because the sizes were
//!   verified by the first pass. Recursion is bounded by the path length, never the document depth: a tag chain
//!   (payload-transparent, validated to arbitrary depth) is unwrapped ITERATIVELY, so an attacker-chosen tag depth
//!   costs loop iterations, not call stack.
//!
//! What the route materializes is exactly the located subtree: the scoped route decodes exactly `[start, start +
//! subtree_size)` through the shared whole-document walk bounded to that extent (`JqfbDecodeState`'s scoped mode — the
//! same one node-kind dispatch table, never a second copy).

use alloc::string::String;
use alloc::vec::Vec;

use jqf_codec_core::{
    AccessInput, AccessOutcome, AccessResult, AccessSession, CodecError, CodecFailureKind, CodecRunContext,
    DocumentProduct, ExactSelectionRecord, LocatedOutcome, PortableStep, SelectionOrigin,
};
use jqf_data::{
    AccountedDocumentBuilder, AccountedSemanticNode, AuthoritativeEmptyFamilies, BuilderCoverage, DataError,
    DiagnosticCoverage, DocumentCapabilityFamily, DocumentFinalizationPoll, TagId, ValueKind,
};
use jqf_resource::{ResourceContext, ResourceError, WorkAdmission};
use jqf_source::ResolvedSource;

use crate::jqfb::{self, CoreChunks, kinds};
use crate::jqfb_decode::{JqfbDecodeState, JqfbImage};
use crate::parse::try_temporal;
use crate::provider;

/// One owned path step, copied from the portable requirement path.
#[derive(Debug)]
pub(crate) enum Step {
    Member(String),
    Index(i64),
    /// A contiguous element RANGE over an array container. v1 declines range footprints for these routes (the engine's
    /// static pushdown), so the variant exists only for the navigator's contract check.
    Range,
}

/// The located answer of the node-table walk.
#[derive(Debug)]
enum Located {
    /// The subtree at the target path: node-table entries `[start, start + subtree_size)`.
    Value { start: usize, size: u32 },
    /// The step at which navigation stopped: no member or position exists.
    Missing { step: usize },
    /// The step at which a kind mismatch stopped the path.
    TypeMismatch { step: usize, actual: ValueKind },
}

pub(crate) fn own_steps(steps: &[PortableStep]) -> Result<Vec<Step>, CodecError> {
    let mut owned = Vec::new();
    owned
        .try_reserve_exact(steps.len())
        .map_err(ResourceError::from)
        .map_err(CodecError::from)?;
    for step in steps {
        owned.push(match step {
            PortableStep::SemanticMember(member) => {
                let mut stored = String::new();
                stored
                    .try_reserve_exact(member.as_str().len())
                    .map_err(ResourceError::from)
                    .map_err(CodecError::from)?;
                stored.push_str(member.as_str());
                Step::Member(stored)
            }
            PortableStep::SemanticIndex(index) => Step::Index(*index),
            PortableStep::SemanticRange { .. } => Step::Range,
        });
    }
    Ok(owned)
}

// --------------------------------------------------------------------------- Pass 1: the validating walk (mirrors the
// whole decode's exact strictness) ---------------------------------------------------------------------------

/// One frame of the validating walk's explicit container stack.
struct VFrame {
    start: usize,
    subtree_size: u32,
    remaining: usize,
    /// The container kind, for the attach law (an array consumes one child, an object consumes its pending key and one
    /// member, a tag its single payload).
    kind: VKind,
    /// Object frames: whether a KEYTEXT was seen and the next value consumes it (the decode's `pending_key` law).
    pending_key: bool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum VKind {
    Array,
    Object,
    Tag,
}

/// Validates the COMPLETE node table and pool content to the whole-document decode's exact strictness, building
/// nothing: every entry's extent, every container's subtree-size invariant (the frame stack), scalar leaf sizes, key
/// positions, pool index bounds, and each pool payload's grammar (string UTF-8, number text/bits, temporal spellings,
/// tag identity). This is the jqfb analog of the walk's validate-everything-first law: a corrupt entry or pool byte
/// anywhere fails the demand routes exactly as it fails the whole-document floor.
#[allow(
    clippy::too_many_lines,
    reason = "one node-kind validation table: every table kind's law sits beside the others"
)]
fn validate_table(
    node: &[u8],
    strg: &[u8],
    numb: &[u8],
    strg_offsets: &[u32],
    numb_offsets: &[u32],
    node_count: usize,
) -> Result<(), CodecError> {
    {
        let mut frames: Vec<VFrame> = Vec::new();
        let mut cursor = 0usize;
        let mut root_seen = false;
        loop {
            // Pop finished frames and attach their owners to the enclosing frame.
            while let Some(frame) = frames.last() {
                if frame.remaining != 0 {
                    break;
                }
                let frame = frames.pop().ok_or_else(data_contract)?;
                if cursor - frame.start != frame.subtree_size as usize {
                    return Err(jqfb::invalid("a node's subtree size does not match its span"));
                }
                attach_to_parent(&mut frames, &mut root_seen)?;
            }
            if frames.is_empty() && root_seen {
                if cursor != node_count {
                    return Err(jqfb::invalid("the node table has trailing entries"));
                }
                break;
            }
            if cursor >= node_count {
                return Err(jqfb::invalid("the node table walk exceeds its extent"));
            }
            let entry = jqfb::read_node(node, cursor)?;
            cursor += 1;
            match entry.kind {
                kinds::NULL | kinds::BOOL => {
                    if entry.subtree_size != 1 {
                        return Err(jqfb::invalid("a scalar node must be a leaf"));
                    }
                }
                kinds::BYTES => {
                    if entry.subtree_size != 1 {
                        return Err(jqfb::invalid("a scalar node must be a leaf"));
                    }
                    strg_bytes(strg, strg_offsets, entry.payload)?;
                }
                kinds::INTEGER | kinds::DECIMAL | kinds::FLOAT => {
                    if entry.subtree_size != 1 {
                        return Err(jqfb::invalid("a scalar node must be a leaf"));
                    }
                    validate_number(numb, numb_offsets, entry.payload)?;
                }
                kinds::STRING => {
                    if entry.subtree_size != 1 {
                        return Err(jqfb::invalid("a scalar node must be a leaf"));
                    }
                    strg_text(strg, strg_offsets, entry.payload)?;
                }
                kinds::LOCAL_DATE | kinds::LOCAL_TIME | kinds::LOCAL_DATE_TIME | kinds::OFFSET_DATE_TIME => {
                    if entry.subtree_size != 1 {
                        return Err(jqfb::invalid("a scalar node must be a leaf"));
                    }
                    let text = strg_text(strg, strg_offsets, entry.payload)?;
                    if try_temporal(text).is_none() {
                        return Err(jqfb::invalid("a temporal pool entry does not parse"));
                    }
                }
                kinds::TAG => {
                    let text = strg_text(strg, strg_offsets, entry.payload)?;
                    TagId::try_new_unaccounted(text).map_err(|_| jqfb::invalid("a tag is not one nonempty string"))?;
                    frames.push(VFrame {
                        start: cursor - 1,
                        subtree_size: entry.subtree_size,
                        remaining: 1,
                        kind: VKind::Tag,
                        pending_key: false,
                    });
                }
                kinds::ARRAY => {
                    let children =
                        usize::try_from(entry.payload).map_err(|_| jqfb::invalid("array child count overflows"))?;
                    frames.push(VFrame {
                        start: cursor - 1,
                        subtree_size: entry.subtree_size,
                        remaining: children,
                        kind: VKind::Array,
                        pending_key: false,
                    });
                }
                kinds::OBJECT => {
                    let members =
                        usize::try_from(entry.payload).map_err(|_| jqfb::invalid("object member count overflows"))?;
                    let remaining = members
                        .checked_mul(2)
                        .ok_or_else(|| jqfb::invalid("object member count overflows"))?;
                    frames.push(VFrame {
                        start: cursor - 1,
                        subtree_size: entry.subtree_size,
                        remaining,
                        kind: VKind::Object,
                        pending_key: false,
                    });
                }
                kinds::KEYTEXT => {
                    let in_key_position = matches!(
                        frames.last(),
                        Some(VFrame {
                            kind: VKind::Object,
                            pending_key: false,
                            ..
                        })
                    );
                    if !in_key_position {
                        return Err(jqfb::invalid("a KEYTEXT node appears outside object key position"));
                    }
                    if entry.subtree_size != 1 {
                        return Err(jqfb::invalid("a KEYTEXT node must be a leaf"));
                    }
                    strg_text(strg, strg_offsets, entry.payload)?;
                    let frame = frames.last_mut().ok_or_else(data_contract)?;
                    frame.pending_key = true;
                    frame.remaining -= 1;
                }
                _ => return Err(CodecError::new(CodecFailureKind::UnsupportedRepresentation)),
            }
            // A completed SCALAR attaches immediately (a container attaches at its frame pop, handled by the loop
            // head).
            if is_scalar(entry.kind) {
                attach_to_parent(&mut frames, &mut root_seen)?;
            }
        }
    }
    Ok(())
}

/// Attaches one completed value to its enclosing frame (or marks it the root), mirroring the whole decode's `attach`
/// law: an array consumes one child, an object consumes its pending key and one member, a tag its single payload.
fn attach_to_parent(frames: &mut [VFrame], root_seen: &mut bool) -> Result<(), CodecError> {
    let Some(frame) = frames.last_mut() else {
        *root_seen = true;
        return Ok(());
    };
    match frame.kind {
        VKind::Object => {
            if !frame.pending_key {
                return Err(jqfb::invalid("an object value arrives before its key"));
            }
            frame.pending_key = false;
            frame.remaining -= 1;
        }
        VKind::Array | VKind::Tag => frame.remaining -= 1,
    }
    Ok(())
}

/// Whether one node-table kind is a leaf value (never a container).
fn is_scalar(kind: u8) -> bool {
    !matches!(kind, kinds::TAG | kinds::ARRAY | kinds::OBJECT | kinds::KEYTEXT)
}

// --------------------------------------------------------------------------- Pass 2: the subtree_size skip navigation
// ---------------------------------------------------------------------------

/// Navigates the VALIDATED node table to the exact path. Every skipped sibling costs one head read: the walk advances
/// by the sibling's `subtree_size` instead of replaying it. Recursion is bounded by the path length (each step consumes
/// one path component), never the document depth.
fn navigate(
    node: &[u8],
    node_count: usize,
    strg: &[u8],
    strg_offsets: &[u32],
    steps: &[Step],
) -> Result<Located, CodecError> {
    let mut cursor = 0usize;
    walk_value(node, node_count, strg, strg_offsets, &mut cursor, 0, steps)
}

/// Walks the value beginning at `cursor` (leaving it one past the value's subtree), resolving `steps[step..]` against
/// it. Returns the located answer when the value at `cursor` is the target (no steps remain) or a negative observation;
/// otherwise continues into the target child.
///
/// Tag chains are unwrapped ITERATIVELY: a tag is payload-transparent, and the validated table accepts arbitrarily deep
/// chains, so recursing once per tag layer would bound this walk's stack by the image's tag depth instead of by the
/// path length.
#[allow(
    clippy::too_many_lines,
    reason = "one navigation arm per container kind, beside the subtree-size skip loops"
)]
fn walk_value(
    node: &[u8],
    node_count: usize,
    strg: &[u8],
    strg_offsets: &[u32],
    cursor: &mut usize,
    step: usize,
    steps: &[Step],
) -> Result<Located, CodecError> {
    // Unwrap same-step tag layers in a loop, validating each head exactly as the pre-unwrap walk did, until the first
    // non-tag entry (or the no-steps early return on a tag itself).
    let (entry, start, end) = loop {
        if *cursor >= node_count {
            return Err(jqfb::invalid("the node table walk exceeds its extent"));
        }
        let head = jqfb::read_node(node, *cursor)?;
        let head_start = *cursor;
        let head_size = usize::try_from(head.subtree_size).map_err(|_| jqfb::invalid("subtree size overflows"))?;
        let head_end = head_start
            .checked_add(head_size)
            .ok_or_else(|| jqfb::invalid("subtree extent overflows"))?;
        if head_end > node_count {
            return Err(jqfb::invalid("a subtree exceeds the node table"));
        }
        if step >= steps.len() {
            *cursor = head_end;
            return Ok(Located::Value {
                start: head_start,
                size: head.subtree_size,
            });
        }
        if head.kind != kinds::TAG {
            break (head, head_start, head_end);
        }
        // Payload-transparent: the payload continues with the same step.
        *cursor = head_start + 1;
    };
    match entry.kind {
        kinds::ARRAY => {
            let count = usize::try_from(entry.payload).map_err(|_| jqfb::invalid("array child count overflows"))?;
            match &steps[step] {
                Step::Index(index) => {
                    let Some(position) = jqf_data::resolve_index(count, *index) else {
                        *cursor = end;
                        return Ok(Located::Missing { step });
                    };
                    let mut child = start + 1;
                    for _ in 0..position {
                        let head = jqfb::read_node(node, child)?;
                        let head_size =
                            usize::try_from(head.subtree_size).map_err(|_| jqfb::invalid("subtree size overflows"))?;
                        child = child
                            .checked_add(head_size)
                            .ok_or_else(|| jqfb::invalid("array extent overflows"))?;
                        if child > end {
                            return Err(jqfb::invalid("an array element exceeds the array"));
                        }
                    }
                    let mut target = child;
                    let located = walk_value(node, node_count, strg, strg_offsets, &mut target, step + 1, steps)?;
                    *cursor = end;
                    Ok(located)
                }
                Step::Member(_) => {
                    *cursor = end;
                    Ok(Located::TypeMismatch {
                        step,
                        actual: ValueKind::Array,
                    })
                }
                Step::Range => Err(data_contract()),
            }
        }
        kinds::OBJECT => {
            let members = usize::try_from(entry.payload).map_err(|_| jqfb::invalid("object member count overflows"))?;
            match &steps[step] {
                Step::Member(name) => {
                    let mut child = start + 1;
                    let mut matched = false;
                    for _ in 0..members {
                        let key = jqfb::read_node(node, child)?;
                        if key.kind != kinds::KEYTEXT {
                            return Err(jqfb::invalid("an object member has no text key"));
                        }
                        let key_text = strg_text(strg, strg_offsets, key.payload)?;
                        child += 1; // the key is a leaf (size 1)
                        if key_text.as_bytes() == name.as_str().as_bytes() {
                            matched = true;
                            break;
                        }
                        let head = jqfb::read_node(node, child)?;
                        let head_size =
                            usize::try_from(head.subtree_size).map_err(|_| jqfb::invalid("subtree size overflows"))?;
                        child = child
                            .checked_add(head_size)
                            .ok_or_else(|| jqfb::invalid("object extent overflows"))?;
                        if child > end {
                            return Err(jqfb::invalid("an object member exceeds the object"));
                        }
                    }
                    if !matched {
                        *cursor = end;
                        return Ok(Located::Missing { step });
                    }
                    let mut target = child;
                    let located = walk_value(node, node_count, strg, strg_offsets, &mut target, step + 1, steps)?;
                    *cursor = end;
                    Ok(located)
                }
                Step::Index(_) => {
                    *cursor = end;
                    Ok(Located::TypeMismatch {
                        step,
                        actual: ValueKind::Object,
                    })
                }
                Step::Range => Err(data_contract()),
            }
        }
        // A pending step against a scalar can never resolve: the walker does not navigate into scalar payloads.
        _ => {
            *cursor = end;
            Ok(Located::TypeMismatch {
                step,
                actual: scalar_kind(entry.kind),
            })
        }
    }
}

/// The payload-transparent kind of one scalar node-table kind.
fn scalar_kind(kind: u8) -> ValueKind {
    match kind {
        kinds::NULL => ValueKind::Null,
        kinds::BOOL => ValueKind::Bool,
        kinds::INTEGER | kinds::DECIMAL | kinds::FLOAT => ValueKind::Number,
        kinds::STRING => ValueKind::String,
        kinds::BYTES => ValueKind::Bytes,
        kinds::LOCAL_DATE => ValueKind::LocalDate,
        kinds::LOCAL_TIME => ValueKind::LocalTime,
        kinds::LOCAL_DATE_TIME => ValueKind::LocalDateTime,
        kinds::OFFSET_DATE_TIME => ValueKind::OffsetDateTime,
        _ => unreachable!("not a scalar node-table kind"),
    }
}

// --------------------------------------------------------------------------- Pool readers (borrowed — never copied)
// ---------------------------------------------------------------------------

/// One STRG pool entry's bytes, borrowed from the chunk. Bounds-checks the pool index the way STRING and BYTES both
/// must.
fn strg_bytes<'a>(strg: &'a [u8], offsets: &[u32], index: u32) -> Result<&'a [u8], CodecError> {
    let offset = offsets
        .get(usize::try_from(index).map_err(|_| jqfb::invalid("string pool index overflows"))?)
        .copied()
        .ok_or_else(|| jqfb::invalid("a string pool index exceeds the pool"))?;
    let (bytes, _) = jqfb::pool_entry(strg, offset as usize)?;
    Ok(bytes)
}

/// One STRG pool entry as UTF-8 text.
fn strg_text<'a>(strg: &'a [u8], offsets: &[u32], index: u32) -> Result<&'a str, CodecError> {
    let bytes = strg_bytes(strg, offsets, index)?;
    core::str::from_utf8(bytes).map_err(|_| jqfb::invalid("a string pool entry is not UTF-8"))
}

/// One NUMB pool entry (tag + body), borrowed from the chunk.
fn numb_entry<'a>(numb: &'a [u8], offsets: &[u32], index: u32) -> Result<&'a [u8], CodecError> {
    let offset = offsets
        .get(usize::try_from(index).map_err(|_| jqfb::invalid("number pool index overflows"))?)
        .copied()
        .ok_or_else(|| jqfb::invalid("a number pool index exceeds the pool"))?;
    let end = crate::jqfb_decode::number_entry_end(numb, offset as usize)?;
    numb.get(offset as usize..end)
        .ok_or_else(|| jqfb::invalid("a number pool entry exceeds the chunk"))
}

/// Validates one number-pool entry's grammar (the decode's exact law: the integer text parses, the decimal coefficient
/// parses and the scale word is in bounds, the float bits are in bounds).
fn validate_number(numb: &[u8], offsets: &[u32], index: u32) -> Result<(), CodecError> {
    let body = numb_entry(numb, offsets, index)?;
    match body.first() {
        Some(0) => {
            let (text, _) = jqfb::pool_entry(body, 1)?;
            let text = core::str::from_utf8(text).map_err(|_| jqfb::invalid("an integer pool entry is not UTF-8"))?;
            jqf_data::Integer::parse(text).map_err(|_| jqfb::invalid("an integer pool entry does not parse"))?;
        }
        Some(1) => {
            let (coef, after) = jqfb::pool_entry(body, 1)?;
            let coef = core::str::from_utf8(coef).map_err(|_| jqfb::invalid("a decimal pool entry is not UTF-8"))?;
            jqf_data::Integer::parse(coef).map_err(|_| jqfb::invalid("a decimal pool entry does not parse"))?;
            jqfb::read_u64(body, after).ok_or_else(|| jqfb::invalid("truncated decimal scale"))?;
        }
        Some(2) => {
            jqfb::read_u64(body, 1).ok_or_else(|| jqfb::invalid("truncated float bits"))?;
        }
        _ => return Err(jqfb::invalid("unknown number pool tag")),
    }
    Ok(())
}

// --------------------------------------------------------------------------- Slot 1: Located (scoped)
// ---------------------------------------------------------------------------

/// Native located session: validate the whole table, navigate to the exact path, publish a [`LocatedOutcome`].
pub(crate) struct NativeLocatedSession {
    image: JqfbImage,
    steps: Vec<Step>,
    origin: SelectionOrigin,
    coverage: BuilderCoverage,
    phase: Phase,
    /// The bounded subtree decode (scoped mode), driven to completion.
    decode: Option<JqfbDecodeState>,
    finalizer: Option<jqf_data::AccountedDocumentFinalizer<'static>>,
    /// The published product and selection, assembled once.
    outcome: Option<(DocumentProduct<'static>, ExactSelectionRecord)>,
    published: bool,
}

enum Phase {
    Locate,
    Decode,
    Finalize,
    Publish,
}

impl NativeLocatedSession {
    pub(crate) fn try_new(
        image: JqfbImage,
        steps: &[PortableStep],
        origin: SelectionOrigin,
        coverage: BuilderCoverage,
    ) -> Result<Self, CodecError> {
        Ok(Self {
            image,
            steps: own_steps(steps)?,
            origin,
            coverage,
            phase: Phase::Locate,
            decode: None,
            finalizer: None,
            outcome: None,
            published: false,
        })
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the four-phase session machine: locate, decode, finalize, publish"
    )]
    fn decode_located<'source>(
        &mut self,
        source: ResolvedSource<'source>,
        context: &mut CodecRunContext<'_, '_>,
    ) -> Result<AccessResult<'source>, CodecError> {
        if self.published {
            return Err(data_contract());
        }
        let chunks = self.image.slice(source.bytes())?;
        loop {
            match self.phase {
                Phase::Locate => {
                    let located = locate(&self.image, &chunks, self.steps.as_slice())?;
                    match located {
                        Located::Value { start, size } => {
                            self.decode = Some(JqfbDecodeState::try_new_scoped(
                                &self.image,
                                start,
                                size,
                                self.coverage,
                                context.resources(),
                            )?);
                            self.phase = Phase::Decode;
                        }
                        negative @ (Located::Missing { .. } | Located::TypeMismatch { .. }) => {
                            let (product, selection) = negative_outcome(&negative, self.origin, context.resources())?;
                            self.outcome = Some((product, selection));
                            self.phase = Phase::Publish;
                        }
                    }
                }
                Phase::Decode => {
                    if context.resources().admit_work_transition()? == WorkAdmission::Pending {
                        context.replenish_work()?;
                        continue;
                    }
                    let decode = self.decode.as_mut().ok_or_else(data_contract)?;
                    let step = decode.decode_step(&chunks, context.resources())?;
                    if !step {
                        let finalizer = decode.finish_scoped_document(&chunks, context.resources())?;
                        self.finalizer = Some(finalizer);
                        self.phase = Phase::Finalize;
                    }
                }
                Phase::Finalize => {
                    let finalizer = self.finalizer.as_mut().ok_or_else(data_contract)?;
                    let poll = finalizer.poll(context.resources()).map_err(map_data)?;
                    let DocumentFinalizationPoll::Ready(document) = poll else {
                        context.replenish_work()?;
                        continue;
                    };
                    self.finalizer = None;
                    let product = DocumentProduct::try_new(document, context.resources())?;
                    let selection = ExactSelectionRecord::Node {
                        node: product
                            .document()
                            .node_handle(product.document().root())
                            .map_err(map_data)?,
                        origin: self.origin,
                    };
                    self.outcome = Some((product, selection));
                    self.phase = Phase::Publish;
                }
                Phase::Publish => {
                    let (product, selection) = self.outcome.take().ok_or_else(data_contract)?;
                    self.published = true;
                    let outcome = LocatedOutcome::try_new(&product, selection)?;
                    return Ok(AccessResult::from_outcome(AccessOutcome::Located(outcome)));
                }
            }
        }
    }
}

impl AccessSession for NativeLocatedSession {
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
    fn decode<'source>(
        &mut self,
        input: AccessInput<'_, 'source>,
        context: &mut CodecRunContext<'_, '_>,
    ) -> Result<AccessResult<'source>, CodecError> {
        let source = source_input(&input)?;
        self.decode_located(source, context)
    }
}

/// The one input shape these routes serve: a raw source range.
fn source_input<'s>(input: &AccessInput<'_, 's>) -> Result<ResolvedSource<'s>, CodecError> {
    match input {
        AccessInput::Source(source) => Ok(*source),
        AccessInput::Document(_) => Err(CodecError::new(CodecFailureKind::ProviderRouteMismatch)),
    }
}

/// Validates the whole table, then navigates the exact path over it.
fn locate(image: &JqfbImage, chunks: &CoreChunks<'_>, steps: &[Step]) -> Result<Located, CodecError> {
    validate_table(
        chunks.node,
        chunks.strg,
        chunks.numb,
        &image.strg_offsets,
        &image.numb_offsets,
        image.node_count,
    )?;
    navigate(chunks.node, image.node_count, chunks.strg, &image.strg_offsets, steps)
}

/// A fresh request-accounted jqfb builder at the demand routes' minimal semantic coverage. Negative located
/// observations (missing / kind mismatch) carry a null stand-in and no facts; a located VALUE goes through
/// [`JqfbDecodeState::try_new_scoped`], which validates FACT records on the subtree and attaches them when coverage
/// demanded facts.
fn fresh_builder(_resources: &ResourceContext<'_>) -> Result<AccountedDocumentBuilder<'static>, CodecError> {
    let recipe = provider::jqfb_recipe().map_err(map_data)?;
    let (mut builder, _schema) =
        AccountedDocumentBuilder::try_new_prepared_with_coverage(&recipe, BuilderCoverage::minimal_semantic())
            .map_err(map_data)?;
    builder.set_authoritative_empty_families(AuthoritativeEmptyFamilies::from_family(
        DocumentCapabilityFamily::Attributes,
    ));
    builder.set_diagnostic_coverage(DiagnosticCoverage::NotRequested);
    Ok(builder)
}

/// Builds the null-product for a negative located observation (missing member or kind mismatch), carrying the exact
/// selection record.
fn negative_outcome(
    located: &Located,
    origin: SelectionOrigin,
    resources: &mut ResourceContext<'_>,
) -> Result<(DocumentProduct<'static>, ExactSelectionRecord), CodecError> {
    let mut builder = fresh_builder(resources)?;
    let root = builder
        .add_node(
            provider::kind_for("jqfb", &AccountedSemanticNode::Null),
            AccountedSemanticNode::Null,
            None,
            resources,
        )
        .map_err(map_data)?;
    let selection = match located {
        Located::Missing { step } => ExactSelectionRecord::Missing {
            step_index: *step,
            origin,
        },
        Located::TypeMismatch { step, actual } => ExactSelectionRecord::TypeMismatch {
            step_index: *step,
            actual_type: *actual,
            origin,
            hint: None,
        },
        Located::Value { .. } => return Err(data_contract()),
    };
    let document = builder.finish(root, resources).map_err(map_data)?;
    let product = DocumentProduct::try_new(document, resources)?;
    Ok((product, selection))
}

fn map_data(error: DataError) -> CodecError {
    jqf_codec_core::map_data(error, "jqfb builder rejected document construction")
}

fn data_contract() -> CodecError {
    jqf_codec_core::data_contract("jqfb demand route construction")
}
