//! The CORRELATED-SCAN table — which `select` scans over an outer-bound
//! container may be driven from a sorted keyed index instead of element by
//! element.
//!
//! A correlated equi-join written the way users actually write it —
//! `.orders as $o | .users[] | [$o[] | select(.user_id == $u.id)]` and its
//! `map` spelling — executes Θ(k·m): the inner container is rescanned in full
//! once per outer element. This table names the shapes where that inner scan is
//! provably an equality probe against a key path, so the executor can visit only
//! the matching children (`exec::GraphFrame::IndexedEach`) instead of all of
//! them.
//!
//! Like [`count`](super::count) this is a CLOSED table with a declining default:
//! a shape that is not a row below is not a correlated scan, full stop. A new
//! shape joins by adding a row and its soundness argument, never by widening a
//! default.
//!
//! # What the table promises, and what it does NOT
//!
//! A recognized scan promises exactly one thing: **the node's output sequence is
//! unchanged**. The rewrite replaces the SOURCE ITERATION and nothing else — the
//! `select` predicate still runs, whole and unmodified, over every child the
//! index hands back. That is why the table constrains the source and the
//! predicate's leftmost conjunct and says nothing at all about the CONSUMER: a
//! consumer cannot distinguish two producers that emit the same values in the
//! same order, so `[scan] | length`, `[scan]`, `scan | .id`, and any other
//! consumer are covered by one argument rather than by one row each.
//!
//! The skipped children are exactly those whose key is not equal to the probe.
//! For each of them the naive scan would have evaluated
//! `Binary{Equal, keypath, probe}` to `false` and short-circuited the enclosing
//! `and` before any residual conjunct ran, so skipping them removes no emission,
//! no error, and no ordering. Three side conditions make that argument total,
//! and each is enforced at RUN time rather than assumed here:
//!
//! 1. **The build is TOTAL.** The key path is navigated over EVERY child before
//!    any child is emitted; a navigation that would raise declines the whole
//!    route back to the naive scan. That is `count.rs`'s DOWNGRADE law in its
//!    cheapest form — the build is the scan's first act, so at decline time the
//!    scan has emitted nothing and the naive path reproduces the raise in its
//!    original position.
//! 2. **The probe is TOTAL.** The outer key expression is evaluated ONCE where
//!    the naive form evaluates it once per child. If evaluating it raises, or
//!    yields anything but a single value, the route declines and the naive scan
//!    raises where it always did.
//! 3. **The container is NON-EMPTY.** Over an empty container the naive form
//!    evaluates the outer key expression ZERO times (`Binary` is right-outer per
//!    invocation, and there is no invocation), so an indexed form that probed
//!    once could surface an error the naive form never reaches — the probe
//!    would run even though no child exists to evaluate it against. An empty
//!    container declines.
//!
//! # The rows
//!
//! ## SOURCE row — where the scanned container comes from
//!
//! | row | shape | why it is an outer-bound container |
//! | --- | --- | --- |
//! | S1 | `FlatMap{ Stage{Variable(S) \| Current, [static…, Each]}, Call(SELECT, [P]) }` | the source stage navigates a STATIC path ending in `.[]`, so the container it iterates is a fixed value whose identity the runtime can key on: the `.[]` is the stage's LAST step, so the value reaching `select` IS the child |
//!
//! The container path is the stage's static PREFIX (the steps before the
//! `.[]`) applied to the scan node's input. For a `Variable` start the prefix
//! navigates the bound slot (`$o[].user | select(…)`); for a `Current` start
//! it navigates the scan's OWN input, which admits three spellings under one
//! row:
//!
//! - the SPLIT-FORM spelling `$o | .users[] | select(…)`, whose container
//!   `.users` is reached by descending the `Current` stage's prefix over the
//!   located input;
//! - the `map` spelling `$o | map(select(…))` and `$o | map(.users[] |
//!   select(…))`: the inner scan `[.[] | select(P)]`-body IS a `Current`-start
//!   source with an empty (or static) prefix, so the map lowering's collect
//!   barrier needs no row of its own to be looked through;
//! - the bare root `.[] | select(…)`, recognized too and harmless: the runtime
//!   warm-up keys the index slot on the CONTAINER'S NODE HANDLE, and a root
//!   document is seen once, so a bare scan never builds an index it cannot
//!   amortize.
//!
//! The `.[]` must be the stage's LAST step. A residual after it (`$o[].line |
//! select(…)`) would make the value reaching `select` something other than the
//! child, so the index's key path would not be the predicate's, and skipping a
//! child would skip a residual navigation that can raise. Declined.
//!
//! ## FILTER row — which conjunct is the probe
//!
//! | row | shape | why only this one |
//! | --- | --- | --- |
//! | 1 | `P`'s LEFTMOST top-level conjunct is `Binary{Equal, A, B}` where one side is `Stage{Current, [Key/Index…]}` (no `?`) and the other is a PROBE (below) | `Logical{And}` is left-outer short-circuiting, so a later conjunct never runs on a child the leftmost rejected. Indexing a LATER conjunct would skip earlier conjuncts' ERRORS on non-candidates, so the index key is always the leftmost conjunct |
//!
//! `P` is walked into through `Logical{And}` LEFT edges only. `Logical{Or}`,
//! `Conditional`, `Try`, and every other composition stop the walk: the leftmost
//! conjunct of an `or` is not a necessary condition for the whole predicate.
//!
//! An optional (`?`) step anywhere in the key path is declined. A `?` turns the
//! foreign-category outcome from "raises" into "emits nothing", which changes
//! the operand's CARDINALITY — the `Binary` then produces no output for that
//! child and `select` emits nothing. The index has no cell for "no key", so
//! the row would have to invent one.
//!
//! ## PROBE rows — the element-independent side of the equality
//!
//! | row | shape | why it is constant across the scan |
//! | --- | --- | --- |
//! | K1 | `Stage{Variable(T), [Key/Index…]}` (no `?`) | reads an env slot and static steps only; nothing in it reads the current element |
//! | K2 | `Stage{Literal(v), []}` | a literal reads nothing at all |
//! | K3 | K1/K2 leaves composed through `Binary`, `Logical` and `Conditional` | each composition family is total and single-output over single-output children, so the whole expression is one value per scan — the wider probe grammar, admitted now that the executor evaluates it with the engine's own operators (`semantics::binary::apply`, the truth law) rather than a second expression language |
//!
//! A `Current`-start stage is declined ANYWHERE in the probe graph: it reads
//! the current element, which is the key path's side of the equality. `Call`,
//! `Try`, `Choice`, constructors, and the binding family are not rows either:
//! each is sound to add only with its own evaluator arm, and the declining
//! default is what makes the omission safe rather than silent.
//!
//! Every leaf and every composition family is single-output by construction, so
//! a recognized probe is single-output with no run-time check; what the run
//! time still declines is a RAISE anywhere in the graph — the naive predicate
//! evaluates the same expression once per child and raises in its own position.
//!
//! # Deliberate non-rows
//!
//! - **`INDEX(…)`/`GROUP_BY` idiom recognition.** A DIFFERENT rewrite:
//!   `INDEX/2` keeps the LAST duplicate and stringifies its keys, so `1` and
//!   `"1"` collide there and do not here. The naive form keys on `==`, which is
//!   `total_cmp == Equal`, and the index is a MULTIMAP. Never conflate them.
//! - **The `all/2` spelling is row A1**: the negated containment
//!   `all($o[]; .k != probe)` answers from the same index. The row is the
//!   `select` call whose predicate is the
//!   prelude expansion `isempty($o[] | (cond and empty))` — see
//!   [`anti_joins`]. The `not (==)` spelling (a `not` call around an equality)
//!   and `any/2` (the semi-join) are NOT rows.
//! - **An OWNED source.** Identity needs a `NodeHandle`; an owned container has
//!   none. Enforced at run time, where the representation is known.

use alloc::vec::Vec;

use crate::program::{BinaryKind, LogicalOp, ProgramNode, ProgramNodeId, StageStart, StageStep, StepAccess};
use jqf_builtins::registry::builtins::id;

/// One recognized correlated scan, addressed entirely by arena node id.
///
/// Every field is a node the executor re-reads from the arena when it runs, so
/// the table carries no copy of the program and costs four `u32`s per row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CorrelatedScan {
    scan: ProgramNodeId,
    source: ProgramNodeId,
    key_stage: ProgramNodeId,
    probe: ProgramNodeId,
}

impl CorrelatedScan {
    /// The `FlatMap` node the executor intercepts — the scan itself.
    pub const fn scan(self) -> ProgramNodeId {
        self.scan
    }

    /// The source `Stage` whose `.[]` the index replaces.
    pub const fn source(self) -> ProgramNodeId {
        self.source
    }

    /// The INNER key path (`Stage{Current, [Key/Index…]}`), navigated over every
    /// child at build time.
    pub const fn key_stage(self) -> ProgramNodeId {
        self.key_stage
    }

    /// The element-INDEPENDENT side of the equality, evaluated once per probe.
    pub const fn probe(self) -> ProgramNodeId {
        self.probe
    }
}

/// One recognized ANTI-join scan: a `select` whose predicate is the
/// prelude `all/2` expansion `isempty($o[] | (cond and empty))` with a NEGATED
/// equality inside — "no inner element matches" — answered by probing the same
/// sorted keyed index the equi-join builds, for the COMPLEMENT.
///
/// The scan is the SELECT call node itself, not a `FlatMap`: the outer source
/// iteration is untouched (every element flows into the select in order), and
/// only the PREDICATE evaluation is replaced — the element is dropped when the
/// index holds a key equal to the probe (the naive `isempty` is false), and
/// routed to the selection's out-continuation when it does not (the naive
/// `isempty` is true). That is exactly the complement of the equi-join's
/// `IndexedEach`, and it needs no separate semi-join builder: the index's
/// `total_cmp` equivalence IS the `==` law, and the negated form asks the same
/// index an existence question.
///
/// Every field is a node the executor re-reads from the arena when it runs, so
/// the table carries no copy of the program and costs four `u32`s per row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AntiJoinScan {
    /// The `select` call node — the interception point.
    select: ProgramNodeId,
    /// The inner generator's source stage: `Stage{Variable(_), [static…, Each]}`,
    /// the BOUND container the index is built over (an outer-bound container,
    /// exactly like the equi-join's bound-source row — a `Current`-start
    /// generator would read the scan element's own members, a per-element
    /// container that would rebuild the index once per element, and is
    /// declined).
    container_stage: ProgramNodeId,
    /// The INNER key path (`Stage{Current, [Key/Index…]}`), navigated over every
    /// child at build time.
    key_stage: ProgramNodeId,
    /// The element-INDEPENDENT side of the negated equality (K1/K2/K3),
    /// evaluated once per scan element.
    probe: ProgramNodeId,
}

impl AntiJoinScan {
    /// The `select` call the executor intercepts.
    pub const fn select(self) -> ProgramNodeId {
        self.select
    }

    /// The inner generator's source stage (the bound container).
    pub const fn container_stage(self) -> ProgramNodeId {
        self.container_stage
    }

    /// The INNER key path, navigated over every child at build time.
    pub const fn key_stage(self) -> ProgramNodeId {
        self.key_stage
    }

    /// The element-independent side of the negated equality.
    pub const fn probe(self) -> ProgramNodeId {
        self.probe
    }
}

/// Every anti-join `select` in `nodes`, in arena order.
///
/// An allocation failure yields an EMPTY table rather than an error: the table
/// is a pure optimization, and a program the naive scan can run must never fail
/// because the optimizer could not allocate.
pub fn anti_joins(nodes: &[ProgramNode]) -> Vec<AntiJoinScan> {
    let mut found = Vec::new();
    for index in 0..nodes.len() {
        let Some(id) = ProgramNodeId::from_index(index) else {
            break;
        };
        // The anti select sits in a scan's body (through the optional bind),
        // so the row is discovered from its enclosing `FlatMap`.
        if !matches!(nodes[index], ProgramNode::FlatMap { .. }) {
            continue;
        }
        let Some(scan) = anti_row(nodes, id) else {
            continue;
        };
        if found.try_reserve(1).is_err() {
            return Vec::new();
        }
        found.push(scan);
    }
    found
}

/// Row A1: `select(P)` where P is the prelude `all/2` expansion over a NEGATED
/// equality.
///
/// The prelude spells `isempty(g) := first((g|false), true)`, so the lowered
/// predicate is `Label{ FlatMap{ Choice{ (GENERATOR | false), true },
/// Choice{ Current, Break } } }` — `first`'s label/break machinery. The walk
/// matches that spine exactly:
///
/// ```text
/// Label{body}
///   body = FlatMap{ upstream: Choice{ left, right }, .. }
///     right = the literal `true`
///     left  = FlatMap{ GENERATOR, the literal `false` }
///     GENERATOR = FlatMap{ Stage{Variable(_), [static…, Each]},
///                          Logical{And, Binary{NotEqual, keypath, probe}, Empty} }
/// ```
///
/// The label/break linkage is NOT verified beyond the shapes: the first
/// output of the upstream Choice is the answer (`false` if the generator
/// emitted anything, `true` if it emitted nothing), and the runtime
/// replacement answers the same question from the index.
/// The `select` call a scan body reaches through its (optional) binding
/// frames: `. as $x | select(…)` lowers to `Bind{source, slot, body}` between
/// the `FlatMap` body and the call. The bind is TRANSPARENT for the anti route
/// (the probe reads the bound slot, evaluated per scan element), so the walk
/// descends through any number of them.
fn anti_select_through_binds(nodes: &[ProgramNode], mut node: ProgramNodeId) -> Option<ProgramNodeId> {
    loop {
        match &nodes[node.index()] {
            // `. as $x | select(…)`: the bind is transparent — the probe
            // reads the bound slot, evaluated per scan element.
            ProgramNode::Bind { body, .. } => node = *body,
            // `select(…) | .k`: the scan's trailing pipe puts the select in
            // the UPSTREAM — the downstream transforms each emitted element
            // and runs after the interception either way.
            ProgramNode::FlatMap { upstream, .. } => node = *upstream,
            _ => return select_predicate(nodes, node).map(|_| node),
        }
    }
}

fn anti_row(nodes: &[ProgramNode], id: ProgramNodeId) -> Option<AntiJoinScan> {
    let ProgramNode::FlatMap { body, .. } = &nodes[id.index()] else {
        return None;
    };
    let select = anti_select_through_binds(nodes, *body)?;
    let predicate = select_predicate(nodes, select)?;
    // The `first/1` expansion's label wraps the whole body.
    let ProgramNode::Label { body, .. } = &nodes[predicate.index()] else {
        return None;
    };
    let ProgramNode::FlatMap { upstream, .. } = &nodes[body.index()] else {
        return None;
    };
    let ProgramNode::Choice { left, right } = &nodes[upstream.index()] else {
        return None;
    };
    if !is_literal_true(nodes, *right) {
        return None;
    }
    // `(g | false)`: the generator piped into the literal `false`.
    let ProgramNode::FlatMap {
        upstream: generator,
        body: piped_false,
    } = &nodes[left.index()]
    else {
        return None;
    };
    if !is_literal_false(nodes, *piped_false) {
        return None;
    }
    // The generator: the bound container iterated, with the negated equality
    // short-circuited by `and empty`.
    let ProgramNode::FlatMap {
        upstream: container_stage,
        body: generator_body,
    } = &nodes[generator.index()]
    else {
        return None;
    };
    let ProgramNode::Stage {
        start: StageStart::Variable(_),
        steps,
    } = &nodes[container_stage.index()]
    else {
        return None;
    };
    if !is_iterating_path(steps) {
        return None;
    }
    let ProgramNode::Logical {
        operator: LogicalOp::And,
        left: cond,
        right: empty,
    } = &nodes[generator_body.index()]
    else {
        return None;
    };
    if !matches!(nodes[empty.index()], ProgramNode::Empty) {
        return None;
    }
    let ProgramNode::Binary {
        op: BinaryKind::NotEqual,
        left: a,
        right: b,
        ..
    } = &nodes[cond.index()]
    else {
        return None;
    };
    let (key_stage, probe) = match (is_key_path(nodes, *a), is_probe(nodes, *b)) {
        (true, true) => (*a, *b),
        _ => match (is_key_path(nodes, *b), is_probe(nodes, *a)) {
            (true, true) => (*b, *a),
            _ => return None,
        },
    };
    Some(AntiJoinScan {
        select,
        container_stage: *container_stage,
        key_stage,
        probe,
    })
}

/// Whether `id` is the bare literal `true` (`Stage{Literal(Bool(true)), []}`).
fn is_literal_true(nodes: &[ProgramNode], id: ProgramNodeId) -> bool {
    matches!(
        &nodes[id.index()],
        ProgramNode::Stage {
            start: StageStart::Literal(jqf_data::Value::Bool(true)),
            steps,
        } if steps.is_empty()
    )
}

/// Whether `id` is the bare literal `false` (`Stage{Literal(Bool(false)), []}`).
fn is_literal_false(nodes: &[ProgramNode], id: ProgramNodeId) -> bool {
    matches!(
        &nodes[id.index()],
        ProgramNode::Stage {
            start: StageStart::Literal(jqf_data::Value::Bool(false)),
            steps,
        } if steps.is_empty()
    )
}

/// Every correlated scan in `nodes`, in arena order.
///
/// An allocation failure yields an EMPTY table rather than an error: the table
/// is a pure optimization, and a program the naive scan can run must never fail
/// because the optimizer could not allocate.
pub fn correlated_scans(nodes: &[ProgramNode]) -> Vec<CorrelatedScan> {
    let mut found = Vec::new();
    for index in 0..nodes.len() {
        let Some(id) = ProgramNodeId::from_index(index) else {
            break;
        };
        let Some(scan) = source_row(nodes, id) else {
            continue;
        };
        if found.try_reserve(1).is_err() {
            return Vec::new();
        }
        found.push(scan);
    }
    found
}

/// Row S1: `FlatMap{ Stage{Variable(S) | Current, [static…, Each]}, Call(SELECT, [P]) }`.
///
/// One row covers the bound-slot spelling (`$o[].user | select(…)`), the
/// `map` spelling's inner scan (`[.[] | select(P)]`-body, whose source is a
/// `Current` stage with an empty prefix), the split-form container path
/// (`$o | .users[] | select(…)`, a `Current` stage with a static prefix the
/// executor descends over the scan's input), and the bare root `.[] | select`
/// — the last recognized and harmless, because the runtime warm-up keys the
/// index slot on the container's node handle and a once-seen container never
/// builds an index it cannot amortize.
fn source_row(nodes: &[ProgramNode], id: ProgramNodeId) -> Option<CorrelatedScan> {
    let ProgramNode::FlatMap { upstream, body } = &nodes[id.index()] else {
        return None;
    };
    let ProgramNode::Stage { start, steps } = &nodes[upstream.index()] else {
        return None;
    };
    if !matches!(start, StageStart::Variable(_) | StageStart::Current) {
        return None;
    }
    if !is_iterating_path(steps) {
        return None;
    }
    let predicate = select_predicate(nodes, *body)?;
    let (key_stage, probe) = equality_probe(nodes, predicate)?;
    Some(CorrelatedScan {
        scan: id,
        source: *upstream,
        key_stage,
        probe,
    })
}

/// The predicate graph of a one-argument `select` call, or `None` for any other
/// node.
///
/// The match is ID-ONLY, but the row's assumptions (the predicate passes its
/// input through per output) are pinned to ONE semantic revision of
/// `select/1`: a registry bump under a new revision fires the debug assert
/// instead of leaving the row firing on stale assumptions until a divergence
/// catches it.
#[cfg_attr(
    not(debug_assertions),
    allow(unused_variables, reason = "the revision pin is a debug-only assert")
)]
fn select_predicate(nodes: &[ProgramNode], id: ProgramNodeId) -> Option<ProgramNodeId> {
    let ProgramNode::Call {
        overload,
        revision,
        args,
        ..
    } = &nodes[id.index()]
    else {
        return None;
    };
    if overload.get() != id::SELECT {
        return None;
    }
    #[cfg(debug_assertions)]
    {
        let record = jqf_builtins::registry::resolve_builtin("select", 1)
            .expect("the recognizer matched an unregistered overload");
        assert_eq!(
            *revision, record.semantic_revision,
            "select semantics moved under the join recognizer"
        );
    }
    Some(args.first().copied())?
}

/// Row 1: the LEFTMOST top-level conjunct read as `(inner key path, probe)`.
///
/// The walk descends `Logical{And}` LEFT edges only — `a and b and c` lowers
/// left-associatively, so the leftmost conjunct is the deepest left leaf, and it
/// is the only conjunct every emitted child provably satisfies.
fn equality_probe(nodes: &[ProgramNode], predicate: ProgramNodeId) -> Option<(ProgramNodeId, ProgramNodeId)> {
    let mut node = predicate;
    loop {
        match &nodes[node.index()] {
            ProgramNode::Logical {
                operator: LogicalOp::And,
                left,
                ..
            } => node = *left,
            ProgramNode::Binary {
                op: BinaryKind::Equal,
                left,
                right,
                ..
            } => {
                let (left, right) = (*left, *right);
                return match (is_key_path(nodes, left), is_key_path(nodes, right)) {
                    (true, _) if is_probe(nodes, right) => Some((left, right)),
                    (_, true) if is_probe(nodes, left) => Some((right, left)),
                    _ => None,
                };
            }
            _ => return None,
        }
    }
}

/// Whether a `Binary`'s operands are this table's PROBE EQUALITY — the leaf
/// [`equality_probe`] recognizes, read from outside the walk.
///
/// [`super::demand`]'s spine hoist asks before it rewrites a `Binary`, because
/// the two see the same node and want opposite things from it: the hoist would
/// move a shared static prefix out of the operands, and this table needs the key
/// path and the probe to stay whole and adjacent under the `Binary`. Hoisting
/// `select(.id == 3)` into `.id | (. == 3)` costs the K2 row its recognition and
/// returns the scan to Θ(k·m), which is a far larger loss than a one-step
/// pushdown is a win.
///
/// Only `Equal` is asked about: no other operator reaches [`equality_probe`]'s
/// leaf, so no other operator can lose a row. The caller passes its own `op` so
/// the operator that carries this law stays named HERE, in the table that owns
/// it.
pub fn is_probe_equality(nodes: &[ProgramNode], op: BinaryKind, left: ProgramNodeId, right: ProgramNodeId) -> bool {
    op == BinaryKind::Equal
        && ((is_key_path(nodes, left) && is_probe(nodes, right))
            || (is_key_path(nodes, right) && is_probe(nodes, left)))
}

/// Whether `id` is the INNER key path `Stage{Current, [Key/Index…]}` — at least
/// one step, every one static and non-optional.
fn is_key_path(nodes: &[ProgramNode], id: ProgramNodeId) -> bool {
    matches!(
        &nodes[id.index()],
        ProgramNode::Stage {
            start: StageStart::Current,
            steps,
        } if !steps.is_empty() && steps.iter().all(is_static_step)
    )
}

/// Whether `id` is a PROBE row — K1 (`$t.a.b`), K2 (a bare literal), or K3
/// (the two composed through `Binary`/`Logical`/`Conditional`).
fn is_probe(nodes: &[ProgramNode], id: ProgramNodeId) -> bool {
    probe_shape(nodes, id, MAX_PROBE_DEPTH)
}

/// The most graph nodes one probe recognition walks.
///
/// The walk is over a compile-time arena and is structurally recursive, so this
/// is a depth-independent belt: a probe deeper than any row could plausibly be
/// DECLINES rather than recursing without bound.
const MAX_PROBE_DEPTH: usize = 32;

/// Whether `id` is a row of the probe grammar: K1/K2 leaves, or a `Binary`/
/// `Logical`/`Conditional` composition whose every child is itself a row.
///
/// A `Current`-start stage is the whole point of the exclusion: it reads the
/// current element, so it is the key path's side of the equality, never the
/// probe's. Every other node family (a `Call`, `Try`, `Choice`, a constructor,
/// a binder) is not a row, and the declining default is what makes the
/// omission safe rather than silent.
fn probe_shape(nodes: &[ProgramNode], id: ProgramNodeId, fuel: usize) -> bool {
    let Some(fuel) = fuel.checked_sub(1) else {
        return false;
    };
    match &nodes[id.index()] {
        ProgramNode::Stage { start, steps } => match start {
            StageStart::Variable(_) => steps.iter().all(is_static_step),
            StageStart::Literal(_) => steps.is_empty(),
            StageStart::Current => false,
        },
        ProgramNode::Binary { left, right, .. } | ProgramNode::Logical { left, right, .. } => {
            probe_shape(nodes, *left, fuel) && probe_shape(nodes, *right, fuel)
        }
        ProgramNode::Conditional {
            condition,
            consequent,
            alternative,
        } => {
            probe_shape(nodes, *condition, fuel)
                && probe_shape(nodes, *consequent, fuel)
                && probe_shape(nodes, *alternative, fuel)
        }
        _ => false,
    }
}

/// A static, non-optional `Key`/`Index` step.
///
/// The `?` is refused rather than tolerated: it can turn a per-child navigation
/// from a raise into ZERO outputs, and a zero-output operand changes the
/// `Binary`'s cardinality — an outcome the index has no cell for.
fn is_static_step(step: &StageStep) -> bool {
    !step.is_optional() && matches!(step.access(), StepAccess::Key(_) | StepAccess::Index(_))
}

/// Whether a stage's steps are a static path ending in the iteration: `[Key or
/// Index …, Each]` with the `.[]` LAST and carrying no `?` of its own.
fn is_iterating_path(steps: &[StageStep]) -> bool {
    let Some((last, prefix)) = steps.split_last() else {
        return false;
    };
    !last.is_optional() && matches!(last.access(), StepAccess::Each) && prefix.iter().all(is_static_step)
}

#[cfg(test)]
mod tests {
    use super::{AntiJoinScan, CorrelatedScan, anti_joins, correlated_scans};
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;

    use crate::program::{BinaryKind, LogicalOp, ProgramNode, ProgramNodeId, StageStart, StageStep, StepAccess};
    use jqf_builtins::registry::{BuiltinOverloadId, SemanticRevision};

    fn nid(index: usize) -> ProgramNodeId {
        ProgramNodeId::from_index(index).expect("node id")
    }

    fn key(name: &str) -> StageStep {
        StageStep::new(StepAccess::Key(String::from(name)), false)
    }

    fn optional_key(name: &str) -> StageStep {
        StageStep::new(StepAccess::Key(String::from(name)), true)
    }

    fn each() -> StageStep {
        StageStep::new(StepAccess::Each, false)
    }

    fn select(predicate: ProgramNodeId) -> ProgramNode {
        ProgramNode::call(
            BuiltinOverloadId::new(jqf_builtins::registry::builtins::id::SELECT),
            SemanticRevision::new(1),
            vec![predicate],
        )
    }

    /// The row S1 arena: `$o[] | select(.user_id == $u.id)`.
    ///
    /// 0 source, 1 key path, 2 probe, 3 equality, 4 select, 5 scan.
    fn bound_source_arena(key_steps: Vec<StageStep>, probe: ProgramNode) -> Vec<ProgramNode> {
        vec![
            ProgramNode::Stage {
                start: StageStart::Variable(0),
                steps: vec![each()],
            },
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: key_steps,
            },
            probe,
            ProgramNode::Binary {
                op: BinaryKind::Equal,
                left: nid(1),
                right: nid(2),
                shape: crate::program::BinaryShape::Framed,
            },
            select(nid(3)),
            ProgramNode::FlatMap {
                upstream: nid(0),
                body: nid(4),
            },
        ]
    }

    fn bound_probe() -> ProgramNode {
        ProgramNode::Stage {
            start: StageStart::Variable(1),
            steps: vec![key("id")],
        }
    }

    #[test]
    fn row_s1_recognizes_the_bound_source_scan() {
        let nodes = bound_source_arena(vec![key("user_id")], bound_probe());
        assert_eq!(
            correlated_scans(&nodes),
            vec![CorrelatedScan {
                scan: nid(5),
                source: nid(0),
                key_stage: nid(1),
                probe: nid(2),
            }]
        );
    }

    #[test]
    fn row_s1_reads_the_equality_in_either_operand_order() {
        // `select($u.id == .user_id)` names the same index; the table reads the
        // OPERANDS, never their authored order.
        let mut nodes = bound_source_arena(vec![key("user_id")], bound_probe());
        nodes[3] = ProgramNode::Binary {
            op: BinaryKind::Equal,
            left: nid(2),
            right: nid(1),
            shape: crate::program::BinaryShape::Framed,
        };
        let scans = correlated_scans(&nodes);
        assert_eq!(scans.len(), 1);
        assert_eq!(scans[0].key_stage(), nid(1));
        assert_eq!(scans[0].probe(), nid(2));
    }

    #[test]
    fn only_the_leftmost_conjunct_is_read() {
        // `select(.a == $x and .b == $y)`: the index is the LEFT conjunct's, and
        // the right conjunct stays the naive predicate's business.
        let mut nodes = bound_source_arena(vec![key("a")], bound_probe());
        nodes.push(ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("b")],
        });
        nodes.push(ProgramNode::Stage {
            start: StageStart::Variable(2),
            steps: vec![key("y")],
        });
        nodes.push(ProgramNode::Binary {
            op: BinaryKind::Equal,
            left: nid(6),
            right: nid(7),
            shape: crate::program::BinaryShape::Framed,
        });
        nodes.push(ProgramNode::Logical {
            operator: LogicalOp::And,
            left: nid(3),
            right: nid(8),
        });
        nodes[4] = select(nid(9));
        let scans = correlated_scans(&nodes);
        assert_eq!(scans.len(), 1);
        assert_eq!(scans[0].key_stage(), nid(1), "the LEFT conjunct's key path");
    }

    #[test]
    fn a_right_hand_conjunct_is_never_indexed() {
        // The mirror of the row above: with the indexable equality on the RIGHT
        // of the `and`, the table declines rather than reordering the predicate.
        // Reordering would run the right conjunct's errors on children the left
        // conjunct rejects.
        let mut nodes = bound_source_arena(vec![key("a")], bound_probe());
        nodes.push(ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("b")],
        });
        nodes.push(ProgramNode::call(
            BuiltinOverloadId::new(jqf_builtins::registry::builtins::id::LENGTH),
            SemanticRevision::new(1),
            Vec::new(),
        ));
        nodes.push(ProgramNode::FlatMap {
            upstream: nid(6),
            body: nid(7),
        });
        nodes.push(ProgramNode::Logical {
            operator: LogicalOp::And,
            left: nid(8),
            right: nid(3),
        });
        nodes[4] = select(nid(9));
        assert!(correlated_scans(&nodes).is_empty());
    }

    #[test]
    fn an_or_predicate_is_not_a_conjunction() {
        let mut nodes = bound_source_arena(vec![key("a")], bound_probe());
        nodes.push(ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("b")],
        });
        nodes.push(ProgramNode::Logical {
            operator: LogicalOp::Or,
            left: nid(3),
            right: nid(6),
        });
        nodes[4] = select(nid(7));
        assert!(correlated_scans(&nodes).is_empty());
    }

    #[test]
    fn a_non_equality_comparison_is_not_a_probe() {
        let mut nodes = bound_source_arena(vec![key("a")], bound_probe());
        nodes[3] = ProgramNode::Binary {
            op: BinaryKind::Less,
            left: nid(1),
            right: nid(2),
            shape: crate::program::BinaryShape::Framed,
        };
        assert!(correlated_scans(&nodes).is_empty());
    }

    #[test]
    fn an_optional_step_declines_on_either_side() {
        let key_optional = bound_source_arena(vec![optional_key("user_id")], bound_probe());
        assert!(correlated_scans(&key_optional).is_empty());
        let probe_optional = bound_source_arena(
            vec![key("user_id")],
            ProgramNode::Stage {
                start: StageStart::Variable(1),
                steps: vec![optional_key("id")],
            },
        );
        assert!(correlated_scans(&probe_optional).is_empty());
    }

    #[test]
    fn a_key_path_that_reads_the_current_element_is_not_a_probe() {
        // `select(.a == .b)` correlates the element with ITSELF: both sides move
        // per child, so no single probe value exists.
        let nodes = bound_source_arena(
            vec![key("a")],
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: vec![key("b")],
            },
        );
        assert!(correlated_scans(&nodes).is_empty());
    }

    #[test]
    fn a_literal_probe_is_a_row() {
        let nodes = bound_source_arena(
            vec![key("channel")],
            ProgramNode::Stage {
                start: StageStart::Literal(jqf_data::Value::Bool(true)),
                steps: Vec::new(),
            },
        );
        assert_eq!(correlated_scans(&nodes).len(), 1);
    }

    #[test]
    fn a_composed_probe_is_a_row() {
        // Row K3: `select(.id == $t.a + $t.b)` — K1 leaves composed through a
        // `Binary`, evaluated by the engine's own operator application.
        // 0 source, 1 key path, 2 left probe ($t.a), 3 right probe ($t.b),
        // 4 the composed probe, 5 equality, 6 select, 7 scan.
        let nodes = vec![
            ProgramNode::Stage {
                start: StageStart::Variable(0),
                steps: vec![each()],
            },
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: vec![key("id")],
            },
            ProgramNode::Stage {
                start: StageStart::Variable(1),
                steps: vec![key("a")],
            },
            ProgramNode::Stage {
                start: StageStart::Variable(1),
                steps: vec![key("b")],
            },
            ProgramNode::Binary {
                op: BinaryKind::Add,
                left: nid(2),
                right: nid(3),
                shape: crate::program::BinaryShape::Framed,
            },
            ProgramNode::Binary {
                op: BinaryKind::Equal,
                left: nid(1),
                right: nid(4),
                shape: crate::program::BinaryShape::Framed,
            },
            select(nid(5)),
            ProgramNode::FlatMap {
                upstream: nid(0),
                body: nid(6),
            },
        ];
        assert_eq!(
            correlated_scans(&nodes),
            vec![CorrelatedScan {
                scan: nid(7),
                source: nid(0),
                key_stage: nid(1),
                probe: nid(4),
            }]
        );
    }

    #[test]
    fn a_conditional_probe_is_a_row() {
        // Row K3 through `Conditional`: `select(.id == (if $t.active then
        // $t.a else $t.b end))`.
        // 0 source, 1 key path, 2 condition ($t.active), 3 consequent ($t.a),
        // 4 alternative ($t.b), 5 conditional probe, 6 equality, 7 select, 8 scan.
        let nodes = vec![
            ProgramNode::Stage {
                start: StageStart::Variable(0),
                steps: vec![each()],
            },
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: vec![key("id")],
            },
            ProgramNode::Stage {
                start: StageStart::Variable(1),
                steps: vec![key("active")],
            },
            ProgramNode::Stage {
                start: StageStart::Variable(1),
                steps: vec![key("a")],
            },
            ProgramNode::Stage {
                start: StageStart::Variable(1),
                steps: vec![key("b")],
            },
            ProgramNode::Conditional {
                condition: nid(2),
                consequent: nid(3),
                alternative: nid(4),
            },
            ProgramNode::Binary {
                op: BinaryKind::Equal,
                left: nid(1),
                right: nid(5),
                shape: crate::program::BinaryShape::Framed,
            },
            select(nid(6)),
            ProgramNode::FlatMap {
                upstream: nid(0),
                body: nid(7),
            },
        ];
        assert_eq!(correlated_scans(&nodes).len(), 1);
    }

    #[test]
    fn a_probe_that_reads_the_element_anywhere_declines() {
        // A `Current` stage NESTED inside the composition still reads the
        // element: `select(.id == $t.a + .b)` is not a row.
        // 0 source, 1 key path, 2 left probe ($t.a), 3 element-read (.b),
        // 4 composed probe, 5 equality, 6 select, 7 scan.
        let nodes = vec![
            ProgramNode::Stage {
                start: StageStart::Variable(0),
                steps: vec![each()],
            },
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: vec![key("id")],
            },
            ProgramNode::Stage {
                start: StageStart::Variable(1),
                steps: vec![key("a")],
            },
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: vec![key("b")],
            },
            ProgramNode::Binary {
                op: BinaryKind::Add,
                left: nid(2),
                right: nid(3),
                shape: crate::program::BinaryShape::Framed,
            },
            ProgramNode::Binary {
                op: BinaryKind::Equal,
                left: nid(1),
                right: nid(4),
                shape: crate::program::BinaryShape::Framed,
            },
            select(nid(5)),
            ProgramNode::FlatMap {
                upstream: nid(0),
                body: nid(6),
            },
        ];
        assert!(correlated_scans(&nodes).is_empty());
    }

    #[test]
    fn a_probe_with_a_call_inside_declines() {
        // `Call` is not a row of the probe grammar: `select(.id == ($t.a |
        // length))` declines, and the naive predicate evaluates it per child.
        let mut nodes = bound_source_arena(vec![key("id")], bound_probe());
        nodes[2] = ProgramNode::call(
            BuiltinOverloadId::new(jqf_builtins::registry::builtins::id::LENGTH),
            SemanticRevision::new(1),
            Vec::new(),
        );
        assert!(correlated_scans(&nodes).is_empty());
    }

    #[test]
    fn a_residual_after_the_iteration_declines() {
        // `$o[].line | select(.a == $x)`: the value reaching `select` is not the
        // child, so the index's key path is not the predicate's.
        let mut nodes = bound_source_arena(vec![key("a")], bound_probe());
        nodes[0] = ProgramNode::Stage {
            start: StageStart::Variable(0),
            steps: vec![each(), key("line")],
        };
        assert!(correlated_scans(&nodes).is_empty());
    }

    #[test]
    fn a_current_start_source_is_a_row() {
        // The generalized source row: `.[] | select(.a == $x)` and its
        // split-form sibling `$o | .users[] | select(…)` are recognized; the
        // runtime warm-up keys the index slot on the container's node handle,
        // so a scan whose container never repeats (a root document, seen once)
        // never builds an index it cannot amortize — the recorded S2
        // context argument now lives in the warm-up, not in the row.
        let mut nodes = bound_source_arena(vec![key("a")], bound_probe());
        nodes[0] = ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![each()],
        };
        let scans = correlated_scans(&nodes);
        assert_eq!(scans.len(), 1);
        assert_eq!(scans[0].source(), nid(0));
    }

    #[test]
    fn a_split_form_container_path_is_a_row() {
        // `$o | .users[] | select(.a == $x)`: the source stage is `Current`
        // with a static PREFIX before the `.[]`, which the executor descends
        // over the scan's input to find the container. Previously declined —
        // the exact Θ(k·m) shape the correlated-join vertical exists for.
        let mut nodes = bound_source_arena(vec![key("a")], bound_probe());
        nodes[0] = ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("users"), each()],
        };
        let scans = correlated_scans(&nodes);
        assert_eq!(scans.len(), 1);
        assert_eq!(scans[0].source(), nid(0));
    }

    /// The row S2 arena: `$o | map(select(.user_id == $u.id))`.
    ///
    /// 0 bound upstream, 1 inner source, 2 key path, 3 probe, 4 equality,
    /// 5 select, 6 inner scan, 7 collect, 8 enclosing flat map.
    fn map_spelling_arena() -> Vec<ProgramNode> {
        vec![
            ProgramNode::Stage {
                start: StageStart::Variable(0),
                steps: Vec::new(),
            },
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: vec![each()],
            },
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: vec![key("user_id")],
            },
            bound_probe(),
            ProgramNode::Binary {
                op: BinaryKind::Equal,
                left: nid(2),
                right: nid(3),
                shape: crate::program::BinaryShape::Framed,
            },
            select(nid(4)),
            ProgramNode::FlatMap {
                upstream: nid(1),
                body: nid(5),
            },
            ProgramNode::CollectArray { body: Some(nid(6)) },
            ProgramNode::FlatMap {
                upstream: nid(0),
                body: nid(7),
            },
        ]
    }

    #[test]
    fn row_s2_reports_the_inner_scan_the_executor_intercepts() {
        // The map spelling's INNER scan (`[.[] | select(P)]`-body) is itself a
        // `Current`-start source row now — the collect barrier is looked
        // through without an enclosing-context row of its own.
        assert_eq!(
            correlated_scans(&map_spelling_arena()),
            vec![CorrelatedScan {
                scan: nid(6),
                source: nid(1),
                key_stage: nid(2),
                probe: nid(3),
            }]
        );
    }

    #[test]
    fn row_s2_without_the_bound_upstream_is_still_a_row() {
        // `map(select(.a == $x))` over whatever dot happens to be: the inner
        // scan is recognized regardless of the enclosing producer, and the
        // runtime warm-up declines the index for a container that never
        // repeats. Soundness does not depend on the context.
        let mut nodes = map_spelling_arena();
        nodes[0] = ProgramNode::Stage {
            start: StageStart::Current,
            steps: Vec::new(),
        };
        let scans = correlated_scans(&nodes);
        assert_eq!(scans.len(), 1);
        assert_eq!(scans[0].scan(), nid(6));
    }

    #[test]
    fn a_prefix_inside_the_map_spelling_is_a_row() {
        // `$o | map(.users[] | select(.a == $x))`: the map body's source stage
        // carries its own static prefix before the `.[]`, which the executor
        // descends over the bound container — the map spelling's twin of the
        // split-form row above.
        let mut nodes = map_spelling_arena();
        nodes[1] = ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("users"), each()],
        };
        let scans = correlated_scans(&nodes);
        assert_eq!(scans.len(), 1);
        assert_eq!(scans[0].source(), nid(1));
    }

    #[test]
    fn two_scans_over_one_program_get_two_rows() {
        // `complex_merged_user_activity`'s shape: two independent inner scans,
        // each its own row and later its own index slot.
        let mut nodes = bound_source_arena(vec![key("user_id")], bound_probe());
        let base = nodes.len();
        nodes.push(ProgramNode::Stage {
            start: StageStart::Variable(2),
            steps: vec![each()],
        });
        nodes.push(ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("user_id")],
        });
        nodes.push(bound_probe());
        nodes.push(ProgramNode::Binary {
            op: BinaryKind::Equal,
            left: nid(base + 1),
            right: nid(base + 2),
            shape: crate::program::BinaryShape::Framed,
        });
        nodes.push(select(nid(base + 3)));
        nodes.push(ProgramNode::FlatMap {
            upstream: nid(base),
            body: nid(base + 4),
        });
        let scans = correlated_scans(&nodes);
        assert_eq!(scans.len(), 2);
        assert_eq!(scans[0].scan(), nid(5));
        assert_eq!(scans[1].scan(), nid(base + 5));
    }

    /// The row A1 arena for the bound-container spelling:
    /// `select(all($o[]; .k != $x.k))`.
    ///
    /// 0 container stage, 1 key path, 2 probe, 3 not-equal, 4 empty,
    /// 5 and, 6 generator `FlatMap`, 7 false literal, 8 false-pipe `FlatMap`,
    /// 9 true literal, 10 choice, 11 label, 12 select.
    fn anti_arena() -> Vec<ProgramNode> {
        vec![
            ProgramNode::Stage {
                start: StageStart::Variable(0),
                steps: vec![each()],
            },
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: vec![key("k")],
            },
            ProgramNode::Stage {
                start: StageStart::Variable(1),
                steps: vec![key("k")],
            },
            ProgramNode::Binary {
                op: BinaryKind::NotEqual,
                left: nid(1),
                right: nid(2),
                shape: crate::program::BinaryShape::Framed,
            },
            ProgramNode::Empty,
            ProgramNode::Logical {
                operator: LogicalOp::And,
                left: nid(3),
                right: nid(4),
            },
            ProgramNode::FlatMap {
                upstream: nid(0),
                body: nid(5),
            },
            ProgramNode::Stage {
                start: StageStart::Literal(jqf_data::Value::Bool(false)),
                steps: Vec::new(),
            },
            ProgramNode::FlatMap {
                upstream: nid(6),
                body: nid(7),
            },
            ProgramNode::Stage {
                start: StageStart::Literal(jqf_data::Value::Bool(true)),
                steps: Vec::new(),
            },
            ProgramNode::Choice {
                left: nid(8),
                right: nid(9),
            },
            // The `first/1` expansion's FlatMap: the upstream Choice's FIRST
            // output flows through `., break $out` and ends the label.
            ProgramNode::FlatMap {
                upstream: nid(10),
                body: nid(12),
            },
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: Vec::new(),
            },
            ProgramNode::Break { slot: 0 },
            ProgramNode::Choice {
                left: nid(12),
                right: nid(13),
            },
            ProgramNode::Label {
                slot: 0u32,
                body: nid(11),
            },
            select(nid(15)),
            // The scan: `.u[] | . as $x | select(16)` — the FlatMap whose
            // body (through the bind) reaches the select.
            ProgramNode::FlatMap {
                upstream: nid(18),
                body: nid(19),
            },
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: vec![key("u"), each()],
            },
            ProgramNode::Bind {
                source: nid(20),
                slot: 0,
                body: nid(16),
                frame: false,
            },
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: Vec::new(),
            },
        ]
    }

    #[test]
    fn row_a1_recognizes_the_anti_join_select() {
        let nodes = anti_arena();
        assert_eq!(
            anti_joins(&nodes),
            vec![AntiJoinScan {
                select: nid(16),
                container_stage: nid(0),
                key_stage: nid(1),
                probe: nid(2),
            }]
        );
    }

    #[test]
    fn row_a1_declines_the_equality_and_any_forms() {
        // The equi-join's `==` inside the same expansion is NOT an anti row:
        // its answer is the semi-join, and the equi-join's own rows cover the
        // `select(==)` spellings.
        let mut nodes = anti_arena();
        nodes[3] = ProgramNode::Binary {
            op: BinaryKind::Equal,
            left: nid(1),
            right: nid(2),
            shape: crate::program::BinaryShape::Framed,
        };
        assert_eq!(anti_joins(&nodes), Vec::new());
        // A probe that reads the current element (a `Current`-start stage) is
        // not a probe row: it is the key path's side of the equality.
        let mut nodes = anti_arena();
        nodes[2] = ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("k")],
        };
        assert_eq!(anti_joins(&nodes), Vec::new());
        // An optional key path declines (the `?` changes the cardinality).
        let mut nodes = anti_arena();
        nodes[1] = ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![optional_key("k")],
        };
        assert_eq!(anti_joins(&nodes), Vec::new());
        // A `Current`-start GENERATOR (per-element container) declines: the
        // index would be rebuilt per element.
        let mut nodes = anti_arena();
        nodes[0] = ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![each()],
        };
        assert_eq!(anti_joins(&nodes), Vec::new());
    }

    /// The anti-join row probe on a REAL compiled program: the corpus spelling
    /// must reach the row through the whole pipeline, not just through a
    /// hand-built arena like the rows above.
    #[test]
    fn row_a1_recognizes_the_real_anti_join_spelling() {
        // The corpus spelling compiles to the row: the prelude `all/2`
        // expansion through `isempty`/`first`/`Label` is exactly the arena
        // above, and the bind `. as $x` is transparent to the row.
        let resources = jqf_resource::ResourceContext::new(
            jqf_resource::RequestAccount::try_new(jqf_resource::ResourceLimits::new(
                u64::MAX,
                u64::MAX,
                u64::MAX,
                u64::MAX,
                u32::MAX,
            ))
            .expect("account"),
            &jqf_resource::ContinueControl,
            jqf_resource::WorkMeter::try_new_v1(4096).expect("work"),
        )
        .expect("resources");
        let program = crate::compile::try_compile_program(
            ".o as $o | [.u[] | . as $x | select(all($o[]; .k != $x.k)) | .k]",
            crate::CodecRequirementPolicy::new(
                jqf_codec_core::ValidationMode::Strict,
                jqf_codec_core::DiagnosticPolicy::ErrorsOnly,
            ),
            &resources,
        )
        .expect("compiles");
        let rows = anti_joins(program.arena());
        assert_eq!(rows.len(), 1, "the anti spelling is one row");
        // The container is the bound `$o` stage and the key path is `.k`.
        let ProgramNode::Stage { start, .. } = &program.arena()[rows[0].container_stage().index()] else {
            panic!("container stage");
        };
        assert!(matches!(start, StageStart::Variable(_)));
        let ProgramNode::Stage { steps, .. } = &program.arena()[rows[0].key_stage().index()] else {
            panic!("key stage");
        };
        let steps = steps.as_slice();
        assert_eq!(steps.len(), 1);
        assert!(matches!(steps[0].access(), StepAccess::Key(k) if k == "k"));
        assert!(!steps[0].is_optional());
    }
}
