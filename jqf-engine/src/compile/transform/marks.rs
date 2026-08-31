//! Post-fuse marking passes on the fused arena.

use crate::program::ProgramNode;

/// Marks tail calls, keyed collects, binary shapes, and static object keys.
///
/// Tail-call detection walks each distinct callable body (DFS); the keyed-
/// collect and post-fuse fact passes are linear arena scans. One orchestrator
/// keeps the pipeline at a single call site while preserving pass order.
pub(crate) fn mark_post_fuse(nodes: &mut [ProgramNode]) {
    crate::analysis::mark_tail_calls(nodes);
    crate::exec::GraphMachine::mark_keyed_collects(nodes);
    crate::exec::mark_post_fuse_facts(nodes);
}
