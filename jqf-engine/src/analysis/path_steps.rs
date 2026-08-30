//! Shared static-path helpers for the count, element, and demand tables.

use alloc::vec::Vec;

use jqf_data::{CountStep, SliceRange};
use jqf_resource::ResourceError;

use crate::program::{ProgramNode, ProgramNodeId, SliceBounds, StageStart, StageStep, StepAccess};

/// The container-path range of one recognized row: the slice's bounds
/// normalized under the boundary law, admitted only when neither bound is
/// strictly negative (the document-side v1 admission law,
/// [`super::admits_document_range`] — this table's resolution is
/// `jqf-data`'s pure clamp, which has no len-relative wrap).
pub(crate) fn range_normalized(bounds: &SliceBounds) -> Option<SliceRange> {
    let range = bounds.try_normalize()?;
    super::admits_document_range(&range).then_some(range)
}

/// One contiguous static `Key`/`Index` step run as count steps, or `None`
/// when any step is not static `Key`/`Index` or is optional (`?`).
pub(crate) fn static_path(steps: &[StageStep]) -> Option<Vec<CountStep>> {
    let mut path = Vec::new();
    path.try_reserve_exact(steps.len()).ok()?;
    for step in steps {
        if step.is_optional() {
            return None;
        }
        match step.access() {
            StepAccess::Key(key) => path.push(CountStep::ObjectKey(key.clone())),
            StepAccess::Index(index) => path.push(CountStep::ArrayIndex(*index)),
            StepAccess::Each
            | StepAccess::DynVar(_)
            | StepAccess::Descend
            | StepAccess::Slice(_)
            | StepAccess::NodeAccessor(_)
            | StepAccess::Attribute(_)
            | StepAccess::DynNodeAccessor(_)
            | StepAccess::DynAttribute(_) => return None,
        }
    }
    Some(path)
}

/// A Current-start stage of non-optional Key/Index steps — the static prefix
/// of `PATH | map(f)` and `PATH | map(f) | length`. Empty is already a root
/// collect row.
pub(crate) fn static_member_stage_steps(nodes: &[ProgramNode], id: ProgramNodeId) -> Option<Vec<StageStep>> {
    let ProgramNode::Stage {
        start: StageStart::Current,
        steps,
    } = &nodes[id.index()]
    else {
        return None;
    };
    if steps.is_empty() {
        return None;
    }
    if steps
        .iter()
        .any(|step| !matches!(step.access(), StepAccess::Key(_) | StepAccess::Index(_)) || step.is_optional())
    {
        return None;
    }
    Some(steps.clone())
}

/// Prepends `prefix` onto the leading Current-start stage of `inner` (or of that
/// stage when `inner` is a `FlatMap`). `Ok(false)` when the shape is not that
/// pair; `Err` on reserve failure.
pub(crate) fn prepend_in_place(
    out: &mut [ProgramNode],
    inner: ProgramNodeId,
    prefix: &[StageStep],
) -> Result<bool, ResourceError> {
    let stage_id = match out.get(inner.index()) {
        Some(ProgramNode::Stage {
            start: StageStart::Current,
            ..
        }) => inner,
        Some(ProgramNode::FlatMap { upstream, .. }) => {
            let upstream = *upstream;
            match out.get(upstream.index()) {
                Some(ProgramNode::Stage {
                    start: StageStart::Current,
                    ..
                }) => upstream,
                _ => return Ok(false),
            }
        }
        _ => return Ok(false),
    };
    let ProgramNode::Stage { steps, .. } = &mut out[stage_id.index()] else {
        return Ok(false);
    };
    let mut combined = Vec::new();
    combined
        .try_reserve(prefix.len() + steps.len())
        .map_err(|_| ResourceError::AllocationFailed)?;
    combined.extend(prefix.iter().cloned());
    combined.extend(steps.iter().cloned());
    *steps = combined;
    Ok(true)
}

/// [`static_member_stage_steps`] as count steps.
pub(crate) fn static_member_prefix(nodes: &[ProgramNode], id: ProgramNodeId) -> Option<Vec<CountStep>> {
    static_path(&static_member_stage_steps(nodes, id)?)
}

/// True when a collect-hoist prefix is a non-empty run of non-optional Key/Index steps.
pub(crate) fn hoistable_collect_prefix(prefix: &[StageStep]) -> bool {
    !prefix.is_empty()
        && prefix
            .iter()
            .all(|step| matches!(step.access(), StepAccess::Key(_) | StepAccess::Index(_)) && !step.is_optional())
}

/// The number of `.[]` ([`StepAccess::Each`]) steps in the whole arena.
///
/// One is what licenses the classifier's exact per-boundary narrowing; more than
/// one is nested iteration, which keeps the conservative join.
pub(crate) fn count_each_steps(nodes: &[ProgramNode]) -> usize {
    nodes
        .iter()
        .map(|node| match node {
            ProgramNode::Stage { steps, .. } => steps
                .iter()
                .filter(|step| matches!(step.access(), StepAccess::Each))
                .count(),
            _ => 0,
        })
        .sum()
}
