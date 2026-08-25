//! Who owns which occurrences, derived in one forward pass.
//!
//! Depth-first authoring closes containers in LIFO order, so a stack of open frames is enough. An edge added to an
//! already closed owner returns [`EdgeStep::Fallback`]; the caller then uses a counting-sort.
//!
//! On `Ok(None)` or `Err`, open-frame markers may still sit on `nodes`. Run the fallback or discard the builder. Do not
//! re-enter this pass.

use alloc::vec::Vec;

use super::storage::{NodeRecord, StorageRange};
use super::{DataError, LocalOwnerRef, NodeId, OccurrenceId, OccurrenceRecord};

/// `occurrence_range.start` sentinel marking an owner whose frame is still open.
///
/// A real finished start could only reach `u32::MAX` with 2^32 occurrences — far past any enforced resource limit —
/// and the guard in [`derive_accounted`] that routes to the fallback at `occurrences.len() >= u32::MAX as usize`
/// enforces the non-collision rather than assuming it.
const OPEN_MARKER: u32 = u32::MAX;

/// One open owner frame: the owner node and the offset of its edges within the scratch stack.
#[derive(Clone, Copy)]
struct Frame {
    owner: NodeId,
    start: usize,
}

/// The action the stack pass must take for one authored occurrence.
enum EdgeStep {
    /// A root-owned occurrence: it carries an edge id but no node adjacency.
    Skip,
    /// Extend the innermost open frame.
    Append,
    /// Open a new innermost frame for `owner`, then extend it.
    Enter(NodeId),
    /// Return to the already-open owner at stack index `to`, closing every deeper frame first, then extend it.
    Return { to: usize },
    /// The authoring order is not stack-disciplined; the fast path cannot apply.
    Fallback,
}

/// Classifies one authored occurrence in constant time against the open-frame markers each owner carries in its
/// `occurrence_range`.
///
/// An open owner's range is `{ start: OPEN_MARKER, len: stack_index }`; a never seen owner has the default empty range;
/// a closed owner has a real `(start, len)` with `len > 0`. Reopening a closed owner is only possible with
/// non-depth-first authoring and demands the counting-sort fallback.
fn classify(frames: &[Frame], nodes: &[NodeRecord], owner: LocalOwnerRef) -> EdgeStep {
    let owner = match owner {
        LocalOwnerRef::DocumentRoot => return EdgeStep::Skip,
        LocalOwnerRef::Node(node) => node,
        LocalOwnerRef::Occurrence(_) => return EdgeStep::Fallback,
    };
    if frames.last().is_some_and(|frame| frame.owner == owner) {
        return EdgeStep::Append;
    }
    let frames_len = frames.len();
    let index = owner.index();
    let Some(record) = nodes.get(index) else {
        return EdgeStep::Fallback;
    };
    let range = record.occurrence_range;
    if range.start == OPEN_MARKER {
        let stack_index = range.len as usize;
        if stack_index + 1 == frames_len {
            EdgeStep::Append
        } else {
            EdgeStep::Return { to: stack_index }
        }
    } else if range.len == 0 {
        EdgeStep::Enter(owner)
    } else {
        EdgeStep::Fallback
    }
}

/// Marks `owner` as the open frame at `stack_index` by borrowing its range.
fn mark_open(nodes: &mut [NodeRecord], owner: NodeId, stack_index: usize) -> Result<(), DataError> {
    let index = owner.index();
    nodes.get_mut(index).ok_or(DataError::InvalidNode)?.occurrence_range = StorageRange {
        start: OPEN_MARKER,
        len: u32::try_from(stack_index).map_err(|_| DataError::ArithmeticOverflow)?,
    };
    Ok(())
}

/// Writes a frame owner's finished range, replacing its open marker.
fn assign_range(nodes: &mut [NodeRecord], owner: NodeId, start: usize, len: usize) -> Result<(), DataError> {
    let index = owner.index();
    nodes.get_mut(index).ok_or(DataError::InvalidNode)?.occurrence_range = StorageRange::try_new(start, len)?;
    Ok(())
}

/// Assigns each still-open frame's finished `occurrence_range`. The remaining frames hold their edges contiguously in
/// `scratch` in frame order, so frame `i` owns `scratch[frame[i].start .. frame[i + 1].start]` (or `scratch.len()` for
/// the innermost frame). `base` is the offset at which the scratch is appended onto the finished output (zero when the
/// scratch is returned unchanged).
fn assign_open_frames(
    frames: &[Frame],
    scratch_len: usize,
    base: usize,
    nodes: &mut [NodeRecord],
) -> Result<(), DataError> {
    for (position, frame) in frames.iter().enumerate() {
        let end = frames.get(position + 1).map_or(scratch_len, |next| next.start);
        assign_range(nodes, frame.owner, base + frame.start, end - frame.start)?;
    }
    Ok(())
}

/// True once `occurrences.len()` reaches the point where a finished `occurrence_range.start` could equal
/// [`OPEN_MARKER`] (see its doc comment for the bound argument). Both `derive_*` entry points take the fallback once
/// this holds, rather than relying on the bound never being reached.
pub(super) fn open_marker_reachable(occurrences_len: usize) -> bool {
    occurrences_len >= OPEN_MARKER as usize
}

/// Derives owner adjacency in one stack pass for the accounted builder's synchronous path.
///
/// Returns `Ok(None)` when the authoring order is not stack-disciplined, or when `occurrences` is large enough that a
/// finished range's start could collide with [`OPEN_MARKER`]; the caller then falls back to the counting-sort. Node
/// `occurrence_range`s written before a fallback are irrelevant because the counting-sort unconditionally reassigns
/// every node's range, so no `OPEN_MARKER` survives a fallback. On `Ok(None)` or `Err(_)`, the caller must either run
/// that fallback or discard the builder — see this module's contract section for why re-entering the fast pass on the
/// same `nodes` afterward is forbidden.
pub(super) fn derive_accounted(
    nodes: &mut [NodeRecord],
    occurrences: &[OccurrenceRecord],
) -> Result<Option<Vec<OccurrenceId>>, DataError> {
    if open_marker_reachable(occurrences.len()) {
        return Ok(None);
    }
    // One whole-length step of the cooperative pass: the synchronous path is the resumable machine driven with a single
    // all-covering grant.
    let mut pass = CooperativeAdjacency::new(occurrences.len())?;
    pass.step(occurrences.len(), nodes, occurrences)?;
    pass.finish(nodes)
}

/// The resumable form of [`derive_accounted`] the cooperative finalizer drives. The counting-sort exists in the
/// finalizer because the synchronous pass is one unbroken loop a poll cannot interrupt; this pass splits the same loop
/// at any occurrence boundary while keeping the full adjacency state (`frames`, `scratch`, `output`, `cursor`) on the
/// finalizer between polls, so cooperative admission — a batch still observes control within one credit budget —
/// survives the single-pass build.
///
/// The `nodes` open-frame markers written by each `step` persist across polls because the finalizer holds the node
/// table; the pass is entered with every owner's range default and exits with every marker resolved or a fallback
/// declared (the caller then runs the counting-sort, which unconditionally reassigns every node's range — same
/// contract as [`derive_accounted`]).
pub(super) struct CooperativeAdjacency {
    total: usize,
    cursor: usize,
    frames: Vec<Frame>,
    scratch: Vec<OccurrenceId>,
    output: Option<Vec<OccurrenceId>>,
    /// The authoring order broke the stack discipline: stop and let the caller fall back to the order-agnostic
    /// counting-sort.
    fallback: bool,
}

impl CooperativeAdjacency {
    /// Starts the pass. The scratch is pre-sized to the occurrence count so that, when no frame closes mid-pass, it is
    /// returned as the finished adjacency without a copy (the same reservation [`derive_accounted`] makes); the caller
    /// falls back to the counting-sort when the marker bound is reachable.
    pub(super) fn new(occurrences_len: usize) -> Result<Self, DataError> {
        let mut scratch = Vec::new();
        scratch
            .try_reserve_exact(occurrences_len)
            .map_err(jqf_resource::ResourceError::from)?;
        Ok(Self {
            total: occurrences_len,
            cursor: 0,
            frames: Vec::new(),
            scratch,
            output: None,
            fallback: false,
        })
    }

    /// Occurrences still to classify in this pass.
    pub(super) fn remaining(&self) -> usize {
        self.total - self.cursor
    }

    /// Whether [`Self::step`] declared the counting-sort fallback.
    pub(super) fn is_fallback(&self) -> bool {
        self.fallback
    }

    /// Classifies `granted` occurrences at `cursor`, returning `Ok(true)` when the pass is complete (every occurrence
    /// classified, or the fallback declared). The open-frame markers in `nodes` are the pass state and survive between
    /// calls.
    pub(super) fn step(
        &mut self,
        granted: usize,
        nodes: &mut [NodeRecord],
        occurrences: &[OccurrenceRecord],
    ) -> Result<bool, DataError> {
        let end = self.cursor + granted;
        for (relative, record) in occurrences[self.cursor..end].iter().enumerate() {
            let index = self.cursor + relative;
            let id = OccurrenceId::try_from_index(index).ok_or(DataError::ArithmeticOverflow)?;
            match classify(self.frames.as_slice(), nodes, record.owner) {
                EdgeStep::Skip => {}
                EdgeStep::Append => self.scratch.push(id),
                EdgeStep::Enter(owner) => {
                    let start = self.scratch.len();
                    mark_open(nodes, owner, self.frames.len())?;
                    self.frames.push(Frame { owner, start });
                    self.scratch.push(id);
                }
                EdgeStep::Return { to } => {
                    while self.frames.len() > to + 1 {
                        let frame = self.frames.pop().ok_or(DataError::InvalidDocument)?;
                        flush_frame(frame, &mut self.scratch, &mut self.output, nodes, self.total)?;
                    }
                    self.scratch.push(id);
                }
                EdgeStep::Fallback => {
                    self.fallback = true;
                    return Ok(true);
                }
            }
        }
        self.cursor = end;
        Ok(self.cursor == self.total)
    }

    /// Assigns the still-open frames' finished ranges and produces the grouped adjacency, mirroring
    /// [`derive_accounted`]'s completion. The fallback path returns `Ok(None)` and the caller runs the counting- sort;
    /// the pass's scratch is dropped with it.
    pub(super) fn finish(self, nodes: &mut [NodeRecord]) -> Result<Option<Vec<OccurrenceId>>, DataError> {
        if self.fallback {
            return Ok(None);
        }
        let Some(mut output) = self.output else {
            // No frame closed mid-pass: the scratch is the finished adjacency.
            assign_open_frames(self.frames.as_slice(), self.scratch.len(), 0, nodes)?;
            return Ok(Some(self.scratch));
        };
        assign_open_frames(self.frames.as_slice(), self.scratch.len(), output.len(), nodes)?;
        output
            .try_reserve_exact(self.scratch.len())
            .map_err(jqf_resource::ResourceError::from)?;
        output.extend_from_slice(self.scratch.as_slice());
        Ok(Some(output))
    }
}

/// Records a closed frame's edges into the output, allocating the output arena on first use. The frame's edges occupy
/// the top of `scratch`; they are popped off once copied so the ancestor frame stays contiguous.
///
/// The first flush reserves `total` (the occurrence count), not `scratch.len()`. Reserving at the first closed frame
/// would pin the ledger high-water to that frame's scratch prefix; the remaining occurrences still land in the same
/// arena, so the known total is the charge that belongs here.
fn flush_frame(
    frame: Frame,
    scratch: &mut Vec<OccurrenceId>,
    output: &mut Option<Vec<OccurrenceId>>,
    nodes: &mut [NodeRecord],
    total: usize,
) -> Result<(), DataError> {
    let len = scratch.len() - frame.start;
    let output = match output {
        Some(output) => output,
        none => {
            let mut fresh = Vec::new();
            fresh
                .try_reserve_exact(total)
                .map_err(jqf_resource::ResourceError::from)?;
            none.insert(fresh)
        }
    };
    let start = output.len();
    output.extend_from_slice(&scratch.as_slice()[frame.start..]);
    assign_range(nodes, frame.owner, start, len)?;
    scratch.truncate(frame.start);
    Ok(())
}
