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
//! What the route materializes is the locate product: count/element Exact publishes the
//! landed subtree as a container span with the payload child count already in the node
//! table. Print without that hint still scoped-decodes `[start, start + subtree_size)`.

use alloc::vec::Vec;

use jqf_codec_core::{
    AccessInput, AccessOutcome, AccessResult, AccessSession, CodecError, CodecFailureKind, CodecRunContext,
    DocumentProduct, ExactSelectionRecord, LocatedOutcome, OwnedStep, PortableStep, SelectionOrigin, own_steps,
};
use jqf_data::{
    AccountedDocumentBuilder, AccountedSemanticNode, AuthoritativeEmptyFamilies, BuilderCoverage, ContainerSpanKind,
    DataError, DiagnosticCoverage, DocumentCapabilityFamily, DocumentCapacity, DocumentFinalizationPoll,
    DocumentSourceBindingPoll, DocumentSourceBindingStage, LazySpanMaterializer, TagId, ValueKind,
};
use jqf_resource::{ResourceContext, WorkAdmission};
use jqf_source::ResolvedSource;

use crate::jqfb::{self, CoreChunks, kinds};
use crate::jqfb_decode::{JqfbDecodeState, JqfbImage};
use crate::parse::try_temporal;
use crate::provider;

/// The located answer of the node-table walk.
#[derive(Debug)]
enum Located {
    /// The subtree at the target path: node-table entries `[start, start + subtree_size)`.
    Value {
        start: usize,
        size: u32,
        container: Option<(ContainerSpanKind, u64)>,
    },
    /// The step at which navigation stopped: no member or position exists.
    Missing { step: usize },
    /// The step at which a kind mismatch stopped the path.
    TypeMismatch { step: usize, actual: ValueKind },
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

fn located_value(node: &[u8], start: usize, size: u32) -> Result<Located, CodecError> {
    Ok(Located::Value {
        start,
        size,
        container: container_facts(node, start)?,
    })
}

/// Unwraps payload-transparent tags so Exact count caches the ARRAY/OBJECT payload count.
fn container_facts(node: &[u8], start: usize) -> Result<Option<(ContainerSpanKind, u64)>, CodecError> {
    let mut cursor = start;
    loop {
        let entry = jqfb::read_node(node, cursor)?;
        match entry.kind {
            kinds::TAG => {
                cursor = cursor
                    .checked_add(1)
                    .ok_or_else(|| jqfb::invalid("tag payload index overflows"))?;
            }
            kinds::ARRAY => {
                return Ok(Some((ContainerSpanKind::Array, u64::from(entry.payload))));
            }
            kinds::OBJECT => {
                return Ok(Some((ContainerSpanKind::Object, u64::from(entry.payload))));
            }
            _ => return Ok(None),
        }
    }
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
    steps: &[OwnedStep],
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
    steps: &[OwnedStep],
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
            return located_value(node, head_start, head.subtree_size);
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
                OwnedStep::Index(index) => {
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
                OwnedStep::Member(_) => {
                    *cursor = end;
                    Ok(Located::TypeMismatch {
                        step,
                        actual: ValueKind::Array,
                    })
                }
                OwnedStep::Range { .. } => Err(decline_located_range()),
            }
        }
        kinds::OBJECT => {
            let members = usize::try_from(entry.payload).map_err(|_| jqfb::invalid("object member count overflows"))?;
            match &steps[step] {
                OwnedStep::Member(name) => {
                    // Duplicate KEYTEXT is last-wins, not unique-key physics.
                    let mut child = start + 1;
                    let mut winner = None;
                    for _ in 0..members {
                        let key = jqfb::read_node(node, child)?;
                        if key.kind != kinds::KEYTEXT {
                            return Err(jqfb::invalid("an object member has no text key"));
                        }
                        let key_text = strg_text(strg, strg_offsets, key.payload)?;
                        child += 1; // the key is a leaf (size 1)
                        if key_text.as_bytes() == name.as_str().as_bytes() {
                            winner = Some(child);
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
                    let Some(mut target) = winner else {
                        *cursor = end;
                        return Ok(Located::Missing { step });
                    };
                    let located = walk_value(node, node_count, strg, strg_offsets, &mut target, step + 1, steps)?;
                    *cursor = end;
                    Ok(located)
                }
                OwnedStep::Index(_) => {
                    *cursor = end;
                    Ok(Located::TypeMismatch {
                        step,
                        actual: ValueKind::Object,
                    })
                }
                OwnedStep::Range { .. } => Err(decline_located_range()),
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
    steps: Vec<OwnedStep>,
    origin: SelectionOrigin,
    coverage: BuilderCoverage,
    /// Count/element Exact: publish the node-table subtree as a cached container span.
    skeleton: bool,
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
        skeleton: bool,
    ) -> Result<Self, CodecError> {
        Ok(Self {
            image,
            steps: own_steps(steps)?,
            origin,
            coverage,
            skeleton,
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
                        Located::Value { start, size, container } => {
                            if self.skeleton
                                && let Some((kind, child_count)) = container
                            {
                                let product = publish_located_skeleton(
                                    source,
                                    chunks.node,
                                    start,
                                    size,
                                    kind,
                                    child_count,
                                    context,
                                )?;
                                let selection = ExactSelectionRecord::Node {
                                    node: product
                                        .document()
                                        .node_handle(product.document().root())
                                        .map_err(map_data)?,
                                    origin: self.origin,
                                };
                                self.published = true;
                                let outcome = LocatedOutcome::try_new(&product, selection)?;
                                return Ok(AccessResult::from_outcome(AccessOutcome::Located(outcome)));
                            }
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
fn locate(image: &JqfbImage, chunks: &CoreChunks<'_>, steps: &[OwnedStep]) -> Result<Located, CodecError> {
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
fn fresh_builder(resources: &ResourceContext<'_>) -> Result<AccountedDocumentBuilder<'static>, CodecError> {
    let recipe = provider::jqfb_recipe().map_err(map_data)?;
    let (mut builder, _schema) =
        AccountedDocumentBuilder::try_new_prepared_with_coverage(&recipe, BuilderCoverage::minimal_semantic())
            .map_err(map_data)?;
    let _ = builder.try_reserve(
        DocumentCapacity {
            nodes: 1,
            ..DocumentCapacity::default()
        },
        resources,
    );
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

struct JqfbSpanMaterializer;

impl LazySpanMaterializer for JqfbSpanMaterializer {
    fn materialize_span(
        &self,
        _text: &str,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<jqf_data::Value, DataError> {
        Err(DataError::InvalidDocument)
    }

    fn materialize_span_bytes(
        &self,
        _bytes: &[u8],
        _resources: &mut ResourceContext<'_>,
    ) -> Result<jqf_data::Value, DataError> {
        Err(DataError::InvalidDocument)
    }
}

static JQFB_SPAN_MATERIALIZER: JqfbSpanMaterializer = JqfbSpanMaterializer;

/// Publishes the landed node-table subtree as a lazy span root. `validate_table` already
/// proved the image; `navigate` already named `{start, size}` and the payload count.
/// Count reads the cache — no second scoped decode of the hit.
#[allow(
    unsafe_code,
    reason = "span admission and source attach are unsafe by jqf-data; validate_table proved this subtree"
)]
fn publish_located_skeleton<'source>(
    source: ResolvedSource<'source>,
    node: &'source [u8],
    start: usize,
    size: u32,
    container: ContainerSpanKind,
    child_count: u64,
    context: &mut CodecRunContext<'_, '_>,
) -> Result<DocumentProduct<'source>, CodecError> {
    let byte_start = start
        .checked_mul(kinds::ENTRY_LEN)
        .ok_or_else(|| jqfb::invalid("node span start overflows"))?;
    let byte_end = start
        .checked_add(usize::try_from(size).map_err(|_| jqfb::invalid("subtree size overflows"))?)
        .and_then(|end| end.checked_mul(kinds::ENTRY_LEN))
        .ok_or_else(|| jqfb::invalid("node span end overflows"))?;
    let bytes = node
        .get(byte_start..byte_end)
        .ok_or_else(|| jqfb::invalid("located subtree exceeds the NODE chunk"))?;
    let base = 0u64;
    let sub = ResolvedSource::new(source.source(), source.label(), bytes, base);
    let recipe = provider::jqfb_recipe().map_err(map_data)?;
    let (mut builder, schema) =
        AccountedDocumentBuilder::try_new_prepared_with_coverage(&recipe, BuilderCoverage::minimal_semantic())
            .map_err(map_data)?;
    builder.set_authoritative_empty_families(AuthoritativeEmptyFamilies::from_family(
        DocumentCapabilityFamily::Attributes,
    ));
    builder.set_diagnostic_coverage(DiagnosticCoverage::NotRequested);
    builder.bind_span_materializer(&JQFB_SPAN_MATERIALIZER as &dyn LazySpanMaterializer);
    let mut stage = DocumentSourceBindingStage::new(sub).map_err(map_data)?;
    let binding = loop {
        // SAFETY: codec-core holds this session's source unchanged; `sub` is the
        // node-table range navigate recorded on that authority.
        match unsafe { stage.poll(sub, context.resources()) }.map_err(map_data)? {
            DocumentSourceBindingPoll::Pending => context.replenish_work()?,
            DocumentSourceBindingPoll::Ready(binding) => break binding,
        }
    };
    builder.bind_source(binding).map_err(map_data)?;
    let span = jqf_source::Span::from_usize(0, bytes.len());
    let slot = match container {
        ContainerSpanKind::Array => 10,
        ContainerSpanKind::Object => 11,
    };
    let kind = schema.node_kind(slot).ok_or_else(data_contract)?;
    // SAFETY: validate_table proved this contiguous subtree on the session's image.
    let root =
        unsafe { builder.add_prepared_bound_container_span_node(&schema, kind, span, container, context.resources()) }
            .map_err(map_data)?;
    builder
        .set_container_span_counts(root, Some(child_count), None)
        .map_err(map_data)?;
    let document = builder.finish(root, context.resources()).map_err(map_data)?;
    let document =
        unsafe { document.with_borrowed_source_from_bound_authority(sub, context.resources()) }.map_err(map_data)?;
    DocumentProduct::try_new(document, context.resources())
}

fn map_data(error: DataError) -> CodecError {
    jqf_codec_core::map_data(error, "jqfb builder rejected document construction")
}

fn data_contract() -> CodecError {
    jqf_codec_core::data_contract("jqfb demand route construction")
}

/// A located route that cannot serve the demanded step shape declines, so the binder's whole-document floor serves the
/// demand instead.
fn decline_located_range() -> CodecError {
    CodecError::new(CodecFailureKind::RequirementMismatch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jqfb_decode::JqfbImage;
    use jqf_codec_core::{EncodeItem, EncodeRequest, PreservationRequest};
    use jqf_data::{Array, DialectId, FormatId, Integer, Number, Value};
    use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

    fn resources() -> ResourceContext<'static> {
        ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                .expect("account"),
            &ContinueControl,
            WorkMeter::try_new_v1(4_096).expect("work"),
        )
        .expect("resources")
    }

    fn encode_array(values: &[i64]) -> alloc::vec::Vec<u8> {
        let items: alloc::vec::Vec<Value> = values
            .iter()
            .map(|value| Value::Number(Number::integer(Integer::from_i64(*value))))
            .collect();
        let array = Value::Array(Array::try_from_vec(items).expect("array"));
        let mut resources = resources();
        let registration = crate::registration_jqfb().expect("jqfb registration");
        let request = EncodeRequest {
            format: &FormatId::try_new(crate::FORMAT_ID_JQFB).expect("format"),
            dialect: &DialectId::try_new(crate::JQFB_CANONICAL_DIALECT_ID).expect("dialect"),
            diagnostics: jqf_codec_core::DiagnosticPolicy::ErrorsOnly,
            preservation: PreservationRequest::None,
            options: Some(&crate::JqfbEncodeOptions { with_source: false }),
        };
        let factory = registration
            .encoder()
            .expect("encoder")
            .create_factory(request, &mut resources)
            .expect("factory");
        let mut session = factory
            .start(EncodeItem::owned(&array), PreservationRequest::None, &mut resources)
            .expect("session");
        let mut out = alloc::vec::Vec::new();
        let mut sink = jqf_codec_core::VecByteSink::new(&mut out);
        let mut context = CodecRunContext::new(&mut resources);
        context.set_cooperative_credits(4_096);
        session.encode(&mut sink, &mut context).expect("encode");
        out
    }

    /// A RANGE step reaching the native located route declines as a requirement mismatch — the binder's
    /// whole-document floor serves ranges — never an internal contract violation from a user program.
    #[test]
    fn a_range_step_declines_to_the_floor() {
        let bytes = encode_array(&[1, 2]);
        let image = JqfbImage::validate(&bytes).expect("valid jqfb image");
        let chunks = image.slice(&bytes).expect("core chunks");
        let error = locate(&image, &chunks, &[OwnedStep::Range { start: None, end: None }])
            .expect_err("range declines the located route");
        assert_eq!(error.kind(), CodecFailureKind::RequirementMismatch);
    }
}
