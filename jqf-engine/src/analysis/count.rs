//! The COUNT demand table: which programs are count-class rows,
//! and the container path + per-item probe that answers them.
//!
//! This is the analysis half of the count consumer. The structure-only
//! route kind was removed; the count rows it served are recognized HERE
//! and lowered into a [`jqf_data::CountDemand`] that the document-core
//! consumer answers from the lazy document's span skeleton. It is a CLOSED
//! table with a declining default: a shape that is not a row below is not
//! count-equivalent, full stop — the caller's ordinary route reproduces the
//! floor.
//!
//! # The rows
//!
//! | shape | example | demand |
//! | --- | --- | --- |
//! | `[C[] SUFFIX] \| length` | `[.catalog[] \| .name] \| length` | [`CountRow::Collect`], path `C`, probe `SUFFIX` |
//! | `[C[] \| select(P)] \| length` / `PATH \| map(select(P)) \| length` | `.catalog \| map(select(.stock > 0)) \| length` | [`CountRow::Collect`] with [`CountDemand::filter`] |
//! | `PATH \| length` / bare `length` | `.catalog \| length` | [`CountRow::Container`], path `PATH` |
//! | `PATH \| keys \| length` / `PATH \| keys_unsorted \| length` | `.catalog \| keys \| length` | [`CountRow::Container`], path `PATH` |
//!
//! The collect row requires the independent backward-lattice witness
//! ([`ProjectionClass::Structure`]): no element payload is consumed. The
//! container row deliberately does NOT consult the lattice — its soundness is
//! the consumer's container-kind decline (an array's element count, an
//! object's member count, and every other resolved shape — including null
//! and a missing path — falls through to the floor byte for byte, so
//! `PATH | length` still answers 0 and `PATH | keys | length` still raises).
//! `keys | length` is the sort-invariant member/element count, the same
//! answer as `PATH | length` for objects and arrays (an array's keys are its
//! indices).
//!
//! Deliberate v1 declines (the floor serves them): optional (`?`) probe steps,
//! nested iteration (more than one `.[]`), multi-output bodies (`length,
//! length`), `?` on the `keys`/`keys_unsorted` or `length` call, and any path
//! or probe naming an ARRAY POSITION (`.users[0] | length`) — [`PruneNode`]
//! cannot name a position, so that row would keep the whole document to answer
//! one count while the floor reads only the named element. Each decline is a
//! missed win, never a wrong answer. Replicate `[C[] | LITERAL] | length` is
//! the collect row with an empty probe: the Structure witness already names a
//! payload-free body, so the count is the container's element count.

use alloc::vec::Vec;

use jqf_data::{CountDemand, CountRow, CountStep};

use super::prune::PruneNode;
use crate::analysis::ProjectionClass;
use crate::analysis::PushdownSplit;
use crate::analysis::path_steps::{count_each_steps, range_normalized, static_member_prefix, static_path};
use crate::program::{BinaryKind, ProgramNode, ProgramNodeId, StageStart, StageStep, StepAccess};
use jqf_builtins::registry::builtins::id;

/// The recognized count demand of one compiled program, or `None` when the
/// program is not a count row.
pub(crate) fn count_demand(
    nodes: &[ProgramNode],
    root: ProgramNodeId,
    split: PushdownSplit,
    class: &ProjectionClass<'_>,
) -> Option<CountDemand> {
    // The classic collect row requires the Structure witness; the container
    // row carries its own soundness (the consumer's container-kind decline).
    // A FILTER row also carries its own soundness — the closed predicate
    // vocabulary below IS the witness that no payload beyond the tested
    // member is consumed and every item contributes 0-or-1 — so it admits
    // without consulting the lattice.
    let collected = collect_row(nodes, root, split);
    let demand = match collected {
        Some(demand) if demand.filter.is_some() => demand,
        other => match (class, other) {
            (ProjectionClass::Structure, Some(demand)) => demand,
            _ => container_row(nodes, root)?,
        },
    };
    // A demand no prune tree can scope keeps the WHOLE document to answer one
    // count, which is strictly worse than the floor — the floor reads only the
    // position the path names. Measured on the 46 MB `large` fixture:
    // `.users[0] | keys | length` costs 0.18 s / 261 MB on this route against
    // 0.07 s / 51 MB on the ordinary one. Decline, and the floor serves it.
    count_prune_tree(&demand).is_some().then_some(demand)
}

/// The `[C[] SUFFIX] | length` row: the count is Σ per item of `SUFFIX`'s
/// outputs, so `SUFFIX` must be a static `Key`/`Index` path that emits
/// exactly one value per item it touches, or nothing (the reference's empty).
fn collect_row(nodes: &[ProgramNode], root: ProgramNodeId, split: PushdownSplit) -> Option<CountDemand> {
    collect_length_row(nodes, root, split).or_else(|| prefixed_collect_length_row(nodes, root))
}

/// `PATH | map(f) | length` ≡ `[PATH[] | f] | length`. Pipe is right-associative,
/// so the arena is `FlatMap(PATH, FlatMap(Collect, length))` and the unprefixed
/// row's split is the PATH stage, not the collect. Peel the static prefix and
/// recognize the inner collect against its own split.
fn prefixed_collect_length_row(nodes: &[ProgramNode], root: ProgramNodeId) -> Option<CountDemand> {
    let ProgramNode::FlatMap { upstream, body } = &nodes[root.index()] else {
        return None;
    };
    let prefix = static_member_prefix(nodes, *upstream)?;
    let inner_split = crate::analysis::analyze(nodes, *body);
    let mut demand = collect_length_row(nodes, *body, inner_split)?;
    let mut path = prefix;
    path.extend(demand.path.iter().cloned());
    demand.path = path;
    Some(demand)
}

fn collect_length_row(nodes: &[ProgramNode], root: ProgramNodeId, split: PushdownSplit) -> Option<CountDemand> {
    let ProgramNode::FlatMap { upstream, body } = &nodes[root.index()] else {
        return None;
    };
    let ProgramNode::CollectArray {
        body: Some(collect_body),
    } = &nodes[upstream.index()]
    else {
        return None;
    };
    if !is_length_call(nodes, *body) {
        return None;
    }
    // The pushdown ENTRY stage is the collect body's stage (the split
    // descends the collect barrier); its leading `prefix_len` steps are the
    // container path, the step at `prefix_len` is the boundary `.[]` (or a
    // trailing `[a:b]` immediately before it — the container-path range),
    // and the steps after the boundary are the per-item residual
    // (the probe).
    let entry = split.entry();
    let prefix_len = split.prefix_len();
    let ProgramNode::Stage { steps, .. } = &nodes[entry.index()] else {
        return None;
    };
    // The boundary `.[]` must be the FIRST residual step, un-optional (a
    // `.[]?` boundary's suppression semantics live in the floor). A `[a:b]`
    // IMMEDIATELY before it is the container-path range: its bounds
    // normalize (a spelling no integer can carry declines), the `.[]` must
    // follow directly, and the range rides the demand so the consumer counts
    // only the in-range elements.
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
    // The boundary must be the program's SOLE `.[]` (nested iteration's
    // per-item cardinality is not one).
    if count_each_steps(nodes) != 1 {
        return None;
    }
    let path = static_path(&steps[..prefix_len])?;
    let residual = &steps[boundary + 1..];
    // A select directly after the boundary stage is the FILTER twin: no
    // residual steps, and the collect body is exactly
    // `FlatMap { entry stage, Call(select) }` over an admitted predicate.
    // `PATH | map(select(P)) | length` is the same row after
    // [`prefixed_collect_length_row`] peels the static prefix.
    // Any other body spelling declines (probe rows keep their law).
    let filter = if residual.is_empty() {
        select_filter(nodes, *collect_body, entry)
    } else {
        None
    };
    let probe = static_path(residual)?;
    Some(CountDemand {
        row: CountRow::Collect,
        path,
        range,
        probe,
        filter,
    })
}

/// The `select(P)` half of the filter twin: the collect body must be EXACTLY
/// `FlatMap { upstream == entry stage, body == Call(select/1, [P]) }` and P
/// must be an [`admitted_predicate`]. Anything else — an intermediate pipe
/// stage, another call, extra arguments — declines.
fn select_filter(
    nodes: &[ProgramNode],
    collect_body: ProgramNodeId,
    entry: ProgramNodeId,
) -> Option<jqf_data::CountFilter> {
    let ProgramNode::FlatMap { upstream, body } = &nodes[collect_body.index()] else {
        return None;
    };
    if *upstream != entry {
        return None;
    }
    let ProgramNode::Call { overload, args, .. } = &nodes[body.index()] else {
        return None;
    };
    if overload.get() != jqf_builtins::registry::builtins::id::SELECT || args.len() != 1 {
        return None;
    }
    let (test, key) = admitted_predicate(nodes, args[0])?;
    Some(jqf_data::CountFilter {
        // The predicate's ONE key step is the tested member's navigation:
        // the consumers read it per element (the byte scan matches the key
        // text; the owned walk descends it). Without the transfer the
        // filter names no member and every consumer declines.
        path: alloc::vec![jqf_data::CountStep::ObjectKey(key)],
        test,
    })
}

/// The closed predicate vocabulary of the collect-filter row. Every form
/// here is provably SINGLE-output over its domain — a Key step emits exactly
/// one value per item it touches (present member, absent member's null, or
/// a null item), a literal emits one, so a comparison emits one pair — and
/// a `select` over a single-output truthy-or-not predicate contributes 0 or
/// 1 per item. A raise anywhere (a key step over a non-object) is not
/// expressible in the demand; the consumer declines those items and the
/// floor renders the reference's error byte for byte.
///
/// Admitted:
/// - `.key` — the member's truthiness;
/// - `.key OP LITERAL` — the member against a compile-time scalar literal
///   (`==` `!=` `<` `<=` `>` `>=`; null, boolean, exact-decimal number, or
///   string; array/object literals decline — they would open same-rank deep
///   compares the closed law does not own).
///
/// Everything else declines: multi-step paths, optionals (`?`), dynamic or
/// variable operands, reversed operand order, float-category literals.
pub(crate) fn admitted_predicate(
    nodes: &[ProgramNode],
    arg: ProgramNodeId,
) -> Option<(jqf_data::CountTest, alloc::string::String)> {
    // The bare-truthiness spelling: the argument IS one Key step over the
    // item.
    if let ProgramNode::Stage {
        start: StageStart::Current,
        steps,
    } = &nodes[arg.index()]
    {
        single_key_step(steps)?;
        let StepAccess::Key(key) = steps[0].access() else {
            unreachable!("single_key_step admitted a non-key")
        };
        return Some((jqf_data::CountTest::Truthy, alloc::string::String::from(key)));
    }
    // The comparison spelling has TWO lowered forms over one law — the
    // member navigated once, compared against one literal:
    // - `FlatMap { key stage, Binary { OP, empty current, literal } }`
    //   (the ordering operators' fused shape), and
    // - `Binary { OP, key stage, literal }` with the navigation riding the
    //   LEFT operand (`==`'s framed shape).
    // In both, the binary is the RIGHT-outer product of two graphs over the
    // SAME input; each side here emits exactly one value per item it
    // touches, so the comparison emits exactly one boolean.
    let (op, rhs, key) = match &nodes[arg.index()] {
        ProgramNode::FlatMap { upstream, body } => {
            // Fused ordering shape: the key stage drives, then the binary
            // compares an empty Current (the piped member) against a
            // step-less literal.
            let ProgramNode::Binary {
                op:
                    kind @ (BinaryKind::Equal
                    | BinaryKind::NotEqual
                    | BinaryKind::Less
                    | BinaryKind::LessEqual
                    | BinaryKind::Greater
                    | BinaryKind::GreaterEqual),
                left,
                right,
                ..
            } = &nodes[body.index()]
            else {
                return None;
            };
            let op = compare_kind(*kind)?;
            let ProgramNode::Stage {
                start: StageStart::Current,
                steps,
            } = &nodes[upstream.index()]
            else {
                return None;
            };
            single_key_step(steps)?;
            let StepAccess::Key(key) = steps[0].access() else {
                unreachable!("single_key_step admitted a non-key")
            };
            // The piped member IS the binary's left operand: an empty
            // Current stage. Any other left graph reads beyond the one
            // member and declines.
            let ProgramNode::Stage {
                start: StageStart::Current,
                steps: left_steps,
            } = &nodes[left.index()]
            else {
                return None;
            };
            if !left_steps.is_empty() {
                return None;
            }
            (op, count_literal(nodes, *right)?, alloc::string::String::from(key))
        }
        ProgramNode::Binary {
            op:
                kind @ (BinaryKind::Equal
                | BinaryKind::NotEqual
                | BinaryKind::Less
                | BinaryKind::LessEqual
                | BinaryKind::Greater
                | BinaryKind::GreaterEqual),
            left,
            right,
            ..
        } => {
            // Framed shape (`==` lowers here): the navigation rides the
            // binary's LEFT operand itself — one Key stage over select's
            // input.
            let op = compare_kind(*kind)?;
            let ProgramNode::Stage {
                start: StageStart::Current,
                steps,
            } = &nodes[left.index()]
            else {
                return None;
            };
            single_key_step(steps)?;
            let StepAccess::Key(key) = steps[0].access() else {
                unreachable!("single_key_step admitted a non-key")
            };
            (op, count_literal(nodes, *right)?, alloc::string::String::from(key))
        }
        _ => return None,
    };
    Some((jqf_data::CountTest::Compare { op, rhs }, key))
}

/// The count-vocabulary comparison operator for one engine binary kind, or
/// `None` for every non-comparison kind.
fn compare_kind(kind: BinaryKind) -> Option<jqf_data::CountCompare> {
    Some(match kind {
        BinaryKind::Equal => jqf_data::CountCompare::Equal,
        BinaryKind::NotEqual => jqf_data::CountCompare::NotEqual,
        BinaryKind::Less => jqf_data::CountCompare::Less,
        BinaryKind::LessEqual => jqf_data::CountCompare::LessEqual,
        BinaryKind::Greater => jqf_data::CountCompare::Greater,
        BinaryKind::GreaterEqual => jqf_data::CountCompare::GreaterEqual,
        _ => return None,
    })
}

/// The one Key step a predicate path may carry: present, un-optional, and
/// nothing else.
fn single_key_step(steps: &[StageStep]) -> Option<()> {
    if steps.len() != 1 || steps[0].is_optional() {
        return None;
    }
    match steps[0].access() {
        StepAccess::Key(_) => Some(()),
        _ => None,
    }
}

/// The compiled right-hand scalar of a filter comparison: a bare literal
/// stage with no steps, mapped into the closed vocabulary. Float-category
/// numbers have no exact decimal reading and decline.
fn count_literal(nodes: &[ProgramNode], id: ProgramNodeId) -> Option<jqf_data::CountLiteral> {
    let ProgramNode::Stage {
        start: StageStart::Literal(value),
        steps,
    } = &nodes[id.index()]
    else {
        return None;
    };
    if !steps.is_empty() {
        return None;
    }
    match value {
        jqf_data::Value::Null => Some(jqf_data::CountLiteral::Null),
        jqf_data::Value::Bool(value) => Some(jqf_data::CountLiteral::Bool(*value)),
        jqf_data::Value::String(text) => Some(jqf_data::CountLiteral::Text(alloc::string::String::from(text.as_str()))),
        jqf_data::Value::Number(number) => {
            let (negative, digits, scale) = match number.category() {
                jqf_data::NumberCategory::Integer => {
                    // `to_integer` covers machine and boxed integers;
                    // `as_integer` sees only the boxed form.
                    let integer = number.to_integer()?;
                    let text = integer.as_str();
                    (
                        text.starts_with('-'),
                        alloc::string::String::from(text.trim_start_matches('-')),
                        0,
                    )
                }
                jqf_data::NumberCategory::Decimal => {
                    let decimal = number.as_decimal()?;
                    let coefficient = decimal.coefficient().as_str();
                    (
                        coefficient.starts_with('-'),
                        alloc::string::String::from(coefficient.trim_start_matches('-')),
                        decimal.scale(),
                    )
                }
                // A float-category literal has no exact decimal reading.
                jqf_data::NumberCategory::Float => return None,
            };
            Some(jqf_data::CountLiteral::Decimal {
                negative,
                digits,
                scale,
            })
        }
        _ => None,
    }
}

/// The `PATH | length` row (bare `length` included — an empty path is the
/// root selection), and the keys-count twin `PATH | keys | length` /
/// `PATH | keys_unsorted | length` (empty PATH is the root `keys | length`).
///
/// Pipe is right-associative, so the keys-count IR is
/// `FlatMap { PATH, FlatMap { keys, length } }`. The parenthesized
/// left-associated twin `(PATH | keys) | length` is the same row. A `?` on
/// either call is a `Try` and is not a row.
fn container_row(nodes: &[ProgramNode], root: ProgramNodeId) -> Option<CountDemand> {
    match &nodes[root.index()] {
        // The bare-root spelling `length`.
        ProgramNode::Call { overload, args, .. } if overload.get() == id::LENGTH && args.is_empty() => {
            Some(container_demand(Vec::new(), None))
        }
        // `PATH | length`, `keys | length`, and `(PATH | keys) | length`.
        ProgramNode::FlatMap { upstream, body } if is_length_call(nodes, *body) => {
            if let Some(demand) = container_from_path_node(nodes, *upstream) {
                return Some(demand);
            }
            if is_keys_count_call(nodes, *upstream) {
                return Some(container_demand(Vec::new(), None));
            }
            // `(PATH | keys) | length`: the left-associated parenthesized twin.
            let ProgramNode::FlatMap {
                upstream: path,
                body: keys,
            } = &nodes[upstream.index()]
            else {
                return None;
            };
            if is_keys_count_call(nodes, *keys) {
                container_from_path_node(nodes, *path)
            } else {
                None
            }
        }
        // `PATH | (keys | length)`: the authored right-associated spelling.
        ProgramNode::FlatMap { upstream, body } if is_keys_then_length(nodes, *body) => {
            container_from_path_node(nodes, *upstream)
        }
        _ => None,
    }
}

/// The container demand of a static `Current` path stage, including a
/// trailing `[a:b]` as the container-path range.
fn container_from_path_node(nodes: &[ProgramNode], id: ProgramNodeId) -> Option<CountDemand> {
    let ProgramNode::Stage {
        start: StageStart::Current,
        steps,
    } = &nodes[id.index()]
    else {
        return None;
    };
    let (path, range) = match steps.split_last() {
        Some((last, prefix)) if matches!(last.access(), StepAccess::Slice(_)) => {
            if last.is_optional() {
                return None;
            }
            let StepAccess::Slice(bounds) = last.access() else {
                unreachable!("the split_last arm named a slice")
            };
            (static_path(prefix)?, Some(range_normalized(bounds)?))
        }
        _ => (static_path(steps)?, None),
    };
    Some(container_demand(path, range))
}

fn container_demand(path: Vec<CountStep>, range: Option<(Option<i64>, Option<i64>)>) -> CountDemand {
    CountDemand {
        row: CountRow::Container,
        path,
        range,
        probe: Vec::new(),
        filter: None,
    }
}

/// Whether `id` is a bare `length` call.
fn is_length_call(nodes: &[ProgramNode], id: ProgramNodeId) -> bool {
    matches!(
        &nodes[id.index()],
        ProgramNode::Call { overload, args, .. }
            if args.is_empty() && overload.get() == jqf_builtins::registry::builtins::id::LENGTH
    )
}

/// Whether `id` is a bare `keys` or `keys_unsorted` call.
fn is_keys_count_call(nodes: &[ProgramNode], id: ProgramNodeId) -> bool {
    matches!(
        &nodes[id.index()],
        ProgramNode::Call { overload, args, .. }
            if args.is_empty()
                && (overload.get() == jqf_builtins::registry::builtins::id::KEYS
                    || overload.get() == jqf_builtins::registry::builtins::id::KEYS_UNSORTED)
    )
}

/// Whether `id` is `keys | length` or `keys_unsorted | length`.
fn is_keys_then_length(nodes: &[ProgramNode], id: ProgramNodeId) -> bool {
    let ProgramNode::FlatMap { upstream, body } = &nodes[id.index()] else {
        return false;
    };
    is_keys_count_call(nodes, *upstream) && is_length_call(nodes, *body)
}

/// The static path a type-class program names, or `None` when it is not a row.
/// Empty is the document root.
///
/// Rows: a bare-root `type` call (empty path) and `PATH | type` where PATH is a
/// static Current Key/Index stage with no trailing slice. A trailing slice
/// (`.[1:3] | type`) declines: dropping the range would look like bare `type`
/// and Whole would type the root. `type?` (a `Try` root) and any other wrapper
/// stay off the table — the floor serves them. The path rides the requirement
/// so a codec can skip building the named node's payload; XML's
/// measure-skeleton still only opens on the empty path.
pub(crate) fn type_demand_path(nodes: &[ProgramNode], root: ProgramNodeId) -> Option<Vec<CountStep>> {
    path_then_call(nodes, root, is_type_call)
}

fn is_type_call(nodes: &[ProgramNode], id: ProgramNodeId) -> bool {
    matches!(
        &nodes[id.index()],
        ProgramNode::Call { overload, args, .. }
            if args.is_empty() && overload.get() == jqf_builtins::registry::builtins::id::TYPE
    )
}

/// The recognized KEYS-publish demand: `keys` at the root, or `PATH | keys`
/// over a static Current path with no trailing slice. `keys_unsorted`
/// declines because the publish sorts. The keys-count twins
/// (`PATH | keys | length`) stay on the count table — they match first at
/// requirement lowering. `keys?` declines (a Try root).
pub(crate) fn keys_demand(nodes: &[ProgramNode], root: ProgramNodeId) -> Option<Vec<CountStep>> {
    if !keys_builtin_present(nodes) {
        return None;
    }
    path_then_call(nodes, root, is_keys_sorted_call)
}

pub(crate) fn keys_builtin_present(nodes: &[ProgramNode]) -> bool {
    nodes.iter().any(|node| {
        matches!(
            node,
            ProgramNode::Call { overload, args, .. }
                if args.is_empty() && overload.get() == jqf_builtins::registry::builtins::id::KEYS
        )
    })
}

fn path_then_call(
    nodes: &[ProgramNode],
    root: ProgramNodeId,
    is_call: fn(&[ProgramNode], ProgramNodeId) -> bool,
) -> Option<Vec<CountStep>> {
    if is_call(nodes, root) {
        return Some(Vec::new());
    }
    let ProgramNode::FlatMap { upstream, body } = &nodes[root.index()] else {
        return None;
    };
    if !is_call(nodes, *body) {
        return None;
    }
    let demand = container_from_path_node(nodes, *upstream)?;
    demand.range.is_none().then_some(demand.path)
}

fn is_keys_sorted_call(nodes: &[ProgramNode], id: ProgramNodeId) -> bool {
    matches!(
        &nodes[id.index()],
        ProgramNode::Call { overload, args, .. }
            if args.is_empty() && overload.get() == jqf_builtins::registry::builtins::id::KEYS
    )
}

/// The kept-subtree prune for a count demand: the container's elements keep
/// their SPINE (the count is the only observable — no element payload), with
/// the probe's static key path kept all the way to its leaves.
///
/// The tree rides the count requirement as the same monotone hint
/// [`PruneNode`] rides the ordinary whole-document lowering: the strict-JSON
/// codec drops it under the lazy frontier (spans win there), and an eager
/// codec with no span skeleton (YAML) honors it and builds only the counted
/// container's element skeletons — the count-gate rss lanes' memory.
///
/// `None` — a path or probe containing an array index, which the tree cannot
/// name — is what [`count_demand`] declines the whole row on: an unscoped
/// count route keeps the whole document, and the floor is cheaper.
pub(crate) fn count_prune_tree(demand: &CountDemand) -> Option<PruneNode> {
    let element = if let Some(filter) = demand.filter.as_ref() {
        let mut node = PruneNode::all();
        for step in filter.path.iter().rev() {
            match step {
                CountStep::ObjectKey(key) => {
                    node = PruneNode::default().with_key(key.clone(), node);
                }
                CountStep::ArrayIndex(_) => return None,
            }
        }
        node
    } else if demand.probe.is_empty() {
        // The elements' spine: the element nodes exist, their payloads are
        // pruned — exactly what the element COUNT needs.
        PruneNode::default()
    } else {
        // The probe's static key path kept whole.
        let mut probe = PruneNode::all();
        for step in demand.probe.iter().rev() {
            match step {
                CountStep::ObjectKey(key) => {
                    probe = PruneNode::default().with_key(key.clone(), probe);
                }
                CountStep::ArrayIndex(_) => return None,
            }
        }
        probe
    };
    // The container's elements share the element prune; the container itself
    // is kept whole by the walk (the count reads it).
    let mut node = PruneNode::default().with_element(element);
    for step in demand.path.iter().rev() {
        match step {
            CountStep::ObjectKey(key) => {
                node = PruneNode::default().with_key(key.clone(), node);
            }
            CountStep::ArrayIndex(_) => return None,
        }
    }
    Some(node)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec_requirement::CodecRequirementPolicy;
    use crate::compile::{CompileOptions, try_compile_program};
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

    fn compiled(source: &str) -> crate::compile::CompiledProgram {
        let resources = resources();
        let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
        try_compile_program(source, policy, CompileOptions::new(), &resources).expect("compiles")
    }

    fn demand(source: &str) -> Option<CountDemand> {
        compiled(source).count_demand().cloned()
    }

    #[test]
    fn collect_rows_recognize_path_and_probe() {
        let d = demand("[.catalog[] | .name] | length").expect("collect row");
        assert_eq!(d.row, CountRow::Collect);
        assert_eq!(d.path, vec![CountStep::ObjectKey("catalog".into())]);
        assert_eq!(d.probe, vec![CountStep::ObjectKey("name".into())]);

        let d = demand("[.catalog[]] | length").expect("collect row");
        assert_eq!(d.row, CountRow::Collect);
        assert_eq!(d.path, vec![CountStep::ObjectKey("catalog".into())]);
        assert!(d.probe.is_empty());

        let d = demand("map(.name) | length").expect("collect row");
        assert_eq!(d.row, CountRow::Collect);
        assert!(d.path.is_empty());
        assert_eq!(d.probe, vec![CountStep::ObjectKey("name".into())]);

        // Pipe is right-associative: PATH | map(f) | length ≡ [PATH[] | f] | length.
        let mapped = demand(".catalog | map(.name) | length").expect("prefixed map|length");
        let collected = demand("[.catalog[] | .name] | length").expect("collect row");
        assert_eq!(mapped, collected);
    }

    #[test]
    fn container_rows_recognize_path_and_root() {
        let d = demand("length").expect("container row");
        assert_eq!(d.row, CountRow::Container);
        assert!(d.path.is_empty());

        let d = demand(".catalog | length").expect("container row");
        assert_eq!(d.row, CountRow::Container);
        assert_eq!(d.path, vec![CountStep::ObjectKey("catalog".into())]);
    }

    #[test]
    fn keys_count_rows_are_container_at_path() {
        let length = demand(".catalog | length").expect("container row");
        assert_eq!(demand(".catalog | keys | length").as_ref(), Some(&length));
        assert_eq!(demand(".catalog | keys_unsorted | length").as_ref(), Some(&length));
        assert_eq!(demand("(.catalog | keys) | length").as_ref(), Some(&length));

        let root = demand("length").expect("root container");
        assert_eq!(demand("keys | length").as_ref(), Some(&root));
        assert_eq!(demand("keys_unsorted | length").as_ref(), Some(&root));
    }

    #[test]
    fn keys_count_ir_is_path_then_keys_then_length() {
        use crate::program::ProgramNode;
        let program = compiled(".catalog | keys | length");
        let nodes = program.arena();
        let ProgramNode::FlatMap { upstream, body } = &nodes[program.root().index()] else {
            panic!(
                "keys-count root must be FlatMap, got {:?}",
                nodes[program.root().index()]
            );
        };
        assert!(
            super::container_from_path_node(nodes, *upstream).is_some(),
            "upstream must be the catalog path stage"
        );
        assert!(super::is_keys_then_length(nodes, *body), "body must be keys | length");
    }

    #[test]
    fn keys_count_optional_and_nested_iteration_decline() {
        assert!(demand(".catalog | keys? | length").is_none());
        assert!(demand(".catalog | keys | length?").is_none());
        assert!(demand("keys? | length").is_none());
        assert!(demand(".catalog[] | keys | length").is_none());
        assert!(demand(".[] | keys | length").is_none());
    }

    #[test]
    fn range_rows_recognize_the_container_path_range() {
        // `[.catalog[10:20][] | .name] | length`: the boundary `.[]` is
        // preceded by a trailing `[a:b]` — the range rides the demand and the
        // consumer counts only the in-range elements.
        let d = demand("[.catalog[10:20][] | .name] | length").expect("range collect row");
        assert_eq!(d.row, CountRow::Collect);
        assert_eq!(d.path, vec![CountStep::ObjectKey("catalog".into())]);
        assert_eq!(d.range, Some((Some(10), Some(20))));
        assert_eq!(d.probe, vec![CountStep::ObjectKey("name".into())]);

        // `PATH[a:b] | length`: the trailing slice on the container path is
        // the range.
        let d = demand(".catalog[10:20] | length").expect("range container row");
        assert_eq!(d.row, CountRow::Container);
        assert_eq!(d.path, vec![CountStep::ObjectKey("catalog".into())]);
        assert_eq!(d.range, Some((Some(10), Some(20))));
        assert!(d.probe.is_empty());

        // An open end bound rides as `None`.
        let d = demand(".catalog[5:] | length").expect("open-end range row");
        assert_eq!(d.range, Some((Some(5), None)));

        // A slice NOT immediately before the `.[]` declines (a mid-path
        // slice is not a container-path range).
        assert!(demand("[.catalog[10:20] | .items[] | .name] | length").is_none());
    }

    #[test]
    fn non_count_shapes_decline() {
        // A residual that reads element payloads without a closed filter law.
        assert!(demand("[.catalog[] | length] | length").is_none());
        // Nested iteration.
        assert!(demand("[.catalog[] | .items[] | .name] | length").is_none());
        // A body that reads element payloads (the Structure witness).
        assert!(demand("[.catalog[] | length] | length").is_none());
        // An optional probe step.
        assert!(demand("[.catalog[] | .name?] | length").is_none());
        // A NON-normalizing range declines (a strictly-negative resolved bound
        // needs the container's length, which costs the early stop).
        assert!(demand(".catalog[-2:] | length").is_none());
        // An optional slice declines.
        assert!(demand(".catalog[0:2]? | length").is_none());
        // A path or probe naming an ARRAY POSITION declines: no prune tree can
        // scope it, so the row would keep the whole document while the floor
        // reads only that element.
        assert!(demand(".users[0] | length").is_none());
        assert!(demand(".users[0] | keys | length").is_none());
        assert!(demand("[.catalog[] | .items[0]] | length").is_none());
    }

    #[test]
    fn replicate_literal_collect_is_the_empty_probe_row() {
        // `[C[] | LITERAL] | length` is the collect row with an empty probe:
        // the Structure witness already names a payload-free body, so the
        // count is the container's element count (one output per item).
        let d = demand("[.catalog[] | 1] | length").expect("replicate collect");
        assert_eq!(d.row, CountRow::Collect);
        assert_eq!(d.path, vec![CountStep::ObjectKey("catalog".into())]);
        assert!(d.probe.is_empty());
        assert!(d.filter.is_none());
        assert_eq!(
            demand("[.catalog[] | true] | length").as_ref(),
            Some(&d),
            "boolean literal is the same empty-probe collect"
        );
        let root = demand("[.[] | null] | length").expect("root replicate");
        assert_eq!(root.row, CountRow::Collect);
        assert!(root.path.is_empty());
        assert!(root.probe.is_empty());
    }

    // -- Collect-filter rows: `[C[] | select(P)] | length`. ---------------

    fn filter_of(source: &str) -> jqf_data::CountFilter {
        let demand = demand(source).unwrap_or_else(|| panic!("{source} declined"));
        demand.filter.unwrap_or_else(|| panic!("{source} has no filter"))
    }

    #[test]
    fn select_over_comparison_admits_as_filter_row() {
        let f = filter_of("[.catalog[] | select(.stock > 0)] | length");
        assert_eq!(
            f,
            jqf_data::CountFilter {
                path: alloc::vec![jqf_data::CountStep::ObjectKey(alloc::string::String::from("stock"))],
                test: jqf_data::CountTest::Compare {
                    op: jqf_data::CountCompare::Greater,
                    rhs: jqf_data::CountLiteral::Decimal {
                        negative: false,
                        digits: alloc::string::String::from("0"),
                        scale: 0,
                    },
                },
            }
        );

        // Every comparison operator admits with its operator.
        for (source, op) in [
            ("[.a[] | select(.k == 1)] | length", jqf_data::CountCompare::Equal),
            ("[.a[] | select(.k != 1)] | length", jqf_data::CountCompare::NotEqual),
            ("[.a[] | select(.k < 1)] | length", jqf_data::CountCompare::Less),
            ("[.a[] | select(.k <= 1)] | length", jqf_data::CountCompare::LessEqual),
            ("[.a[] | select(.k > 1)] | length", jqf_data::CountCompare::Greater),
            (
                "[.a[] | select(.k >= 1)] | length",
                jqf_data::CountCompare::GreaterEqual,
            ),
        ] {
            let f = filter_of(source);
            let jqf_data::CountTest::Compare { op: got, .. } = f.test else {
                panic!("{source} is a comparison")
            };
            assert_eq!(got, op, "{source}");
        }

        let text = filter_of(r#"[.a[] | select(.k == "x")] | length"#);
        assert_eq!(
            text.test,
            jqf_data::CountTest::Compare {
                op: jqf_data::CountCompare::Equal,
                rhs: jqf_data::CountLiteral::Text(alloc::string::String::from("x")),
            }
        );
        let truthy = filter_of("[.a[] | select(.k)] | length");
        assert_eq!(truthy.test, jqf_data::CountTest::Truthy);

        // Exact decimals keep their parts (never a binary64 rounding).
        let d = filter_of("[.a[] | select(.k > -12.30e1)] | length");
        assert_eq!(
            d.test,
            jqf_data::CountTest::Compare {
                op: jqf_data::CountCompare::Greater,
                rhs: jqf_data::CountLiteral::Decimal {
                    negative: true,
                    digits: alloc::string::String::from("1230"),
                    scale: 1,
                },
            }
        );
    }

    #[test]
    fn filter_rows_decline_every_wider_predicate_shape() {
        // Multi-step paths, optionals, index steps.
        assert!(demand("[.a[] | select(.b.c > 1)] | length").is_none());
        assert!(demand("[.a[] | select(.b?.c? > 1)] | length").is_none());
        assert!(demand("[.a[] | select(.[0] > 1)] | length").is_none());
        // Reversed operand order reads beyond the piped member.
        assert!(demand("[.a[] | select(1 < .k)] | length").is_none());
        // Dynamic / computed right-hand sides.
        assert!(demand("[.a[] | select(.k > .lim)] | length").is_none());
        assert!(demand("[.a[] | select(.k > (1 + 2))] | length").is_none());
        // Array/object literals would open same-rank deep compares.
        assert!(demand("[.a[] | select(.k == [1])] | length").is_none());
        assert!(demand("[.a[] | select(.k == {})] | length").is_none());
        // Non-comparison predicates and non-select bodies.
        assert!(demand("[.a[] | select(.k and .j)] | length").is_none());
        assert!(demand("[.a[] | select(.k > 1 or .k < -1)] | length").is_none());
        assert!(demand("[.a[] | map(select(.k > 1))] | length").is_none());
        // A residual after select is outside the filter vocabulary; this
        // tree declines the whole row (the floor answers it).
        assert!(demand("[.a[] | select(.k > 1) | .j] | length").is_none());
        // Nested iteration's per-item cardinality is not one.
        assert!(demand("[.a[][].k | select(. > 1)] | length").is_none());
    }

    #[test]
    fn filter_rows_carry_the_container_path_and_range() {
        let d = demand("[.catalog[10:20][] | select(.stock > 0)] | length").expect("range filter row");
        assert_eq!(d.path, vec![CountStep::ObjectKey("catalog".into())]);
        assert_eq!(d.range, Some((Some(10), Some(20))));
        assert!(d.probe.is_empty());
        assert!(d.filter.is_some());

        // The prune tree stays the elements' spine (probe is empty).
        let tree = super::count_prune_tree(&d);
        assert!(tree.is_some(), "spine prune applies to filter rows");
    }

    #[test]
    fn prefixed_map_select_length_is_the_filter_row() {
        let mapped = demand(".catalog | map(select(.stock > 0)) | length").expect("PATH|map(select)|length");
        let collected = demand("[.catalog[] | select(.stock > 0)] | length").expect("collect filter");
        assert_eq!(mapped, collected);
        assert!(mapped.filter.is_some());
        assert_eq!(mapped.path, vec![CountStep::ObjectKey("catalog".into())]);

        let root_map = demand("map(select(.stock > 0)) | length").expect("map(select)|length");
        let root_collect = demand("[.[] | select(.stock > 0)] | length").expect("root collect filter");
        assert_eq!(root_map, root_collect);

        let active = demand(".users | map(select(.active == true)) | length").expect("boolean filter");
        assert_eq!(active.path, vec![CountStep::ObjectKey("users".into())]);
        assert!(active.filter.is_some());
    }
}

#[cfg(test)]
mod requirement_tests {
    use crate::codec_requirement::CodecRequirementPolicy;
    use crate::compile::{CompileOptions, try_compile_program};
    use jqf_codec_core::{DiagnosticPolicy, ValidationMode};
    use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

    static CONTROL: ContinueControl = ContinueControl;

    #[test]
    fn count_program_lowers_the_whole_requirement_with_the_hint() {
        let resources = ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                .expect("account"),
            &CONTROL,
            WorkMeter::try_new_v1(4096).expect("work"),
        )
        .expect("resources");
        let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
        let program = try_compile_program("length", policy, CompileOptions::new(), &resources).expect("compiles");
        let requirement = program.try_requirement(&resources).expect("lowers");
        assert!(requirement.count().is_some(), "length must carry the count hint");
        assert!(requirement.footprint().is_whole());
        assert!(requirement.result() == jqf_codec_core::AccessResultKind::CompleteDocument);
    }
}

#[cfg(test)]
mod prune_tests {
    use crate::codec_requirement::CodecRequirementPolicy;
    use crate::compile::{CompileOptions, try_compile_program};
    use jqf_codec_core::{DiagnosticPolicy, ValidationMode};
    use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

    static CONTROL: ContinueControl = ContinueControl;

    #[test]
    fn count_program_lowers_with_the_prune_hint() {
        let resources = ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                .expect("account"),
            &CONTROL,
            WorkMeter::try_new_v1(4096).expect("work"),
        )
        .expect("resources");
        let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
        for (source, should_prune) in [
            ("[.catalog[]] | length", true),
            ("[.catalog[] | .name] | length", true),
            ("length", true),
        ] {
            let program = try_compile_program(source, policy, CompileOptions::new(), &resources).expect("compiles");
            let requirement = program.try_requirement(&resources).expect("lowers");
            assert!(requirement.count().is_some(), "{source} must carry the count hint");
            assert_eq!(requirement.prune().is_some(), should_prune, "{source} prune presence");
        }
    }
}
