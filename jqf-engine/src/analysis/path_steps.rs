//! Shared static-path helpers for the count, element, and demand tables.

use alloc::vec::Vec;

use jqf_data::{CountStep, SliceRange};

use crate::program::{ProgramNode, SliceBounds, StageStep, StepAccess};

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
