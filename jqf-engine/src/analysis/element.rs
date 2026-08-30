//! The ELEMENT-ITERATION demand table: which programs are
//! element-iteration rows, and the container path + per-element probe that
//! answers them.
//!
//! This is the analysis half of the element-iteration consumer, the sibling
//! of [`super::count`]'s count table. The element-stream route kind was
//! removed; the fan-out/fold rows it served are recognized HERE and lowered
//! into a [`jqf_data::ElementDemand`] that the document-core consumer
//! answers by iterating the lazy document's span skeleton — the same
//! machinery the count consumer built, extended from counts to element
//! streams. It is a CLOSED table with a declining default:
//! a shape that is not a row below is not element-iteration-equivalent, full
//! stop — the caller's ordinary route reproduces the floor.
//!
//! # The rows (v1 — the shapes the rss lanes take)
//!
//! | shape | example | demand |
//! | --- | --- | --- |
//! | fan-out, static probe | `.catalog[] \| .name` | [`ElementRow::FanOut`], path `C`, probe `[name]` |
//! | fan-out, bare | `.catalog[]` | [`ElementRow::FanOut`], path `C`, empty probe |
//! | fan-out, length | `.[] \| length` | [`ElementRow::FanOut`], path `[]`, [`ElementProbe::Length`] |
//! | collected fan-out | `[.catalog[] \| .id]` / `.catalog \| map(.id)` | [`ElementRow::FanOut`], path `C`, probe `[id]`, collect |
//! | fold | `reduce (C[] \| .attrs.warehouse) as $x ({}; .[$x] += 1)` | [`ElementRow::ReduceFold`], path `C`, probe `[attrs, warehouse]` |
//! | counted prefix | `limit(k; C[] \| .id)` / `nth(k; C[] \| .id)` | [`ElementRow::FanOut`], path `C`, range `[0:k]` / `[k:k+1]` |
//! | first | `first(C[] \| .id)` | [`ElementRow::FanOut`], path `C`, range `[0:1]` |
//! | select fan-out | `.catalog[] \| select(.ok) \| .name` | [`ElementRow::FanOut`] with [`ElementDemand::filter`] |
//!
//! The fold row requires the update to be the OBJECT-INCREMENT shape —
//! `.[$x] += LITERAL` with a document-independent literal init — because the
//! consumer reproduces the fold without the evaluator: the per-element read
//! is the probe, and the increment is one closed arithmetic law (null → the
//! literal, an exact integer → plus the literal; everything else declines to
//! the floor's generic `+`). The row carries the increment so the consumer
//! does not re-derive it.
//!
//! Deliberate v1 declines (the floor serves them byte for byte): optional
//! (`?`) probe steps, nested iteration (more than one `.[]`), a residual
//! call beyond `select`/`length`, a non-literal fold init,
//! a fold update that is not the increment row, `foreach`, a non-literal
//! `limit`/`nth` count, `skip` (it still walks the tail), `first`/`limit`
//! whose generator is not a static-probe fan-out (`select` in the residual
//! is first-k-matches, not a prefix range). Collected construct
//! (`[C[] | {id}]`, `PATH | map({id})`) is a row: the SDK publishes one array
//! of reconstructed objects. Each decline is a missed win, never a wrong answer.

use alloc::vec::Vec;

use jqf_data::{CountStep, ElementDemand, ElementProbe, ElementRow, SliceRange};

use crate::analysis::path_steps::{count_each_steps, range_normalized, static_path};
use crate::program::{
    BinaryKind, CountedKind, ModifyMode, ObjectMemberNode, ProgramNode, ProgramNodeId, StageStart, StageStep,
    StepAccess, VarSlot,
};

/// The per-element static members of a `PATH[] | {static keys}` fan-out.
/// Each pair is the constructor key and the static path its value reads
/// from the element. Empty is never a row — a projection that names
/// nothing is structure-only.
pub(crate) type ConstructFields = Vec<(alloc::string::String, Vec<CountStep>)>;

/// The recognized element-iteration plan of one compiled program, or `None`
/// when the program is not an element-iteration row.
pub(crate) struct ElementPlan {
    /// The document-core demand the consumer iterates.
    pub demand: ElementDemand,
    /// Static-key object construction riding a fan-out, when the body is
    /// `{static: .static_path, …}`. `None` for path/length/fold rows.
    pub construct: Option<ConstructFields>,
    /// The program is `[FAN-OUT]`: the SDK collects every visited probe
    /// value into one array and publishes that array as the sole item.
    /// `false` for the streaming fan-out and for the fold row.
    pub collect: bool,
}

/// The recognized element-iteration demand of one compiled program, or `None`
/// when the program is not an element-iteration row.
pub(crate) fn element_plan(
    nodes: &[ProgramNode],
    root: ProgramNodeId,
    split: crate::analysis::PushdownSplit,
) -> Option<ElementPlan> {
    fan_out_row(nodes, root, split)
        .or_else(|| select_fan_out_row(nodes, root))
        .or_else(|| reduce_fold_row(nodes, root))
        .or_else(|| counted_fan_out_row(nodes, root))
}

/// The fan-out row: the root is either ONE fused [`ProgramNode::Stage`]
/// holding the boundary `.[]` and the static probe, or a
/// [`ProgramNode::FlatMap`] whose upstream stage ends at the boundary and
/// whose body is the probe (a static path or a bare `length` call — the
/// `.[] | length` shape).
///
/// A root [`ProgramNode::CollectArray`] wrapping either shape is the same
/// row with [`ElementPlan::collect`]: the SDK publishes one array. Collected
/// construct (`[C[] | {id}]`, `PATH | map({id, name})`) is that row too.
fn fan_out_row(
    nodes: &[ProgramNode],
    root: ProgramNodeId,
    split: crate::analysis::PushdownSplit,
) -> Option<ElementPlan> {
    // The boundary `.[]` must be the program's SOLE one (nested iteration's
    // per-element cardinality is not one output per element).
    if count_each_steps(nodes) != 1 {
        return None;
    }
    // `[FAN-OUT]` is the streaming row's publication twin: unwrap one collect
    // the way `unwrap_counted_or_first` does, then match Stage/FlatMap.
    let (inner, collect) = match &nodes[root.index()] {
        ProgramNode::CollectArray { body: Some(body) } => (*body, true),
        _ => (root, false),
    };
    match &nodes[inner.index()] {
        ProgramNode::Stage {
            start: StageStart::Current,
            steps,
        } => {
            // The fused spelling `.catalog[] | .name` is ONE stage; the split
            // names the boundary: `prefix_len` is the `.[]` offset in the
            // entry stage (or a `[a:b]` IMMEDIATELY before it — the
            // container-path range), the steps before it the
            // container path, the steps after it the per-element probe.
            // A collect wrapper is transparent to pushdown (`analyze_pushdown`
            // descends it), so the entry is the inner stage, not the collect.
            let entry = split.entry();
            if entry != inner {
                return None;
            }
            let prefix_len = split.prefix_len();
            let (boundary, range) = match steps.get(prefix_len).map(StageStep::access) {
                Some(StepAccess::Slice(_)) => {
                    if steps[prefix_len].is_optional() {
                        return None;
                    }
                    let each = steps.get(prefix_len + 1)?;
                    if !matches!(each.access(), StepAccess::Each) || each.is_optional() {
                        return None;
                    }
                    let StepAccess::Slice(bounds) = steps[prefix_len].access() else {
                        unreachable!("the match arm named a slice")
                    };
                    (prefix_len + 1, Some(range_normalized(bounds)?))
                }
                Some(StepAccess::Each) if !steps[prefix_len].is_optional() => (prefix_len, None),
                _ => return None,
            };
            let path = static_path(&steps[..prefix_len])?;
            let probe = ElementProbe::Path(static_path(&steps[boundary + 1..])?);
            Some(fan_out_plan(path, range, probe, None, collect))
        }
        ProgramNode::FlatMap { upstream, body } => {
            // The `.[] | length` spelling: the upstream stage ends at the
            // boundary, the body is the per-element probe (a static path, or
            // a bare `length` call over an EMPTY trailing probe — a probe
            // followed by `| length` (`.catalog[].name | length`) reads the
            // probe value's LENGTH, a combined read this v1 probe cannot
            // express, so it declines to the floor).
            let ProgramNode::Stage {
                start: StageStart::Current,
                steps,
            } = &nodes[upstream.index()]
            else {
                return None;
            };
            let boundary = steps
                .iter()
                .position(|step| matches!(step.access(), StepAccess::Each))?;
            if steps[boundary].is_optional()
                || steps[boundary + 1..]
                    .iter()
                    .any(|step| matches!(step.access(), StepAccess::Each))
            {
                return None;
            }
            let (path, range) = container_path_and_range(steps, boundary)?;
            let trailing = static_path(&steps[boundary + 1..])?;
            let (probe, construct) = match &nodes[body.index()] {
                ProgramNode::Stage {
                    start: StageStart::Current,
                    steps,
                } => {
                    // The body's steps follow the upstream's trailing steps:
                    // one concatenated probe.
                    let mut probe_steps = trailing;
                    probe_steps.extend(static_path(steps)?);
                    (ElementProbe::Path(probe_steps), None)
                }
                ProgramNode::Call { overload, args, .. }
                    if args.is_empty() && overload.get() == jqf_builtins::registry::builtins::id::LENGTH =>
                {
                    if !trailing.is_empty() {
                        return None;
                    }
                    (ElementProbe::Length, None)
                }
                _ => construct_plan(nodes, *body, &trailing)?,
            };
            Some(fan_out_plan(path, range, probe, construct, collect))
        }
        _ => None,
    }
}

/// The reduce-fold row: a `reduce` whose SOURCE holds the boundary and whose
/// per-element read is a static probe, with a document-independent literal
/// init and the object-increment update.
fn reduce_fold_row(nodes: &[ProgramNode], root: ProgramNodeId) -> Option<ElementPlan> {
    let ProgramNode::Reduce {
        source,
        slot,
        init,
        update,
        keyed_collect: _,
    } = &nodes[root.index()]
    else {
        return None;
    };
    // The boundary `.[]` must be the program's SOLE one, inside the source.
    if count_each_steps(nodes) != 1 {
        return None;
    }
    let (path, probe) = boundary_path_and_probe(nodes, *source)?;
    // A document-independent init: a bare literal stage.
    let ProgramNode::Stage {
        start: StageStart::Literal(_),
        steps,
    } = &nodes[init.index()]
    else {
        return None;
    };
    if !steps.is_empty() {
        return None;
    }
    let increment = increment_update(nodes, *update, *slot)?;
    Some(ElementPlan {
        demand: ElementDemand {
            row: ElementRow::ReduceFold,
            path,
            range: None,
            probe,
            increment: Some(increment),
            filter: None,
        },
        construct: None,
        collect: false,
    })
}

/// `limit(k; C[] | PROBE)`, `nth(k; C[] | PROBE)`, and `first(C[] | PROBE)`
/// as the existing fan-out row with a prefix/singleton range.
///
/// The `ElementStream` access kind is gone; Counted/Label still name no
/// element boundary on the spine. The legal mechanism against today's
/// two-access-kind floor is the document-core fan-out consumer already
/// walking a deferred array span under `demand.range` — `limit(10; C[] | .id)`
/// is the same in-range walk `.catalog[0:10][] | .id` takes. A `select` in
/// the residual is first-k-matches, not a prefix, and declines (the floor's
/// Counted cut still truncates, but the catalog is materialized first).
///
/// Soundness: `limit`/`nth` cut the source the same way a slice cuts the
/// container; the codec already validated the whole input; remaining
/// elements are not evaluated by the floor either. `first` is `limit(1)`
/// of the same generator. A collect wrapper (`[limit(k; …)]`) is the same
/// row with [`ElementPlan::collect`]: the SDK publishes one array.
fn counted_fan_out_row(nodes: &[ProgramNode], root: ProgramNodeId) -> Option<ElementPlan> {
    if count_each_steps(nodes) != 1 {
        return None;
    }
    let (inner, range, collect) = unwrap_counted_or_first(nodes, root)?;
    let (path, source_range, probe, construct) = fan_out_shape(nodes, inner)?;
    // A slice already on the generator (`limit(k; C[a:b][] | .id)`) is a
    // composed cut the v1 range cannot join. Decline; the floor serves it.
    if source_range.is_some() {
        return None;
    }
    Some(fan_out_plan(path, Some(range), probe, construct, collect))
}

fn fan_out_plan(
    path: Vec<CountStep>,
    range: Option<SliceRange>,
    probe: ElementProbe,
    construct: Option<ConstructFields>,
    collect: bool,
) -> ElementPlan {
    fan_out_plan_filter(path, range, probe, construct, collect, None)
}

fn fan_out_plan_filter(
    path: Vec<CountStep>,
    range: Option<SliceRange>,
    probe: ElementProbe,
    construct: Option<ConstructFields>,
    collect: bool,
    filter: Option<jqf_data::CountFilter>,
) -> ElementPlan {
    ElementPlan {
        demand: ElementDemand {
            row: ElementRow::FanOut,
            path,
            range,
            probe,
            increment: None,
            filter,
        },
        construct,
        collect,
    }
}

/// `.catalog[] | select(P) | .name` and `.catalog[] | select(P)`: a sole `.[]`
/// whose residual is the closed collect-filter predicate, optionally followed
/// by a static probe. First-k-matches (`limit`/`first` wrapping select) stay
/// declined — those are not a prefix range. The count twin
/// `PATH | map(select(P)) | length` already lives on the count table.
fn select_fan_out_row(nodes: &[ProgramNode], root: ProgramNodeId) -> Option<ElementPlan> {
    if count_each_steps(nodes) != 1 {
        return None;
    }
    let (inner, collect) = match &nodes[root.index()] {
        ProgramNode::CollectArray { body: Some(body) } => (*body, true),
        _ => (root, false),
    };
    // Two-pipe spellings. Pipe is right-associative, so
    // `.catalog[] | select(P) | .name` is FlatMap { stage, FlatMap { select, probe } }.
    // The left-associated twin is FlatMap { FlatMap { stage, select }, probe }.
    if let ProgramNode::FlatMap { upstream, body } = &nodes[inner.index()] {
        if let ProgramNode::FlatMap {
            upstream: select_id,
            body: probe_id,
        } = &nodes[body.index()]
        {
            let filter = select_call_filter(nodes, *select_id)?;
            let (path, range) = stage_each_path(nodes, *upstream)?;
            let probe = probe_from_node(nodes, *probe_id)?;
            return Some(fan_out_plan_filter(path, range, probe, None, collect, Some(filter)));
        }
        if let ProgramNode::FlatMap {
            upstream: stage_id,
            body: select_body,
        } = &nodes[upstream.index()]
        {
            let filter = select_call_filter(nodes, *select_body)?;
            let (path, range) = stage_each_path(nodes, *stage_id)?;
            let probe = probe_from_node(nodes, *body)?;
            return Some(fan_out_plan_filter(path, range, probe, None, collect, Some(filter)));
        }
    }
    // One-pipe spelling: FlatMap { stage-with-Each, select } — emit the element.
    let ProgramNode::FlatMap { upstream, body } = &nodes[inner.index()] else {
        return None;
    };
    let filter = select_call_filter(nodes, *body)?;
    let (path, range) = stage_each_path(nodes, *upstream)?;
    Some(fan_out_plan_filter(
        path,
        range,
        ElementProbe::Path(Vec::new()),
        None,
        collect,
        Some(filter),
    ))
}

fn probe_from_node(nodes: &[ProgramNode], id: ProgramNodeId) -> Option<ElementProbe> {
    match &nodes[id.index()] {
        ProgramNode::Stage {
            start: StageStart::Current,
            steps,
        } => Some(ElementProbe::Path(static_path(steps)?)),
        ProgramNode::Call { overload, args, .. }
            if args.is_empty() && overload.get() == jqf_builtins::registry::builtins::id::LENGTH =>
        {
            Some(ElementProbe::Length)
        }
        _ => None,
    }
}

fn select_call_filter(nodes: &[ProgramNode], id: ProgramNodeId) -> Option<jqf_data::CountFilter> {
    let ProgramNode::Call { overload, args, .. } = &nodes[id.index()] else {
        return None;
    };
    if overload.get() != jqf_builtins::registry::builtins::id::SELECT || args.len() != 1 {
        return None;
    }
    let (test, key) = crate::analysis::count::admitted_predicate(nodes, args[0])?;
    Some(jqf_data::CountFilter {
        path: alloc::vec![jqf_data::CountStep::ObjectKey(key)],
        test,
    })
}

fn stage_each_path(nodes: &[ProgramNode], id: ProgramNodeId) -> Option<(Vec<CountStep>, Option<SliceRange>)> {
    let ProgramNode::Stage {
        start: StageStart::Current,
        steps,
    } = &nodes[id.index()]
    else {
        return None;
    };
    let boundary = steps
        .iter()
        .position(|step| matches!(step.access(), StepAccess::Each))?;
    if steps[boundary].is_optional() {
        return None;
    }
    // Residual steps after Each on the same stage are a fused probe; this row
    // wants select as a separate Call.
    if !steps[boundary + 1..].is_empty() {
        return None;
    }
    container_path_and_range(steps, boundary)
}

/// Unwrap `[…]`, then the `limit`/`nth` Bind+Conditional or the `first`
/// Label cap, to the generator graph plus the in-range cut.
fn unwrap_counted_or_first(nodes: &[ProgramNode], root: ProgramNodeId) -> Option<(ProgramNodeId, SliceRange, bool)> {
    let (id, collect) = match &nodes[root.index()] {
        ProgramNode::CollectArray { body: Some(body) } => (*body, true),
        _ => (root, false),
    };
    if let Some((inner, range)) = first_cap(nodes, id) {
        return Some((inner, range, collect));
    }
    let (inner, kind, n) = counted_from_bind(nodes, id)?;
    let range = match kind {
        CountedKind::Limit if n > 0 => (Some(0), Some(n)),
        CountedKind::Nth if n >= 0 => (Some(n), Some(n.checked_add(1)?)),
        _ => return None,
    };
    crate::analysis::admits_document_range(&range).then_some((inner, range, collect))
}

/// `label $o | GEN | (., break $o)` — `first/1`'s cap. The generator is
/// the `FlatMap` upstream; the body is identity-then-break.
fn first_cap(nodes: &[ProgramNode], id: ProgramNodeId) -> Option<(ProgramNodeId, SliceRange)> {
    let ProgramNode::Label { body, slot } = &nodes[id.index()] else {
        return None;
    };
    let ProgramNode::FlatMap { upstream, body: cap } = &nodes[body.index()] else {
        return None;
    };
    let ProgramNode::Choice { left, right } = &nodes[cap.index()] else {
        return None;
    };
    let ProgramNode::Stage {
        start: StageStart::Current,
        steps,
    } = &nodes[left.index()]
    else {
        return None;
    };
    if !steps.is_empty() {
        return None;
    }
    let ProgramNode::Break { slot: break_slot } = &nodes[right.index()] else {
        return None;
    };
    if break_slot != slot {
        return None;
    }
    Some((*upstream, (Some(0), Some(1))))
}

/// `limit`/`nth` lower to `Bind { Literal n, Conditional { … Counted } }`.
/// `limit`'s Counted is the `> 0` consequent; `nth`'s is the non-negative
/// alternative. A non-literal count declines — the range would not be a
/// compile-time cut.
fn counted_from_bind(nodes: &[ProgramNode], id: ProgramNodeId) -> Option<(ProgramNodeId, CountedKind, i64)> {
    let ProgramNode::Bind { source, slot, body, .. } = &nodes[id.index()] else {
        return None;
    };
    let n = literal_i64(nodes, *source)?;
    let counted = match &nodes[body.index()] {
        ProgramNode::Counted { .. } => *body,
        ProgramNode::Conditional {
            consequent,
            alternative,
            ..
        } => {
            if matches!(&nodes[consequent.index()], ProgramNode::Counted { .. }) {
                *consequent
            } else if matches!(&nodes[alternative.index()], ProgramNode::Counted { .. }) {
                *alternative
            } else {
                return None;
            }
        }
        _ => return None,
    };
    let ProgramNode::Counted {
        source: inner,
        count,
        kind,
        ..
    } = &nodes[counted.index()]
    else {
        return None;
    };
    if count != slot {
        return None;
    }
    Some((*inner, *kind, n))
}

fn literal_i64(nodes: &[ProgramNode], id: ProgramNodeId) -> Option<i64> {
    let ProgramNode::Stage {
        start: StageStart::Literal(jqf_data::Value::Number(number)),
        steps,
    } = &nodes[id.index()]
    else {
        return None;
    };
    if !steps.is_empty() {
        return None;
    }
    number.to_integer().and_then(|i| i.to_i64())
}

/// One fan-out row: the element path, its optional range cap, the probe, and
/// the static constructor fields a projected publish would rebuild.
type FanOutShape = (
    Vec<CountStep>,
    Option<SliceRange>,
    ElementProbe,
    Option<ConstructFields>,
);

/// Fan-out path/probe of one nested generator, without the program-root
/// pushdown split (the counted/`first` wrappers make that split the Bind
/// or Label, not the `.[]`).
fn fan_out_shape(nodes: &[ProgramNode], id: ProgramNodeId) -> Option<FanOutShape> {
    match &nodes[id.index()] {
        ProgramNode::Stage {
            start: StageStart::Current,
            steps,
        } => {
            let boundary = steps
                .iter()
                .position(|step| matches!(step.access(), StepAccess::Each))?;
            if steps[boundary].is_optional()
                || steps[boundary + 1..]
                    .iter()
                    .any(|step| matches!(step.access(), StepAccess::Each))
            {
                return None;
            }
            let (path, range) = container_path_and_range(steps, boundary)?;
            let probe = ElementProbe::Path(static_path(&steps[boundary + 1..])?);
            Some((path, range, probe, None))
        }
        ProgramNode::FlatMap { upstream, body } => {
            let ProgramNode::Stage {
                start: StageStart::Current,
                steps,
            } = &nodes[upstream.index()]
            else {
                return None;
            };
            let boundary = steps
                .iter()
                .position(|step| matches!(step.access(), StepAccess::Each))?;
            if steps[boundary].is_optional()
                || steps[boundary + 1..]
                    .iter()
                    .any(|step| matches!(step.access(), StepAccess::Each))
            {
                return None;
            }
            let (path, range) = container_path_and_range(steps, boundary)?;
            let trailing = static_path(&steps[boundary + 1..])?;
            let (probe, construct) = match &nodes[body.index()] {
                ProgramNode::Stage {
                    start: StageStart::Current,
                    steps,
                } => {
                    let mut probe_steps = trailing;
                    probe_steps.extend(static_path(steps)?);
                    (ElementProbe::Path(probe_steps), None)
                }
                ProgramNode::Call { overload, args, .. }
                    if args.is_empty() && overload.get() == jqf_builtins::registry::builtins::id::LENGTH =>
                {
                    if !trailing.is_empty() {
                        return None;
                    }
                    (ElementProbe::Length, None)
                }
                _ => construct_plan(nodes, *body, &trailing)?,
            };
            Some((path, range, probe, construct))
        }
        _ => None,
    }
}

/// `{static keys}` as a fan-out body, including after construct fusion hoists a
/// shared member prefix (`{id: .id}` → `.id | {id: .}`). Empty field paths
/// (the whole element) stay declined unless a hoisted prefix supplies them.
fn construct_plan(
    nodes: &[ProgramNode],
    body: ProgramNodeId,
    trailing: &[CountStep],
) -> Option<(ElementProbe, Option<ConstructFields>)> {
    if !trailing.is_empty() {
        return None;
    }
    let fields = construct_fields_from_body(nodes, body)?;
    Some((ElementProbe::Path(Vec::new()), Some(fields)))
}

fn construct_fields_from_body(nodes: &[ProgramNode], mut body: ProgramNodeId) -> Option<ConstructFields> {
    let mut prefix = Vec::new();
    loop {
        match &nodes[body.index()] {
            ProgramNode::ConstructObject { members } => {
                let mut fields = static_construct_fields(nodes, members)?;
                for (_, path) in &mut fields {
                    let mut full = prefix.clone();
                    full.extend(path.iter().cloned());
                    *path = full;
                    if path.is_empty() {
                        return None;
                    }
                }
                return Some(fields);
            }
            ProgramNode::FlatMap { upstream, body: inner } => {
                let ProgramNode::Stage {
                    start: StageStart::Current,
                    steps,
                } = &nodes[upstream.index()]
                else {
                    return None;
                };
                prefix.extend(static_path(steps)?);
                body = *inner;
            }
            _ => return None,
        }
    }
}

/// Static-key object construction whose every value is a static field path
/// over the element. Declines on a dynamic key, an optional (`?`) step, or a
/// value graph that is not one Current-start stage of Key/Index steps. An
/// empty value path (identity after a construct-prefix hoist) is allowed
/// here; [`construct_fields_from_body`] declines if the path is still empty
/// after prepending the hoist.
fn static_construct_fields(nodes: &[ProgramNode], members: &[ObjectMemberNode]) -> Option<ConstructFields> {
    if members.is_empty() {
        return None;
    }
    let mut fields = Vec::new();
    fields.try_reserve_exact(members.len()).ok()?;
    for member in members {
        let key = member.static_key.as_ref()?.clone();
        let ProgramNode::Stage {
            start: StageStart::Current,
            steps,
        } = &nodes[member.value.index()]
        else {
            return None;
        };
        let path = static_path(steps)?;
        fields.push((key, path));
    }
    Some(fields)
}

/// The boundary path + per-element probe of one source graph (a fused stage
/// with a trailing `.[]`).
fn boundary_path_and_probe(nodes: &[ProgramNode], source: ProgramNodeId) -> Option<(Vec<CountStep>, ElementProbe)> {
    let ProgramNode::Stage {
        start: StageStart::Current,
        steps,
    } = &nodes[source.index()]
    else {
        return None;
    };
    let boundary = steps
        .iter()
        .position(|step| matches!(step.access(), StepAccess::Each))?;
    let boundary_step = &steps[boundary];
    if boundary_step.is_optional() {
        return None;
    }
    // No step after the boundary may iterate or descend (a nested `.[]`, a
    // `..` — the per-element cardinality would not be one).
    if steps[boundary + 1..]
        .iter()
        .any(|step| matches!(step.access(), StepAccess::Each | StepAccess::Descend))
    {
        return None;
    }
    let path = static_path(&steps[..boundary])?;
    let probe = ElementProbe::Path(static_path(&steps[boundary + 1..])?);
    Some((path, probe))
}

/// The object-increment update row: `Bind($rhs = LITERAL, Modify { paths:
/// .[$slot], update: (. + $rhs) capped by the label/break first-output shape,
/// mode: Update })` — the compiled shape of `.[$slot] += LITERAL`. Returns
/// the increment's exact integer.
fn increment_update(nodes: &[ProgramNode], update: ProgramNodeId, slot: VarSlot) -> Option<i64> {
    // `+=` binds its right-hand side outside the Modify.
    let ProgramNode::Bind {
        source,
        slot: rhs_slot,
        body,
        frame: _,
    } = &nodes[update.index()]
    else {
        return None;
    };
    let ProgramNode::Stage {
        start: StageStart::Literal(jqf_data::Value::Number(number)),
        steps,
    } = &nodes[source.index()]
    else {
        return None;
    };
    if !steps.is_empty() {
        return None;
    }
    let increment = number.to_integer().and_then(|i| i.to_i64())?;
    let ProgramNode::Modify {
        paths,
        update: cap,
        mode: ModifyMode::Update,
    } = &nodes[body.index()]
    else {
        return None;
    };
    // The path is the dynamic member read of the reduce slot.
    let ProgramNode::Stage {
        start: StageStart::Current,
        steps,
    } = &nodes[paths.index()]
    else {
        return None;
    };
    if !(steps.len() == 1 && matches!(steps[0].access(), StepAccess::DynVar(step_slot) if *step_slot == slot)) {
        return None;
    }
    // The at-most-one-output cap wraps the update value.
    let ProgramNode::Label { body: flat, .. } = &nodes[cap.index()] else {
        return None;
    };
    let ProgramNode::FlatMap { upstream, .. } = &nodes[flat.index()] else {
        return None;
    };
    // `old + $rhs`.
    let ProgramNode::Binary {
        op: BinaryKind::Add,
        left,
        right,
        ..
    } = &nodes[upstream.index()]
    else {
        return None;
    };
    let ProgramNode::Stage {
        start: StageStart::Current,
        steps,
    } = &nodes[left.index()]
    else {
        return None;
    };
    if !steps.is_empty() {
        return None;
    }
    let ProgramNode::Stage {
        start: StageStart::Variable(rhs),
        steps,
    } = &nodes[right.index()]
    else {
        return None;
    };
    if *rhs != *rhs_slot || !steps.is_empty() {
        return None;
    }
    Some(increment)
}

/// The container path + normalized range of a boundary stage:
/// `steps` holds the container steps before the boundary `.[]` at `boundary`,
/// with at most ONE trailing `[a:b]` range IMMEDIATELY before it. A
/// non-normalizing slice, an optional slice, or a slice not immediately
/// before the boundary DECLINES — the floor serves the shape.
fn container_path_and_range(steps: &[StageStep], boundary: usize) -> Option<(Vec<CountStep>, Option<SliceRange>)> {
    let (path_steps, range) = match steps[..boundary].split_last() {
        Some((last, prefix)) if matches!(last.access(), StepAccess::Slice(_)) => {
            if last.is_optional() {
                return None;
            }
            let StepAccess::Slice(bounds) = last.access() else {
                unreachable!("the split_last arm named a slice")
            };
            (prefix, Some(range_normalized(bounds)?))
        }
        _ => (&steps[..boundary], None),
    };
    Some((static_path(path_steps)?, range))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec_requirement::CodecRequirementPolicy;
    use crate::compile::try_compile_program;
    use alloc::vec;
    use jqf_codec_core::{DiagnosticPolicy, ValidationMode};
    use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

    static CONTROL: ContinueControl = ContinueControl;

    fn resources() -> ResourceContext<'static> {
        ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                .expect("account"),
            &CONTROL,
            WorkMeter::try_new_v1(4096).expect("work"),
        )
        .expect("resources")
    }

    fn demand(source: &str) -> Option<ElementDemand> {
        let resources = resources();
        let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
        let program = try_compile_program(source, policy, &resources).expect("compiles");
        program.element_demand().cloned()
    }

    fn compiled_fields(source: &str) -> Option<Vec<(alloc::string::String, Vec<CountStep>)>> {
        let resources = resources();
        let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
        let program = try_compile_program(source, policy, &resources).expect("compiles");
        program
            .element_construct_fields()
            .map(<[(alloc::string::String, alloc::vec::Vec<jqf_data::CountStep>)]>::to_vec)
    }

    fn key(name: &str) -> CountStep {
        CountStep::ObjectKey(alloc::string::String::from(name))
    }

    #[test]
    fn fan_out_rows_recognize_path_and_probe() {
        let d = demand(".catalog[] | .name").expect("fan-out row");
        assert_eq!(d.row, ElementRow::FanOut);
        assert_eq!(d.path, vec![key("catalog")]);
        assert_eq!(d.probe, ElementProbe::Path(vec![key("name")]));

        let d = demand(".catalog[]").expect("bare fan-out row");
        assert_eq!(d.path, vec![key("catalog")]);
        assert_eq!(d.probe, ElementProbe::Path(Vec::new()));

        let d = demand(".[] | length").expect("length fan-out row");
        assert_eq!(d.row, ElementRow::FanOut);
        assert!(d.path.is_empty());
        assert_eq!(d.probe, ElementProbe::Length);

        let d = demand(".catalog[] | {id,name,sku}").expect("construct fan-out row");
        assert_eq!(d.row, ElementRow::FanOut);
        assert_eq!(d.path, vec![key("catalog")]);
        assert_eq!(d.probe, ElementProbe::Path(Vec::new()));
        let fields = compiled_fields(".catalog[] | {id,name,sku}").expect("construct fields");
        assert_eq!(
            fields,
            vec![
                (alloc::string::String::from("id"), vec![key("id")]),
                (alloc::string::String::from("name"), vec![key("name")]),
                (alloc::string::String::from("sku"), vec![key("sku")]),
            ]
        );
    }

    #[test]
    fn fold_row_recognizes_the_increment_shape() {
        let d = demand("reduce (.catalog[] | .attrs.warehouse) as $w ({}; .[$w] += 1)").expect("fold row");
        assert_eq!(d.row, ElementRow::ReduceFold);
        assert_eq!(d.path, vec![key("catalog")]);
        assert_eq!(d.probe, ElementProbe::Path(vec![key("attrs"), key("warehouse")]));
        assert_eq!(d.increment, Some(1));
    }

    #[test]
    fn range_rows_recognize_the_container_path_range() {
        // `.catalog[10:20][] | .name`: the fused stage's boundary `.[]` is
        // preceded by a trailing `[a:b]` — the range rides the demand and the
        // consumer iterates only the in-range elements.
        let d = demand(".catalog[10:20][] | .name").expect("range fan-out row");
        assert_eq!(d.row, ElementRow::FanOut);
        assert_eq!(d.path, vec![key("catalog")]);
        assert_eq!(d.range, Some((Some(10), Some(20))));
        assert_eq!(d.probe, ElementProbe::Path(vec![key("name")]));

        // An open end bound rides as `None`.
        let d = demand(".catalog[5:][] | .name").expect("open-end range row");
        assert_eq!(d.range, Some((Some(5), None)));

        // The `.[] | length` spelling with a range.
        let d = demand(".catalog[10:20][] | length").expect("range length row");
        assert_eq!(d.range, Some((Some(10), Some(20))));
        assert_eq!(d.probe, ElementProbe::Length);

        // A slice NOT immediately before the `.[]` (a mid-path slice) declines.
        assert!(demand(".catalog[10:20] | .items[] | .name").is_none());
        // An optional slice declines.
        assert!(demand(".catalog[10:20]?[] | .name").is_none());
    }

    fn collect(source: &str) -> bool {
        let resources = resources();
        let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
        let program = try_compile_program(source, policy, &resources).expect("compiles");
        program.element_collect()
    }

    #[test]
    fn counted_limit_is_a_prefix_fan_out() {
        let d = demand("limit(10; .catalog[] | .id)").expect("limit row");
        assert_eq!(d.row, ElementRow::FanOut);
        assert_eq!(d.path, vec![key("catalog")]);
        assert_eq!(d.range, Some((Some(0), Some(10))));
        assert_eq!(d.probe, ElementProbe::Path(vec![key("id")]));
        assert!(!collect("limit(10; .catalog[] | .id)"));

        let d = demand("[limit(10; .catalog[] | .id)]").expect("collected limit row");
        assert_eq!(d.range, Some((Some(0), Some(10))));
        assert!(collect("[limit(10; .catalog[] | .id)]"));

        // A select in the residual is first-k-matches, not a prefix.
        assert!(demand("limit(10; .catalog[] | select(.stock > 30) | .id)").is_none());
        // A zero count takes the Empty arm, not Counted.
        assert!(demand("limit(0; .catalog[] | .id)").is_none());
        // A non-literal count is not a compile-time range.
        assert!(demand("limit(1 + 1; .catalog[] | .id)").is_none());
    }

    #[test]
    fn collect_of_a_static_probe_fan_out_is_the_element_walk() {
        let streaming = demand(".catalog[] | .id").expect("streaming fan-out");
        let collected = demand("[.catalog[] | .id]").expect("collected fan-out");
        assert_eq!(collected, streaming);
        assert!(collect("[.catalog[] | .id]"));
        assert!(!collect(".catalog[] | .id"));
        let constructed = demand("[.catalog[] | {id}]").expect("collected construct");
        assert_eq!(constructed.path, vec![key("catalog")]);
        assert!(collect("[.catalog[] | {id}]"));
        let fields = compiled_fields("[.catalog[] | {id}]").expect("construct fields");
        assert_eq!(fields, vec![(alloc::string::String::from("id"), vec![key("id")])]);
    }

    #[test]
    fn prefix_piped_map_is_the_collected_fan_out() {
        let mapped = demand(".catalog | map(.name)").expect("PATH|map is a collected fan-out");
        let collected = demand("[.catalog[] | .name]").expect("collected fan-out");
        assert_eq!(mapped, collected);
        assert!(collect(".catalog | map(.name)"));
        assert_eq!(mapped.path, vec![key("catalog")]);
        assert_eq!(mapped.probe, ElementProbe::Path(vec![key("name")]));
        assert!(mapped.range.is_none());

        // Nested prefix.
        let d = demand(".users | map(.profile.department)").expect("nested probe");
        assert_eq!(d.path, vec![key("users")]);
        assert_eq!(d.probe, ElementProbe::Path(vec![key("profile"), key("department")]));

        let mapped_obj = demand(".catalog | map({id, name})").expect("PATH|map construct");
        assert_eq!(mapped_obj.path, vec![key("catalog")]);
        assert!(collect(".catalog | map({id, name})"));
        let fields = compiled_fields(".catalog | map({id, name})").expect("map construct fields");
        assert_eq!(fields.len(), 2);
        let filtered_map = demand(".catalog | map(select(.id > 1))").expect("map(select) collect");
        assert!(filtered_map.filter.is_some());
        assert_eq!(filtered_map.path, vec![key("catalog")]);
        assert!(demand(".catalog | map(.items[])").is_none());
        assert!(demand(".catalog? | map(.name)").is_none());
    }

    #[test]
    fn counted_nth_is_a_singleton_range() {
        let d = demand("nth(10; .catalog[] | .id)").expect("nth row");
        assert_eq!(d.row, ElementRow::FanOut);
        assert_eq!(d.range, Some((Some(10), Some(11))));
        assert_eq!(d.probe, ElementProbe::Path(vec![key("id")]));
    }

    #[test]
    fn first_of_a_static_probe_is_a_k1_fan_out() {
        let d = demand("first(.catalog[] | .id)").expect("first row");
        assert_eq!(d.row, ElementRow::FanOut);
        assert_eq!(d.path, vec![key("catalog")]);
        assert_eq!(d.range, Some((Some(0), Some(1))));
        assert_eq!(d.probe, ElementProbe::Path(vec![key("id")]));
        assert!(demand("first(.catalog[] | select(.stock > 30))").is_none());
    }

    #[test]
    fn non_element_shapes_decline() {
        // A residual with a call beyond length.
        assert!(demand(".catalog[] | keys").is_none());
        // Nested iteration.
        assert!(demand(".catalog[] | .items[]").is_none());
        // An optional probe step.
        assert!(demand(".catalog[] | .name?").is_none());
        // A non-literal fold init.
        assert!(demand("reduce (.catalog[] | .attrs) as $w (.x; .[$w] += 1)").is_none());
        // A fold update that is not the increment row.
        assert!(demand("reduce (.catalog[] | .attrs) as $w ({}; .[$w] = 1)").is_none());
        // foreach publishes per iteration; not a row.
        assert!(demand("foreach .catalog[] as $x (0; . + 1)").is_none());
        // skip is not a prefix cut: the dropped head still has to be
        // produced, so the floor serves it.
        assert!(demand("skip(2; .catalog[] | .id)").is_none());
        // Nested `.[]` is not one output per outer element.
        assert!(demand(".catalog[] | .items[] | .name").is_none());
        assert!(demand("[.catalog[] | .tags[]]").is_none());
        // select residuals that are not the closed predicate.
        assert!(demand(".catalog[] | select(.a.b) | .name").is_none());
        let filtered = demand(".catalog[] | select(.id > 1) | .name").expect("select fan-out");
        assert!(filtered.filter.is_some());
        assert_eq!(filtered.probe, ElementProbe::Path(vec![key("name")]));
        let left_nested = demand("(.catalog[] | select(.id > 1)) | .name").expect("left-nested select fan-out");
        assert!(left_nested.filter.is_some());
        assert_eq!(left_nested.probe, ElementProbe::Path(vec![key("name")]));
        // Dynamic object keys, optional probes, and values that are not a
        // static field path decline the construct row.
        assert!(demand(".catalog[] | {(.k): .v}").is_none());
        assert!(demand(".catalog[] | {id: .id?}").is_none());
        assert!(demand(".catalog[] | {id: .id + 1}").is_none());
        assert!(demand(".catalog[] | {id: .}").is_none());
        assert!(demand(".catalog[] | {id: 1}").is_none());
        assert!(compiled_fields(".catalog[] | {(.k): .v}").is_none());
    }
}
