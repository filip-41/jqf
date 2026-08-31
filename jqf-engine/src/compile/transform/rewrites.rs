//! Shape-specific peephole rewrites on the fused arena.

use super::super::ParseRejection;
use super::super::error::EngineCompileError;
use super::super::lower::MAX_LOWERED_NODES;
use super::super::prelude::*;
use crate::program::{ProgramNode, ProgramNodeId};
use alloc::collections::BTreeMap;

/// `[STREAM] | add` → `reduce STREAM as $x (null; . + $x)`.
///
/// `add/0` folds the input array's children; collecting the stream only to
/// fold it is the peak the element/count rows already avoid. The rewrite
/// keeps `add`'s empty-input `null` and the native `+` update, so the
/// executor's ordinary reduce path serves it.
pub(crate) fn rewrite_collect_add(
    nodes: &mut Vec<ProgramNode>,
    root: ProgramNodeId,
    slots: &mut u32,
) -> Result<ProgramNodeId, EngineCompileError> {
    let Some(stream) = collect_add_stream(nodes, root)? else {
        return Ok(root);
    };
    let slot = *slots;
    *slots = slot.checked_add(1).ok_or_else(|| {
        EngineCompileError::Parse(ParseRejection::internal(
            "program exceeds the variable slot addressing bound",
        ))
    })?;
    let init = push_rewrite_node(
        nodes,
        ProgramNode::Stage {
            start: StageStart::Literal(Value::Null),
            steps: Vec::new(),
        },
    )
    .ok_or(EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    let acc = push_rewrite_node(
        nodes,
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: Vec::new(),
        },
    )
    .ok_or(EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    let addend = push_rewrite_node(
        nodes,
        ProgramNode::Stage {
            start: StageStart::Variable(slot),
            steps: Vec::new(),
        },
    )
    .ok_or(EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    let update = push_rewrite_node(nodes, ProgramNode::binary(BinaryKind::Add, acc, addend))
        .ok_or(EngineCompileError::Resource(ResourceError::AllocationFailed))?;
    push_rewrite_node(
        nodes,
        ProgramNode::Reduce {
            source: stream,
            slot,
            init,
            update,
            keyed_collect: None,
        },
    )
    .ok_or(EngineCompileError::Resource(ResourceError::AllocationFailed))
}

/// `[STREAM] | add`, or `PATH | (map(f) | add)` — pipe is right-associative, so
/// the latter is `FlatMap(PATH, FlatMap(Collect, add))`. The PATH spelling
/// prepends the static prefix onto the collect body in place so the reduce
/// source is `[PATH[] | f]`.
pub(crate) fn collect_add_stream(
    nodes: &mut [ProgramNode],
    root: ProgramNodeId,
) -> Result<Option<ProgramNodeId>, EngineCompileError> {
    let ProgramNode::FlatMap { upstream, body } = nodes[root.index()] else {
        return Ok(None);
    };
    if is_add_zero(nodes, body) {
        if let ProgramNode::CollectArray { body: Some(stream) } = nodes[upstream.index()] {
            return Ok(Some(stream));
        }
        return Ok(None);
    }
    let Some(prefix) = crate::analysis::path_steps::static_member_stage_steps(nodes, upstream) else {
        return Ok(None);
    };
    let ProgramNode::FlatMap {
        upstream: collect,
        body: add,
    } = nodes[body.index()]
    else {
        return Ok(None);
    };
    if !is_add_zero(nodes, add) {
        return Ok(None);
    }
    let ProgramNode::CollectArray { body: Some(stream) } = nodes[collect.index()] else {
        return Ok(None);
    };
    if !crate::analysis::path_steps::prepend_in_place(nodes, stream, &prefix).map_err(EngineCompileError::Resource)? {
        return Ok(None);
    }
    Ok(Some(stream))
}

/// Applies [`rewrite_collect_add`] to every distinct fused callable body and
/// re-points each rewritten def's [`CallDef`] nodes at the new reduce root.
///
/// The body ids come from [`fuse_with_callables`]'s returned map — the FUSED
/// arena, which is compacted and renumbered. The pre-fuse ids the callables
/// carried out of lowering index a different arena and would read the wrong
/// node or run off its end.
pub(crate) fn rewrite_collect_add_callables(
    nodes: &mut Vec<ProgramNode>,
    fused_body_ids: &BTreeMap<usize, ProgramNodeId>,
    slots: &mut u32,
) -> Result<(), EngineCompileError> {
    // One rewrite per DISTINCT fused body (two defs may share one body node);
    // a def whose body root did not match keeps its id. Sort + dedup instead
    // of a per-push `contains` scan: def count is not bounded by the nesting
    // ceiling (flat `def f0: .; def f1: .; …`), so a linear membership scan
    // costs ~N²/2 id compares while compiling.
    let mut distinct: Vec<ProgramNodeId> = fused_body_ids.values().copied().collect();
    distinct.sort_unstable_by_key(|id| id.index());
    distinct.dedup_by(|a, b| a.index() == b.index());
    let mut rewrites: Vec<(ProgramNodeId, ProgramNodeId)> = Vec::new();
    for body in distinct {
        let new_root = rewrite_collect_add(nodes, body, slots)?;
        if new_root.index() != body.index() {
            rewrites.push((body, new_root));
        }
    }
    // `CallDef.body` holds the FUSED body id; every reference to a rewritten
    // body must now target the reduce root the rewrite appended.
    for node in nodes.iter_mut() {
        if let ProgramNode::CallDef { body, .. } = node
            && let Some((_, new_root)) = rewrites.iter().find(|(old, _)| old.index() == body.index())
        {
            *body = *new_root;
        }
    }
    Ok(())
}

pub(crate) fn is_add_zero(nodes: &[ProgramNode], id: ProgramNodeId) -> bool {
    let ProgramNode::Call { overload, args, .. } = &nodes[id.index()] else {
        return false;
    };
    overload.get() == jqf_builtins::registry::builtins::id::ADD_ZERO && args.is_empty()
}

/// Appends one node to a rewrite arena, mirroring the lowerer's arena law:
/// the lowered-node ceiling and allocation-failure mapping (the
/// arena-level rewrite reuses the shared compile-time bound).
pub(crate) fn push_rewrite_node(nodes: &mut Vec<ProgramNode>, node: ProgramNode) -> Option<ProgramNodeId> {
    if nodes.len() >= MAX_LOWERED_NODES {
        return None;
    }
    let id = ProgramNodeId::from_index(nodes.len())?;
    nodes.try_reserve(1).ok()?;
    nodes.push(node);
    Some(id)
}
