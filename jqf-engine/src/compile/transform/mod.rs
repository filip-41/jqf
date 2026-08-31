//! Post-lower transforms on the pre-fusion arena: fuse, rewrite, and mark
//! before analysis.

mod marks;
mod rewrites;

use super::error::EngineCompileError;
use super::prelude::*;
use crate::program::{CallableDef, ProgramNode, ProgramNodeId};

pub(crate) use rewrites::push_rewrite_node;

/// Fuses pipe chains, rewrites collect|add (root and callable bodies), and
/// marks post-fuse facts.
///
/// Order is fixed: fuse → `rewrite_collect_add` → marks. Analysis and
/// `Program::new` assume path-normal shapes with tail/keyed/binary facts set.
pub(crate) fn transform_lowered(
    nodes: &mut [ProgramNode],
    root: ProgramNodeId,
    slots: &mut u32,
    callables: &[CallableDef],
) -> Result<(Vec<ProgramNode>, ProgramNodeId), EngineCompileError> {
    let (mut nodes, root, fused_body_ids) =
        crate::analysis::fuse_with_callables(nodes, root, callables).map_err(EngineCompileError::Resource)?;
    let root = rewrites::rewrite_collect_add(&mut nodes, root, slots)?;
    rewrites::rewrite_collect_add_callables(&mut nodes, &fused_body_ids, slots)?;
    marks::mark_post_fuse(&mut nodes);
    Ok((nodes, root))
}
