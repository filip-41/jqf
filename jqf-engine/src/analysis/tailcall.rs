//! Tail-call detection for user-defined callables.
//!
//! After [`super::fuse_with_callables`] resolves every call site to its shared
//! body, this pass walks each distinct callable body and marks self-calls
//! whose evaluation the engine's trampoline may REPLACE with a re-evaluation
//! of the body — the `tail` flag, consumed at run time without a second walk.
//!
//! The trampoline is sound for a self-call whenever the call's continuation is
//! exactly the frames between the call and its routing position: it re-runs
//! the body over the call's input at the call's own `cont`, so the body's
//! outputs land where the call's outputs would, its raises route at the
//! call's raise position, and the continuation frames the call would have
//! driven from a nested machine stay on the caller's stack and are re-applied
//! once per level as the outputs unwind. The re-run is the nested machine's
//! own drive (the body from its top, over the call's input), so any position
//! whose frames between it and the routing position are exactly the call's
//! continuation is marked; positions whose continuation owes work the re-run
//! would skip are not. The marked set is therefore the classic tail (a pipe's
//! RIGHT member), plus:
//!
//! - a comma's LAST member (`1, f`): its outputs ARE the comma's final
//!   outputs — the frame that produced the left member's output holds no
//!   obligation the trampoline would violate;
//! - a pipe's LEFT member (`nat | . + 1`): not classic tail, but each round's
//!   re-evaluation emits into the same [`super::super::exec`]-side
//!   `FlatMapBody` the call's own outputs would have entered, and the
//!   accumulated `FlatMapBody` frames re-apply that level's continuation
//!   (`. + 1`) exactly once — the same net effect as one nested machine per
//!   level, at a flat callable depth;
//! - the two arms of a `Conditional` (`if C then f else 0 end`): the
//!   re-evaluation re-runs the whole body from its top, so `C` is evaluated
//!   once per level exactly as it is by one nested machine per level;
//! - a `Try` body, a `Bind` body, a `Label` body, and a shared `?//`
//!   `ChainBody`: the re-evaluation re-enters the frame from the body's top,
//!   so a raise re-routes at the call's raise position inside the frame,
//!   exactly as a nested machine's would.
//!
//! This is what makes `limit(n; f)` over a recursive generator keep a flat
//! callable depth for `n` well past the depth ceiling: the trampoline re-runs
//! the body instead of nesting a machine per level, so the callable depth
//! never grows at all. A self-call in any OTHER position (a comma's LEFT
//! member, a binder's SOURCE, a call's argument, a collection, an object
//! member, a `+` operand) is
//! left unmarked: its continuation genuinely cannot run until the call
//! returns, so it nests and pays the callable-depth ceiling — the honest
//! bound the ceiling exists to enforce.

use crate::program::{ProgramNode, ProgramNodeId};
use alloc::vec::Vec;

pub(crate) fn mark_tail_calls(nodes: &mut [ProgramNode]) {
    let mut body_ids: Vec<ProgramNodeId> = Vec::new();
    for node in nodes.iter() {
        if let ProgramNode::CallDef { body, .. } = node {
            body_ids.push(*body);
        }
    }
    // Sort + dedup instead of a per-push `contains` scan: def count is not
    // bounded by the nesting ceiling (flat `def f0: .; def f1: .; …`), so a
    // linear membership scan would cost ~N²/2 id compares while compiling.
    body_ids.sort_unstable_by_key(|id| id.index());
    body_ids.dedup_by(|a, b| a.index() == b.index());
    for body_id in body_ids {
        mark_tail_in_body(nodes, body_id, body_id);
    }
}

fn mark_tail_in_body(nodes: &mut [ProgramNode], node: ProgramNodeId, body_id: ProgramNodeId) {
    match &mut nodes[node.index()] {
        ProgramNode::CallDef { body, tail, .. } => {
            if *body == body_id {
                *tail = true;
            }
        }
        ProgramNode::Conditional {
            consequent,
            alternative,
            ..
        } => {
            let (cons, alt) = (*consequent, *alternative);
            mark_tail_in_body(nodes, cons, body_id);
            mark_tail_in_body(nodes, alt, body_id);
        }
        // The comma's RIGHT member is in tail position: its outputs are the
        // comma's final outputs (the left member's outputs all precede them),
        // so `def f: 1, f` trampolines its self-call instead of nesting one
        // level per emitted output. The LEFT member is
        // never tail (its outputs precede the right member's).
        ProgramNode::Choice { right, .. } => {
            let right = *right;
            mark_tail_in_body(nodes, right, body_id);
        }
        // A pipe's RIGHT member is classic tail (the existing arm). Its LEFT
        // member is marked too: the pipe's continuation is re-applied per
        // output by the FlatMapBody frame the trampolined body emits into, so
        // `def nat: 1, (nat | . + 1)` runs flat as well — see the module
        // doc for the soundness argument.
        ProgramNode::FlatMap { upstream, body, .. } => {
            let (upstream, body) = (*upstream, *body);
            mark_tail_in_body(nodes, upstream, body_id);
            mark_tail_in_body(nodes, body, body_id);
        }
        ProgramNode::Label { body, .. }
        | ProgramNode::ChainBody { body }
        | ProgramNode::Bind { body, .. }
        | ProgramNode::Try { body, .. } => {
            let body = *body;
            mark_tail_in_body(nodes, body, body_id);
        }
        _ => {}
    }
}
