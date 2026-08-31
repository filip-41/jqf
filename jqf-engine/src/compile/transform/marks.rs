//! Post-fuse marking passes on the fused arena.
//!
//! Tail-call detection walks each distinct callable body (DFS); the keyed-
//! collect and post-fuse fact passes are linear arena scans. One orchestrator
//! keeps the pipeline at a single call site while preserving pass order.
//! Runtime reads the stored facts; it does not re-walk the update.

use crate::program::{ModifyMode, ProgramNode, ProgramNodeId, StageStart, StepAccess, VarSlot};
use jqf_data::Value;

/// Marks tail calls, keyed collects, binary shapes, and static object keys.
pub(crate) fn mark_post_fuse(nodes: &mut [ProgramNode]) {
    crate::analysis::mark_tail_calls(nodes);
    mark_keyed_collects(nodes);
    mark_post_fuse_facts(nodes);
}

/// Classifies every Reduce node once after fusion. Runtime reads the
/// stored key graph; it does not re-walk the update.
pub(crate) fn mark_keyed_collects(nodes: &mut [ProgramNode]) {
    for index in 0..nodes.len() {
        let ProgramNode::Reduce { slot, init, update, .. } = nodes[index] else {
            continue;
        };
        let keys = keyed_collect_keys(nodes, update, slot, init);
        if let ProgramNode::Reduce { keyed_collect, .. } = &mut nodes[index] {
            *keyed_collect = keys;
        }
    }
}

/// The keyed-collect shape: a `reduce` over a source that files each row into
/// an object under the row's key — the INDEX lowering (`reduce s as $row
/// ({}; reduce ($row|idx|tostring) as $k (.; .[$k] = $row))`) and the
/// user-written spellings it inlines to (`def INDEX …` bodies, `reduce $x[]
/// as $r ({}; .[$r.k] = $r)`).
///
/// Returns the KEY GRAPH when the reduce is exactly the shape: the outer
/// init is the empty object literal, and the update is ONE `.[$k] = $row`
/// set — either the builtin lowering's inner reduce (identity init, the
/// key already bound in the inner slot) or the generic `=` form (RHS
/// bound outside the Modify, the key bound outside the path stage). The
/// RHS must be a BARE read of the loop's row — a general RHS could raise
/// or read the accumulator, which the specialized drive must not swallow or
/// misread — and the key graph must end in the builtin `tostring` or a
/// string literal, which is the only guarantee that every key is a
/// string (a non-string key would need the generic `DynVar`'s
/// index-class raise, which that drive does not invent).
///
/// Any divergence returns `None` and the generic drive runs, so the fast
/// path can never change an answer; it only makes the recognized shape
/// cheaper.
#[expect(
    clippy::too_many_lines,
    reason = "one shape table with two spelling arms (builtin reduce / generic `=`); splitting \
              it would hide which spelling is covered"
)]
pub(crate) fn keyed_collect_keys(
    nodes: &[ProgramNode],
    update: ProgramNodeId,
    outer_slot: VarSlot,
    outer_init: ProgramNodeId,
) -> Option<ProgramNodeId> {
    // The outer init must be exactly the empty object literal: the
    // specialized drive skips evaluating it (an empty literal has no
    // effect to preserve) and starts the builder empty.
    match nodes.get(outer_init.index())? {
        ProgramNode::Stage {
            start: StageStart::Literal(Value::Object(object)),
            steps,
        } if steps.is_empty() && object.is_empty() => {}
        _ => return None,
    }
    let keys = match nodes.get(update.index())? {
        // The builtin INDEX lowering: the update is the inner reduce.
        ProgramNode::Reduce {
            source: keys,
            slot: key_slot,
            init: inner_init,
            update: modify,
            keyed_collect: _,
        } => {
            // The inner init is the identity stage (the accumulator
            // passes through unchanged, which the builder-as-state
            // reproduces).
            match nodes.get(inner_init.index())? {
                ProgramNode::Stage {
                    start: StageStart::Current,
                    steps,
                } if steps.is_empty() => {}
                _ => return None,
            }
            // The inner update is one `.[$k] = $row` set: a single
            // dynamic-index path on the state root, storing the loop's
            // row directly.
            match nodes.get(modify.index())? {
                ProgramNode::Modify {
                    paths,
                    update: stored,
                    mode,
                } if *mode == ModifyMode::Set => {
                    if !single_dyn_var_path(nodes, *paths, *key_slot) {
                        return None;
                    }
                    match nodes.get(stored.index())? {
                        ProgramNode::Stage {
                            start: StageStart::Variable(slot),
                            steps,
                        } if *slot == outer_slot && steps.is_empty() => {}
                        _ => return None,
                    }
                }
                _ => return None,
            }
            *keys
        }
        // The generic `=` form (user-written defs and reduces): the RHS
        // is bound outside the Modify, and the dynamic index binds its
        // key outside the path stage.
        ProgramNode::Bind {
            source,
            slot: rhs_slot,
            body,
            frame: _,
        } => {
            // The RHS must be a BARE read of the loop's row. A general
            // input-free RHS would still be able to RAISE (`.x` on a
            // non-object row), and the generic `=` runs it with dot =
            // the accumulator — a raise this drive must not swallow.
            // The bare-slot spelling covers the entire `= $row` idiom
            // and cannot raise.
            match nodes.get(source.index())? {
                ProgramNode::Stage {
                    start: StageStart::Variable(slot),
                    steps,
                } if *slot == outer_slot && steps.is_empty() => {}
                _ => return None,
            }
            let ProgramNode::Modify {
                paths,
                update: stored,
                mode,
            } = nodes.get(body.index())?
            else {
                return None;
            };
            if *mode != ModifyMode::Set {
                return None;
            }
            // The paths generator is `KEY-STREAM as $k | .[$k]`.
            let ProgramNode::Bind {
                source: key_stream,
                slot: key_slot,
                body: path_stage,
                frame: _,
            } = nodes.get(paths.index())?
            else {
                return None;
            };
            if !single_dyn_var_path(nodes, *path_stage, *key_slot) {
                return None;
            }
            match nodes.get(stored.index())? {
                ProgramNode::Stage {
                    start: StageStart::Variable(slot),
                    steps,
                } if *slot == *rhs_slot && steps.is_empty() => {}
                _ => return None,
            }
            *key_stream
        }
        _ => return None,
    };
    // The key graph and its STRING GUARANTEE. The drive inserts keys
    // through `ObjectKey`, so a key that is not a string would need the
    // generic DynVar's index-class raise — which this drive must not
    // invent. The guarantee comes from the graph's shape: the key
    // graph's OUTPUT chain ends in the builtin tostring, or the graph
    // is a bare string literal. Anything else (a field read like
    // `.name`, whose type is unknown) DECLINES to the generic drive.
    //
    // The graph's root must read the outer binder — never the eval
    // input the specialized drive supplies — so the upstream spine
    // bottoms at a `Variable($row)` stage (the optimizer fuses
    // `$row | idx` into one stage, so the bottom stage may carry the
    // key steps; they are opaque). The OUTPUT chain is walked
    // body-first: each FlatMap's body is the next output stage, and the
    // walk must end at the builtin tostring call or a string literal.
    let tostring = jqf_builtins::registry::resolve_builtin("tostring", 0)?;
    // A bare string literal is already the whole key graph (its output
    // is the literal itself).
    if let ProgramNode::Stage {
        start: StageStart::Literal(Value::String(_)),
        steps,
    } = nodes.get(keys.index())?
        && steps.is_empty()
    {
        return Some(keys);
    }
    let ProgramNode::FlatMap {
        upstream: subject,
        body: rendered,
    } = nodes.get(keys.index())?
    else {
        return None;
    };
    let mut spine = *subject;
    loop {
        match nodes.get(spine.index())? {
            ProgramNode::FlatMap { upstream, .. } => spine = *upstream,
            ProgramNode::Stage {
                start: StageStart::Variable(slot),
                ..
            } if *slot == outer_slot => break,
            _ => return None,
        }
    }
    let mut tail = *rendered;
    loop {
        match nodes.get(tail.index())? {
            ProgramNode::FlatMap { body, .. } => tail = *body,
            ProgramNode::Call { overload, args, .. } => {
                if *overload == tostring.id && args.is_empty() {
                    return Some(keys);
                }
                return None;
            }
            ProgramNode::Stage {
                start: StageStart::Literal(Value::String(_)),
                steps,
            } if steps.is_empty() => return Some(keys),
            _ => return None,
        }
    }
}

/// Whether `node` is a path stage of exactly `.[$slot]`: a `Current`
/// stage whose single step is the dynamic index by `slot`.
fn single_dyn_var_path(nodes: &[ProgramNode], node: ProgramNodeId, slot: VarSlot) -> bool {
    match nodes.get(node.index()) {
        Some(ProgramNode::Stage {
            start: StageStart::Current,
            steps,
        }) => {
            steps.len() == 1
                && matches!(
                    steps[0].access(),
                    StepAccess::DynVar(bound) if *bound == slot
                )
        }
        _ => false,
    }
}

/// Marks binary shapes and static object keys in one arena pass.
fn mark_post_fuse_facts(nodes: &mut [ProgramNode]) {
    for index in 0..nodes.len() {
        if let ProgramNode::Binary { left, right, .. } = &nodes[index] {
            let shape = if !identity_stage(nodes, *left) {
                crate::program::BinaryShape::Framed
            } else if constant_operand(nodes, *right).is_some() {
                crate::program::BinaryShape::IdentityLiteral
            } else if let ProgramNode::Stage {
                start: StageStart::Variable(slot),
                steps,
            } = &nodes[right.index()]
                && steps.is_empty()
            {
                crate::program::BinaryShape::IdentitySlot(*slot)
            } else {
                crate::program::BinaryShape::Framed
            };
            if let ProgramNode::Binary { shape: slot, .. } = &mut nodes[index] {
                *slot = shape;
            }
        }
        if let ProgramNode::ConstructObject { members, .. } = &nodes[index] {
            let keys: alloc::vec::Vec<Option<alloc::string::String>> = members
                .iter()
                .map(|member| match constant_operand(nodes, member.key) {
                    Some(Value::String(text)) => Some(alloc::string::String::from(text.as_str())),
                    _ => None,
                })
                .collect();
            if let ProgramNode::ConstructObject { members, .. } = &mut nodes[index] {
                for (member, key) in members.iter_mut().zip(keys) {
                    member.static_key = key;
                }
            }
        }
    }
}

fn identity_stage(nodes: &[ProgramNode], node: ProgramNodeId) -> bool {
    matches!(
        &nodes[node.index()],
        ProgramNode::Stage {
            start: StageStart::Current,
            steps,
        } if steps.is_empty()
    )
}

/// The value a step-less `Literal` stage produces, read off the arena.
fn constant_operand(nodes: &[ProgramNode], node: ProgramNodeId) -> Option<&Value> {
    match &nodes[node.index()] {
        ProgramNode::Stage {
            start: StageStart::Literal(literal),
            steps,
        } if steps.is_empty() => Some(literal),
        _ => None,
    }
}
