//! Fusion to path-normal form and the codec-pushdown split.
//!
//! [`fuse_with_callables`] rewrites a lowered arena — [`ProgramNode::Stage`]s (each with a
//! [`StageStart`]) joined by pipe / group-postfix [`ProgramNode::FlatMap`], comma
//! [`ProgramNode::Choice`], and the `CollectArray`/`ConstructObject` constructor
//! nodes — into **path-normal form**: every `FlatMap(Stage{s, p}, Stage{Current,
//! q})` collapses to `Stage{s, p ++ q}` (plain concatenation, each step's optional
//! flag travelling with it, the UPSTREAM start preserved), and no fusable
//! `Stage∘Stage` `FlatMap` survives. A `Choice`/constructor on either side, or a
//! `Literal`-start BODY (which ignores the upstream value, changing cardinality),
//! blocks that fusion, so those coupled `FlatMap` nodes and the `Choice`/
//! constructor nodes remain in the returned arena. Concatenation is byte-exact
//! across a pipe boundary even when the stages carry `Each` steps (the probe
//! `.[].a` ≡ `.[] | .a` holds for output order, error ordering, and suppression).
//!
//! One rewrite runs in the other direction: a `ConstructObject` whose member
//! producers all walk the same leading static steps HOISTS them onto an upstream
//! stage (`{a: .p.x, b: .p.y}` → `.p | {a: .x, b: .y}`), so the part of the
//! constructor's demand the codec can express reaches the pushdown split instead
//! of collapsing to whole-input authority. The recognition and its declining
//! default live on [`fuse_construct_object`].
//!
//! [`analyze`] then classifies the fused arena's maximal static forward prefix:
//! the leading static field/index steps of the ENTRY stage — the [`Stage`]
//! reached by descending the root through `FlatMap` upstream edges (never into a
//! `Choice`) — before its first `.[]` iteration ([`StepAccess::Each`]). That
//! prefix lowers to a codec [`jqf_codec_core::AccessRequirement`]; everything
//! from the first `Each` onward, plus the whole downstream graph, is the residual
//! the executor drives against the codec's outcome. The split records the entry
//! node and the prefix boundary as a step count: a zero-length prefix is
//! whole-document access (identity, a leading `.[]`, or a comma whose spine
//! reaches a `Choice`), and a non-empty prefix is a static forward path. It
//! preserves the whole-document-versus-exact-root distinction: identity must
//! lower through the root requirement, never a forward requirement with an empty
//! path.
//!
//! [`analyze`] performs no lowering itself and stores only the entry node and
//! prefix boundary (the residual is the graph reachable from the root with the
//! entry stage's leading steps consumed, so recording a separate residual arena
//! would duplicate the graph).
//!
//! # The named element boundary
//!
//! Beside the pushdown prefix, [`analyze`] NAMES the program's single element
//! boundary — the `.[]` whose elements a projected route would stream — in
//! (node, offset) coordinates ([`ElementBoundary`]). The naming walks an ELEMENT
//! SPINE that is wider than the pushdown spine: it enters a `CollectArray` body
//! (so `map`'s lowered `[.[] | f]` is nameable), a `Reduce`/`Foreach` SOURCE, and
//! a `Bind` SOURCE — the three places the pushdown prefix stops dead because the
//! enclosing family reads the whole input authority.
//!
//! The naming is PURELY ADDITIVE. It does not move the pushdown prefix, does not
//! widen the element-iteration lowering (which still keys off a bare-`Stage`
//! root and the pushdown prefix), and lowers no requirement.
//! Its consumers are the projection classifier — which records the per-element
//! demand at exactly that boundary when the program has a sole `.[]` — and the
//! projected-plan lowering, which is itself behind an eligibility check that
//! declines every program.
//!
//! [`Stage`]: ProgramNode::Stage

use alloc::vec::Vec;

use jqf_resource::ResourceError;

use crate::analysis::path_steps::{hoistable_collect_prefix, prepend_in_place};
use crate::program::{
    BinaryKind, LogicalOp, ObjectMemberNode, ProgramNode, ProgramNodeId, StageStart, StageStep, StepAccess,
};
use jqf_builtins::registry::builtins::id;

/// What consumes the elements a named `ElementBoundary` yields.
///
/// The boundary is named in (node, offset) coordinates; the consumer records
/// WHICH construct the element spine descended through to reach it, which is the
/// fact a projected route needs in order to know what it is feeding. It is a
/// classification, not a capability: today every variant declines eligibility.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BoundaryConsumer {
    /// The boundary stage's own remaining suffix, plus whatever graph is
    /// downstream of it — the shape the deleted element-stream route drove.
    Residual,
    /// A `reduce`/`foreach` loop whose SOURCE holds the boundary: the loop is the
    /// residual consumer of the elements — the projected fold's leading shape.
    Fold,
    /// A `SOURCE as $x | BODY` binder whose SOURCE holds the boundary.
    Binding,
    /// A `[…]` construction barrier — including `map`'s lowered `[.[] | f]` body,
    /// which is where the `map`-through boundary is named.
    Collect,
}

/// One named element boundary: the single `.[]` whose elements a projected route
/// would stream.
///
/// The boundary is a pair of (node, offset) coordinates rather than one index,
/// because the container path can be split across a `FlatMap` upstream prefix
/// and the boundary stage's own leading steps: `.catalog | map(.name)` names the
/// container `.catalog` in the upstream stage and finds the `.[]` at offset 0 of
/// the collect body's stage. Every step on the named container path is
/// codec-resolvable by construction: static keys and indices, plus at most ONE
/// trailing `.[a:b]` whose bounds normalize (a `..`/`.[$x]`, a non-normalizing
/// slice, or a second slice before the `.[]` declines the naming outright).
///
/// Naming a boundary changes NO routing: [`PushdownSplit::entry`] and
/// [`PushdownSplit::prefix_len`] are computed exactly as before, and
/// the element-iteration lowering still keys off them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ElementBoundary {
    stage: ProgramNodeId,
    stage_prefix_len: usize,
    consumer: BoundaryConsumer,
    sole: bool,
}

impl ElementBoundary {
    /// The [`ProgramNode::Stage`] whose steps hold the boundary `.[]`.
    pub const fn stage(self) -> ProgramNodeId {
        self.stage
    }

    /// How many of that stage's leading steps precede the boundary `.[]` (so the
    /// `.[]` itself sits at exactly this offset).
    pub const fn stage_prefix_len(self) -> usize {
        self.stage_prefix_len
    }

    /// What consumes the elements.
    pub const fn consumer(self) -> BoundaryConsumer {
        self.consumer
    }

    /// Whether this `.[]` is the program's ONLY one.
    ///
    /// Nested iteration (`[.catalog[] | .tags[]]`) keeps the conservative
    /// join: the outer element's demand is not separable from the inner one's
    /// without a per-boundary walk the naming does not build. A sole boundary
    /// licenses the exact per-boundary classification.
    pub const fn is_sole(self) -> bool {
        self.sole
    }
}

/// How a program splits into a codec-pushed prefix and an executor residual.
///
/// The prefix is the leading steps of the ENTRY stage (reached by descending the
/// root through `FlatMap` upstream edges) before its first `.[]`
/// ([`StepAccess::Each`]); the residual is the whole graph from the root with
/// those `prefix_len` entry-stage steps consumed. `entry` names that stage and
/// `prefix_len` is the boundary index; the pushed-down `?` flag lookup indexes
/// the entry stage's steps directly (residual step indices are diagnostic-only
/// and per-branch, so no global step table is threaded).
///
/// It additionally NAMES the program's element boundary when one exists
/// ([`Self::element_boundary`]). That naming reaches inside `Reduce`/`Foreach`
/// sources and through `map`'s lowered `[.[] | f]` body — places the pushdown
/// prefix itself never enters — and is purely additive: it selects no route,
/// lowers no requirement, and widens no route's eligibility.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PushdownSplit {
    entry: ProgramNodeId,
    prefix_len: usize,
    element: Option<ElementBoundary>,
}

impl PushdownSplit {
    /// The entry [`ProgramNode::Stage`] whose leading `prefix_len` steps the
    /// codec pushdown consumes. When the prefix is empty the spine may instead
    /// reach a `Choice` (a whole-document comma); the entry is then unused.
    pub const fn entry(&self) -> ProgramNodeId {
        self.entry
    }

    /// The number of leading static steps of the entry stage pushed to the codec.
    pub const fn prefix_len(&self) -> usize {
        self.prefix_len
    }

    /// Whether the pushed-down prefix is empty: identity, a leading `.[]`, or a
    /// comma whose spine reaches a `Choice` — all of which read the whole
    /// document.
    pub const fn is_whole_document(&self) -> bool {
        self.prefix_len == 0
    }

    /// The program's named element boundary, when it has one.
    pub const fn element_boundary(&self) -> Option<ElementBoundary> {
        self.element
    }
}

/// A partially fused node: either a loose stage (start + steps) not yet
/// materialized into the arena (so a parent `FlatMap` can concatenate a
/// [`StageStart::Current`] body onto it without a dead node) or an
/// already-materialized node id.
enum Fused {
    /// A loose stage awaiting concatenation or materialization. Only a
    /// `StageStart::Current` loose stage may be concatenated ONTO by a parent
    /// `FlatMap`; a `StageStart::Literal` one blocks that fusion.
    Stage {
        /// What the stage's steps start from.
        start: StageStart,
        /// The stage's steps.
        steps: Vec<StageStep>,
    },
    /// An already-placed graph node.
    Node(ProgramNodeId),
}

/// Which SOURCE-arena `Binary{Equal}` nodes are the LEFTMOST conjunct of a
/// `select` call's predicate — the only equalities the correlated-scan table
/// (`join::equality_probe`) can ever recognize, and therefore the only
/// ones whose spine hoist would cost a scan row.
///
/// The scan row's walk descends `Logical{And}` LEFT edges from the select
/// predicate and stops at the first `Binary{Equal}`. This marks exactly those
/// nodes; [`fuse_node`]'s `Binary` arm refuses the hoist for them (the hoist
/// would strip the key path out of the equality and return the scan to Θ(k·m)),
/// while every OTHER equality — a conditional condition, a logical/alternative/
/// binary operand, a top-level comparison — may hoist its shared prefix freely,
/// since no scan row can ever read it. That is the partial-prefix join:
/// `if .a.b == 1 then .a.c else .a.d end` joins `.a` exactly like its
/// `!=`/`<` siblings, because the `==` operand shares the prefix with the arms.
///
/// The SOURCE arena holds the lowered tree; fusion preserves select-call
/// structure (a scan-relevant select sits under a `FlatMap` whose own fusion
/// never rewrites it), so a source walk names the same equalities the fused
/// arena's scan walk reaches. An allocation failure PROPAGATES rather than
/// degrading silently: losing a protection would hoist a scan predicate and
/// return a correlated join to Θ(k·m) without any error to point at.
fn scan_protected_equalities(nodes: &[ProgramNode]) -> Result<Vec<bool>, ResourceError> {
    let mut protected = Vec::new();
    protected
        .try_reserve_exact(nodes.len())
        .map_err(|_| ResourceError::AllocationFailed)?;
    protected.resize(nodes.len(), false);
    for node in nodes {
        if let ProgramNode::Call { overload, args, .. } = node
            && overload.get() == id::SELECT
            && let Some(predicate) = args.first()
        {
            protect_leftmost_conjunct(nodes, *predicate, &mut protected);
        }
    }
    Ok(protected)
}

/// Marks the leftmost-conjunct equality of one select predicate, mirroring the
/// scan table's walk: `Logical{And}` LEFT edges only, stopping at the first
/// `Binary{Equal}`. Any other node family is a decline in the scan walk too, so
/// nothing past it can lose a row.
fn protect_leftmost_conjunct(nodes: &[ProgramNode], mut node: ProgramNodeId, protected: &mut [bool]) {
    loop {
        match &nodes[node.index()] {
            ProgramNode::Logical {
                operator: LogicalOp::And,
                left,
                ..
            } => node = *left,
            ProgramNode::Binary {
                op: BinaryKind::Equal, ..
            } => {
                protected[node.index()] = true;
                return;
            }
            _ => return,
        }
    }
}

/// The fused arena, its root, and the callable-index → fused-body-id map.
type FusedProgram = (
    Vec<ProgramNode>,
    ProgramNodeId,
    alloc::collections::BTreeMap<usize, ProgramNodeId>,
);

/// Fuses a lowered arena into path-normal form with recursive-definition bodies
/// resolved.
///
/// A bottom-up rewrite: `FlatMap(Stage, Stage)` concatenates into one `Stage`
/// (upstream steps before body steps, optional flags intact); a `FlatMap` with a
/// `Choice` on either side, and every `Choice`, is preserved with its children
/// fused. The returned arena holds no `Stage∘Stage` `FlatMap`. Recursion depth is
/// bounded by the query's syntactic nesting (compile-time, not evaluation).
///
/// Each callable's body is fused EXACTLY ONCE (memoized by callable index) and
/// appended to the output arena before the main root, so every call site — the
/// root's and the bodies' own — resolves to the same fused body id, and the
/// self-reference can never re-enter the body's fusion. The `CallDef` nodes
/// carry the callable INDEX (encoded in their `body` slot) through lowering and
/// fusion; this pass rewrites them with the resolved body id and parameter
/// slots after both arenas are complete.
///
/// The returned map is callable index → fused body id — the only live bridge
/// from the pre-fuse callable list to the renumbered output arena, which post-
/// fuse passes (the collect|add callable rewrite) resolve body ids through.
///
/// # Errors
///
/// Returns [`ResourceError::AllocationFailed`] when reserving the fused step
/// backing or a graph node fails.
pub fn fuse_with_callables(
    nodes: &mut [ProgramNode],
    root: ProgramNodeId,
    callables: &[crate::program::CallableDef],
) -> Result<FusedProgram, ResourceError> {
    let mut out: Vec<ProgramNode> = Vec::new();
    let protected = scan_protected_equalities(nodes)?;
    let mut body_ids = alloc::collections::BTreeMap::new();
    // Shared `ChainBody` nodes are referenced by every arm of a `?//` chain:
    // their inner body must be fused EXACTLY ONCE, or the second arm's fuse
    // would either take the first arm's steps or re-expand the body per arm.
    // Memoized by source id, mirroring the callable bodies above.
    let mut shared = alloc::collections::BTreeMap::new();
    for (def, callable) in callables.iter().enumerate() {
        let fused = fuse_node(nodes, callable.body, &mut out, &mut shared, &protected)?;
        let id = materialize(fused, &mut out)?;
        body_ids.insert(def, id);
    }
    let fused_root = fuse_node(nodes, root, &mut out, &mut shared, &protected)?;
    let root_id = materialize(fused_root, &mut out)?;
    for node in &mut out {
        if let ProgramNode::CallDef {
            body,
            args,
            filter_args,
            tail,
            ..
        } = node
        {
            let def = body.index();
            let resolved = body_ids
                .get(&def)
                .copied()
                .ok_or(ResourceError::AccountingInvariantViolation)?;
            let param_slots = callables[def].param_slots.clone();
            let filter_slots = callables[def].filter_slots.clone();
            let tail = *tail;
            *node = ProgramNode::CallDef {
                body: resolved,
                param_slots,
                filter_slots,
                args: core::mem::take(args),
                filter_args: core::mem::take(filter_args),
                tail,
            };
        }
    }
    Ok((out, root_id, body_ids))
}

/// Fuses one source node, appending materialized graph nodes to `out`.
///
/// Lowering produces a tree (each node referenced once), so the steps of a
/// `Stage` are MOVED out of `src` rather than cloned — keeping the fuse
/// allocation-free beyond the new arena's own backing.
#[allow(
    clippy::too_many_lines,
    reason = "one arm per arena node family: the fusion law is read as a single table"
)]
fn fuse_node(
    src: &mut [ProgramNode],
    id: ProgramNodeId,
    out: &mut Vec<ProgramNode>,
    shared: &mut alloc::collections::BTreeMap<usize, ProgramNodeId>,
    protected: &[bool],
) -> Result<Fused, ResourceError> {
    match &mut src[id.index()] {
        ProgramNode::Stage { start, steps } => Ok(Fused::Stage {
            start: core::mem::replace(start, StageStart::Current),
            steps: core::mem::take(steps),
        }),
        ProgramNode::Choice { left, right } => {
            let (left, right) = (*left, *right);
            let left = fuse_child(src, left, out, shared, protected)?;
            let right = fuse_child(src, right, out, shared, protected)?;
            Ok(Fused::Node(push(out, ProgramNode::Choice { left, right })?))
        }
        // A `Binary` survives fusion like a `Choice`: fuse each operand graph and
        // keep the node (both operands read the whole input authority).
        ProgramNode::Binary { op, left, right, .. } => {
            let (op, left, right) = (*op, *left, *right);
            let left = fuse_child(src, left, out, shared, protected)?;
            let right = fuse_child(src, right, out, shared, protected)?;
            let node = push(out, ProgramNode::binary(op, left, right))?;
            // The correlated-scan table reads a `Binary` too, and it needs the
            // key path and the probe left WHOLE and adjacent under the node
            // (`super::join::is_probe_equality`). Hoisting `select(.id == 3)`
            // into `.id | (. == 3)` would trade a one-step pushdown for the K2
            // row's Θ(k·m) rescan, so the scan row wins the node — but ONLY
            // when this equality is actually a select-predicate leftmost
            // conjunct (the one context the scan walk reaches): a conditional
            // condition or any other operand position can never be a scan row,
            // and the hoist stays open there (014 Part A).
            if protected.get(id.index()).copied().unwrap_or(false)
                && super::join::is_probe_equality(out, op, left, right)
            {
                return Ok(Fused::Node(node));
            }
            // Both operands are evaluated (a Cartesian product), so any operand
            // that walks the prefix witnesses it and no separate witness is
            // needed — the same standing every `ConstructObject` member has.
            hoist_spine_prefix(out, node, &[left, right], None, false)
        }
        // A `Concat` survives fusion like a `Binary`: fuse each part and keep
        // the node (every part reads the whole input authority).
        ProgramNode::Concat { parts } => {
            let parts = core::mem::take(parts);
            let mut fused = Vec::new();
            fused
                .try_reserve(parts.len())
                .map_err(|_| ResourceError::AllocationFailed)?;
            for part in parts {
                fused.push(fuse_child(src, part, out, shared, protected)?);
            }
            let node = push(out, ProgramNode::Concat { parts: fused.clone() })?;
            hoist_spine_prefix(out, node, &fused, None, false)
        }
        // The control-flow graph nodes survive fusion like a `Choice`: fuse each
        // child graph and keep the node (the condition, both arms, and the two
        // alternative/logical operands all read the whole input authority).
        ProgramNode::Conditional {
            condition,
            consequent,
            alternative,
        } => {
            let (condition, consequent, alternative) = (*condition, *consequent, *alternative);
            let condition = fuse_child(src, condition, out, shared, protected)?;
            let consequent = fuse_child(src, consequent, out, shared, protected)?;
            let alternative = fuse_child(src, alternative, out, shared, protected)?;
            let node = push(
                out,
                ProgramNode::Conditional {
                    condition,
                    consequent,
                    alternative,
                },
            )?;
            // Only the CONDITION is evaluated unconditionally, so only it can
            // witness the hoisted prefix.
            hoist_spine_prefix(out, node, &[condition, consequent, alternative], Some(condition), false)
        }
        ProgramNode::Alternative { left, right } => {
            let (left, right) = (*left, *right);
            let left = fuse_child(src, left, out, shared, protected)?;
            let right = fuse_child(src, right, out, shared, protected)?;
            let node = push(out, ProgramNode::Alternative { left, right })?;
            // `//` evaluates its RIGHT operand only when the left published no
            // truthy output, so the left is the only witness. The hoist is a
            // suppression-CROSSING one: the miss the fallback handles
            // leaves the alternative's own region, so the hoisted upstream
            // stage is marked suppression-derived and the strict dial treats
            // its cell sites as marked (`hoist_spine_prefix`'s flag).
            hoist_spine_prefix(out, node, &[left, right], Some(left), true)
        }
        ProgramNode::Logical { operator, left, right } => {
            let (operator, left, right) = (*operator, *left, *right);
            let left = fuse_child(src, left, out, shared, protected)?;
            let right = fuse_child(src, right, out, shared, protected)?;
            let node = push(out, ProgramNode::Logical { operator, left, right })?;
            // `and`/`or` short-circuit on the left, so the left is the witness.
            hoist_spine_prefix(out, node, &[left, right], Some(left), false)
        }
        ProgramNode::CollectArray { body } => {
            let body = *body;
            fuse_collect_array(src, body, out, shared, protected)
        }
        // A `CountCollect` is a construction barrier with a scalar output: the
        // body fuses as an ordinary child graph and the barrier is kept (any
        // doubt → no prefix).
        ProgramNode::CountCollect { body } => {
            let body = *body;
            let body = match body {
                Some(body) => Some(fuse_child(src, body, out, shared, protected)?),
                None => None,
            };
            let node = push(out, ProgramNode::CountCollect { body })?;
            Ok(Fused::Node(node))
        }
        ProgramNode::ConstructObject { members } => {
            let members = core::mem::take(members);
            fuse_construct_object(src, members, out, shared, protected)
        }
        ProgramNode::Call {
            overload,
            revision,
            args,
            payload: _,
        } => {
            let (overload, revision) = (*overload, *revision);
            let args = core::mem::take(args);
            fuse_call(src, overload, revision, args, out, shared, protected)
        }
        // A recursive call survives fusion like a `Call`: fuse the call-site
        // argument graphs and keep the node. The body is a LEAF here — its
        // shared id is encoded in the `body` slot (the callable index) until
        // [`fuse_with_callables`] resolves every site to the one pre-fused body
        // — so the self-reference never re-enters fusion.
        ProgramNode::CallDef {
            body,
            args,
            filter_args,
            ..
        } => {
            let (body, args, filter_args) = (*body, core::mem::take(args), core::mem::take(filter_args));
            let mut fused = Vec::new();
            for arg in args {
                fused.push(fuse_child(src, arg, out, shared, protected)?);
            }
            let mut fused_filters = Vec::new();
            for arg in filter_args {
                fused_filters.push(fuse_child(src, arg, out, shared, protected)?);
            }
            Ok(Fused::Node(push(
                out,
                ProgramNode::CallDef {
                    body,
                    param_slots: Vec::new(),
                    filter_slots: Vec::new(),
                    args: fused,
                    filter_args: fused_filters,
                    tail: false,
                },
            )?))
        }
        ProgramNode::CallFilter { slot } => {
            let slot = *slot;
            Ok(Fused::Node(push(out, ProgramNode::CallFilter { slot })?))
        }
        // A `Try` survives fusion like a `Choice`: fuse the body and the optional
        // handler graph, keep the node (both read the whole input authority).
        ProgramNode::Try { body, handler } => {
            let (body, handler) = (*body, *handler);
            let body = fuse_child(src, body, out, shared, protected)?;
            let handler = match handler {
                Some(handler) => Some(fuse_child(src, handler, out, shared, protected)?),
                None => None,
            };
            Ok(Fused::Node(push(out, ProgramNode::Try { body, handler })?))
        }
        // A `ChainBody` is the `?//` chain's shared body barrier. Every arm's
        // frame references the SAME node, so it is fused EXACTLY ONCE: the
        // first arm fuses the inner body and records the fused id; every later
        // arm reuses it. This is what keeps a chain nested in its own body
        // linear instead of squaring per arm.
        ProgramNode::ChainBody { body } => {
            let source_id = id.index();
            if let Some(fused) = shared.get(&source_id) {
                return Ok(Fused::Node(*fused));
            }
            let body = *body;
            let body = fuse_child(src, body, out, shared, protected)?;
            let node = push(out, ProgramNode::ChainBody { body })?;
            shared.insert(source_id, node);
            Ok(Fused::Node(node))
        }
        // A `Label` survives fusion like a `Try`: fuse the body, keep the node.
        // The barrier may NOT be fused away even when its body is a bare `Stage`
        // — it stays a live catch target for any `break` inside that body.
        ProgramNode::Label { slot, body } => {
            let (slot, body) = (*slot, *body);
            let body = fuse_child(src, body, out, shared, protected)?;
            Ok(Fused::Node(push(out, ProgramNode::Label { slot, body })?))
        }
        // `Break` has no children; it survives fusion as itself.
        ProgramNode::Break { slot } => {
            let slot = *slot;
            Ok(Fused::Node(push(out, ProgramNode::Break { slot })?))
        }
        // A `Modify` survives fusion like a `Choice`: fuse the path generator and
        // the update graph, keep the node. Neither child may fuse ACROSS the
        // node — the path generator is evaluated in PATH mode and the update runs
        // over a value read out of the fold accumulator, so a `Stage∘Stage`
        // collapse over either boundary would change what is read.
        ProgramNode::Modify { paths, update, mode } => {
            let (paths, update, mode) = (*paths, *update, *mode);
            let paths = fuse_child(src, paths, out, shared, protected)?;
            let update = fuse_child(src, update, out, shared, protected)?;
            Ok(Fused::Node(push(out, ProgramNode::Modify { paths, update, mode })?))
        }
        // A `FactAssign` survives fusion exactly like a `Modify` (the same two
        // child graphs, the same no-cross boundary — the edit lane resolves its
        // path over the LOCATED input and runs the update over the payload).
        ProgramNode::FactAssign {
            paths,
            role,
            kind,
            selector,
            update,
            mode,
        } => {
            let (paths, update, mode) = (*paths, *update, *mode);
            let role = role.clone();
            let kind = kind.clone();
            let selector = *selector;
            let paths = fuse_child(src, paths, out, shared, protected)?;
            let update = fuse_child(src, update, out, shared, protected)?;
            Ok(Fused::Node(push(
                out,
                ProgramNode::FactAssign {
                    paths,
                    role,
                    kind,
                    selector,
                    update,
                    mode,
                },
            )?))
        }
        // `Empty` has no children; it survives fusion as itself.
        ProgramNode::Empty => Ok(Fused::Node(push(out, ProgramNode::Empty)?)),
        // The binding family survives fusion like a `Choice`: fuse each child
        // graph and keep the node. Bodies never fuse ACROSS the binder — the body
        // depends on the env slot the binder writes, so a `Stage∘Stage` collapse
        // over the boundary would lose the per-source-output rebinding.
        ProgramNode::Bind {
            source,
            slot,
            body,
            frame,
        } => {
            let (source, slot, body) = (*source, *slot, *body);
            let frame = *frame;
            let source = fuse_child(src, source, out, shared, protected)?;
            let body = fuse_child(src, body, out, shared, protected)?;
            Ok(Fused::Node(push(
                out,
                ProgramNode::Bind {
                    source,
                    slot,
                    body,
                    frame,
                },
            )?))
        }
        // An engine binding survives fusion exactly like a `Bind`: fuse the
        // constructor graph and the body, keep the node, and never fuse a stage
        // ACROSS the binder (the cursor is seeded once per evaluation).
        ProgramNode::EngineBind { source, slot, body } => {
            let (source, slot, body) = (*source, *slot, *body);
            let source = fuse_child(src, source, out, shared, protected)?;
            let body = fuse_child(src, body, out, shared, protected)?;
            Ok(Fused::Node(push(out, ProgramNode::EngineBind { source, slot, body })?))
        }
        // An engine pull and the cursor body are leaf-shaped producers: the
        // pull has no children, and the generator's three filters fuse under it
        // like any subgraph. Both survive fusion (zero pushdown prefix).
        ProgramNode::EnginePull { slot, kind } => Ok(Fused::Node(push(
            out,
            ProgramNode::EnginePull {
                slot: *slot,
                kind: *kind,
            },
        )?)),
        ProgramNode::EngineGenerator { init, update, extract } => {
            let (init, update, extract) = (*init, *update, *extract);
            let init = fuse_child(src, init, out, shared, protected)?;
            let update = fuse_child(src, update, out, shared, protected)?;
            let extract = fuse_child(src, extract, out, shared, protected)?;
            Ok(Fused::Node(push(
                out,
                ProgramNode::EngineGenerator { init, update, extract },
            )?))
        }
        // A `~rng` node's seed graph fuses under it like any subgraph; the node
        // survives fusion (zero pushdown prefix).
        ProgramNode::EngineRng { seed } => {
            let seed = *seed;
            let seed = fuse_child(src, seed, out, shared, protected)?;
            Ok(Fused::Node(push(out, ProgramNode::EngineRng { seed })?))
        }
        ProgramNode::Reduce {
            source,
            slot,
            init,
            update,
            keyed_collect: _,
        } => {
            let (source, slot, init, update) = (*source, *slot, *init, *update);
            let source = fuse_child(src, source, out, shared, protected)?;
            let init = fuse_child(src, init, out, shared, protected)?;
            let update = fuse_child(src, update, out, shared, protected)?;
            Ok(Fused::Node(push(
                out,
                ProgramNode::Reduce {
                    source,
                    slot,
                    init,
                    update,
                    keyed_collect: None,
                },
            )?))
        }
        ProgramNode::Foreach {
            source,
            slot,
            init,
            update,
            extract,
        } => {
            let (source, slot, init, update, extract) = (*source, *slot, *init, *update, *extract);
            let source = fuse_child(src, source, out, shared, protected)?;
            let init = fuse_child(src, init, out, shared, protected)?;
            let update = fuse_child(src, update, out, shared, protected)?;
            let extract = match extract {
                Some(extract) => Some(fuse_child(src, extract, out, shared, protected)?),
                None => None,
            };
            Ok(Fused::Node(push(
                out,
                ProgramNode::Foreach {
                    source,
                    slot,
                    init,
                    update,
                    extract,
                },
            )?))
        }
        // A `Counted` survives fusion like the loop family: fuse its source graph
        // and keep the node (its count is an env slot, not a graph edge).
        ProgramNode::Counted {
            source,
            count,
            kind,
            stop,
        } => {
            let (source, count, kind, stop) = (*source, *count, *kind, *stop);
            let source = fuse_child(src, source, out, shared, protected)?;
            Ok(Fused::Node(push(
                out,
                ProgramNode::Counted {
                    source,
                    count,
                    kind,
                    stop,
                },
            )?))
        }
        ProgramNode::FlatMap { upstream, body } => {
            let (upstream, body) = (*upstream, *body);
            fuse_flatmap(src, upstream, body, out, shared, protected)
        }
    }
}

/// Fuses a `CollectArray`'s body graph, then HOISTS the body's leading static
/// prefix onto a stage UPSTREAM of the rebuilt collect — the same rewrite
/// [`fuse_construct_object`] performs, for the same reason and with the same
/// payoff: `[.orders[0]]` becomes `.orders[0] | [.]`, whose prefix the standing
/// `FlatMap` arm of [`analyze_pushdown`] pushes down, where the bare collect
/// contributed zero prefix and took the whole-document route to read one element.
///
/// A collect has ONE producer, so there is no lattice join to take; what there is
/// instead is a STRICTER admission than the constructor's, because the two nodes
/// report a zero-output producer differently. A constructor's members are a
/// Cartesian product, so a member that emits nothing makes the constructor emit
/// nothing — exactly what the hoisted form does when the prefix emits nothing.
/// A collect is not: `[empty]` is `[]`, while `empty | [.]` publishes nothing at
/// all. So the hoisted run stops at the first OPTIONAL step as well as at the
/// first step the codec cannot resolve. What remains — `.k`/`.[i]` without `?` —
/// emits exactly one value or raises, never zero, and a raise is the same raise
/// at the same point either side of the rewrite (`[.a.b]` and `.a.b | [.]` both
/// abort on `{"a": 5}`).
///
/// The hoist is also ALL-OR-NOTHING, where the constructor's is a prefix: a
/// collect body is the one place [`descend_element_spine`] enters besides a
/// `FlatMap` upstream, so a body that keeps ANY suffix is a body whose `.[]` was
/// already reachable, and hoisting part of its path would SPLIT the container
/// path across a pipe — trading a fused boundary for a split one and demoting the
/// route rather than promoting it (`[.catalog[]]` loses its structure count).
/// Requiring the whole body path makes the residual exactly `[.]`, so the rewrite
/// only ever fires where there was no boundary to lose.
///
/// `map(f)` is therefore untouched: it lowers to `[.[] | f]`, whose body holds a
/// `.[]` and a residual, so the collect is rebuilt exactly as before.
fn fuse_collect_array(
    src: &mut [ProgramNode],
    body: Option<ProgramNodeId>,
    out: &mut Vec<ProgramNode>,
    shared: &mut alloc::collections::BTreeMap<usize, ProgramNodeId>,
    protected: &[bool],
) -> Result<Fused, ResourceError> {
    let Some(body) = body else {
        return Ok(Fused::Node(push(out, ProgramNode::CollectArray { body: None })?));
    };
    // Fused but NOT yet materialized, for the reason `fuse_construct_object`
    // records: the hoist reads its steps out of a loose `Fused::Stage`, and
    // materializing first would leave the pre-hoist stage behind as a dead node.
    let body = fuse_node(src, body, out, shared, protected)?;
    let prefix_len = match &body {
        Fused::Stage {
            start: StageStart::Current,
            steps,
        } if total_static_run_len(steps) == steps.len() => steps.len(),
        Fused::Stage { .. } | Fused::Node(_) => 0,
    };
    let mut prefix = Vec::new();
    let mut taken = false;
    let body = strip_hoisted_prefix(body, prefix_len, &mut prefix, &mut taken)?;
    let body = materialize(body, out)?;
    let collect = push(out, ProgramNode::CollectArray { body: Some(body) })?;
    if !taken {
        return Ok(Fused::Node(collect));
    }
    let upstream = push(
        out,
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: prefix,
        },
    )?;
    Ok(Fused::Node(push(
        out,
        ProgramNode::FlatMap {
            upstream,
            body: collect,
        },
    )?))
}

/// Fuses each member's key and value graph of a `ConstructObject`, then HOISTS
/// the static prefix every member producer shares onto a stage UPSTREAM of the
/// rebuilt constructor (a constructor otherwise survives fusion like a `Choice`).
///
/// The constructor node itself still contributes zero pushdown prefix, and that
/// is not a hedge: its members are separate graphs over one input, while the
/// codec vocabulary carries exactly ONE exact path per requirement
/// (`AccessFootprint::try_exact`), so the UNION of two unrelated member paths is
/// not expressible at all. What IS expressible is their JOIN in the prefix
/// lattice — the leading steps every member walks before they diverge — and
/// hoisting that join rewrites `{a: .p.x, b: .p.y}` into `.p | {a: .x, b: .y}`,
/// whose prefix the standing `FlatMap` arm of [`analyze_pushdown`] already pushes
/// down. Nothing about the constructor's own classification moved; what moved is
/// the part that was never the constructor's to begin with.
///
/// It is a REWRITE rather than a classification because [`PushdownSplit`] names
/// ONE entry stage whose leading steps the executor skips by node identity: a
/// constructor has no such stage until one is made.
///
/// The recognition DECLINES — leaving today's whole-input authority exactly as it
/// is — unless every producer is one of two provably total, single-output shapes,
/// because hoisting past a producer that can raise or emit ZERO changes which
/// outcome the constructor reports: `{a: empty, b: .x.y}` publishes nothing on a
/// scalar input, while `.x.y | {a: empty, b: .}` would raise. The two shapes are
/// a steps-free literal (every static key, and any constant member value — one
/// output, no error, never zero) and a current-start stage, which by definition
/// walks the shared prefix itself, so a raise inside that prefix is one the
/// FIRST member already raised. Steps join only when their optional flags agree,
/// since the flag decides whether a mismatch suppresses the whole constructor or
/// aborts it.
fn fuse_construct_object(
    src: &mut [ProgramNode],
    members: Vec<ObjectMemberNode>,
    out: &mut Vec<ProgramNode>,
    shared: &mut alloc::collections::BTreeMap<usize, ProgramNodeId>,
    protected: &[bool],
) -> Result<Fused, ResourceError> {
    let mut fused = Vec::new();
    fused
        .try_reserve(members.len())
        .map_err(|_| ResourceError::AllocationFailed)?;
    for member in members {
        // Fused but NOT yet materialized: a loose `Fused::Stage` is what the
        // hoist reads its steps out of, and materializing first would leave the
        // pre-hoist stages behind as dead arena nodes.
        let key = fuse_node(src, member.key, out, shared, protected)?;
        let value = fuse_node(src, member.value, out, shared, protected)?;
        fused.push((key, value));
    }
    let prefix_len = hoistable_member_prefix(&fused);
    let mut prefix = Vec::new();
    let mut taken = false;
    let mut placed = Vec::new();
    placed
        .try_reserve(fused.len())
        .map_err(|_| ResourceError::AllocationFailed)?;
    for (key, value) in fused {
        let key = strip_hoisted_prefix(key, prefix_len, &mut prefix, &mut taken)?;
        let value = strip_hoisted_prefix(value, prefix_len, &mut prefix, &mut taken)?;
        placed.push(ObjectMemberNode {
            key: materialize(key, out)?,
            value: materialize(value, out)?,
            static_key: None,
        });
    }
    let object = push(out, ProgramNode::ConstructObject { members: placed })?;
    if !taken {
        return Ok(Fused::Node(object));
    }
    let upstream = push(
        out,
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: prefix,
        },
    )?;
    Ok(Fused::Node(push(out, ProgramNode::FlatMap { upstream, body: object })?))
}

/// How many leading steps a `ConstructObject`'s producers all share and the codec
/// can resolve — the hoistable prefix of [`fuse_construct_object`], or `0` when
/// the shape declines.
///
/// The answer is the prefix-lattice join of the current-start producers' step
/// lists, truncated to its leading static run for exactly the reason
/// [`analyze_pushdown`] truncates a stage's: a `.[]`, `.[$x]`, `..`, or `.[a:b]`
/// is not a step the codec resolves. A literal producer constrains nothing (it
/// reads no input); ANY other producer declines the whole hoist.
fn hoistable_member_prefix(members: &[(Fused, Fused)]) -> usize {
    let mut common: Option<&[StageStep]> = None;
    for (key, value) in members {
        for producer in [key, value] {
            match producer {
                // A steps-free literal — every static key, and a constant member
                // value — ignores the input, so it neither walks the prefix nor
                // constrains it, and it can neither raise nor emit zero.
                Fused::Stage {
                    start: StageStart::Literal(_),
                    steps,
                } if steps.is_empty() => {}
                Fused::Stage {
                    start: StageStart::Current,
                    steps,
                } => {
                    common = Some(match common {
                        None => steps.as_slice(),
                        Some(seen) => &seen[..shared_step_len(seen, steps)],
                    });
                }
                Fused::Stage { .. } | Fused::Node(_) => return 0,
            }
        }
    }
    common.map_or(0, static_run_len)
}

/// The most static steps one spine hoist moves upstream.
///
/// Matched to the codec requirement's own projection-path limit, the codec's own
/// limit on how deep a container path it will carry. A longer join is TRUNCATED
/// rather than declined: truncation is the same lattice answer reached one step
/// earlier, and every deeper step stays in the residual where it already was.
const MAX_HOISTED_PREFIX: usize = 8;

/// HOISTS the static prefix every operand of one multi-producer spine node walks
/// onto a stage UPSTREAM of it, or returns the node untouched.
///
/// `Conditional`/`Alternative`/`Logical`/`Binary` each read one dot through
/// SEVERAL producer graphs, so — unlike the single-producer `CollectArray` and
/// `Label`, which [`analyze_pushdown`] simply descends — there is nothing to
/// descend into. What there is instead is the prefix-lattice JOIN of the
/// producers: the leading steps they all walk before they diverge. Hoisting it
/// rewrites `if .u[0].active then .u[0].email else null end` into
/// `.u[0] | if .active then .email else null end`, whose prefix the standing
/// `FlatMap` arm of [`analyze_pushdown`] pushes down, where the bare conditional
/// contributed zero and read the whole document to answer a one-record demand.
/// It is a rewrite rather than a classification for the reason
/// [`fuse_construct_object`] records: [`PushdownSplit`] names ONE entry stage.
///
/// The codec's exact-path vocabulary carries one path per requirement, so the
/// UNION of two divergent operand paths is still not expressible — their JOIN
/// is, and a node whose operands share no leading step keeps today's route.
///
/// `witness` names the operand that is evaluated UNCONDITIONALLY, when only one
/// is: the hoisted prefix must be walked on every evaluation path, or
/// `if true then 1 else .p.x end` would raise on a scalar where the answer is
/// `1`.
/// `None` means every operand is evaluated, which needs no witness at all.
///
/// `suppression_hoist` marks the hoisted steps suppression-derived:
/// the hoist MOVED them out of a suppression region (an `Alternative` operand),
/// so the strict dial must treat their cell sites as marked or `.b // "x"`
/// would fire `missing-object-key`. The other spine nodes' operands are not
/// suppression regions, so their hoists pass `false`.
fn hoist_spine_prefix(
    out: &mut Vec<ProgramNode>,
    node: ProgramNodeId,
    operands: &[ProgramNodeId],
    witness: Option<ProgramNodeId>,
    suppression_hoist: bool,
) -> Result<Fused, ResourceError> {
    let Some(prefix_len) = hoistable_operand_prefix(out, operands, witness) else {
        return Ok(Fused::Node(node));
    };
    let mut prefix = Vec::new();
    let mut taken = false;
    for &operand in operands {
        if let Some(stage) = leading_current_stage(out, operand) {
            strip_leading_steps(out, stage, prefix_len, &mut prefix, &mut taken)?;
        }
    }
    if suppression_hoist {
        for step in &mut prefix {
            step.mark_suppressed();
        }
    }
    let upstream = push(
        out,
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: prefix,
        },
    )?;
    Ok(Fused::Node(push(out, ProgramNode::FlatMap { upstream, body: node })?))
}

/// How many leading steps a spine node's operands all walk and the codec can
/// resolve — the hoistable prefix of [`hoist_spine_prefix`], or `None` when the
/// shape declines.
///
/// The declining rules, in order:
///
/// - a `witness` with no leading producer of its own cannot walk the prefix on
///   the evaluation path it owns, so the whole node declines;
/// - an operand that neither ignores its input nor exposes a `Current`-start
///   leading stage POISONS the node — a `Call`, a comma, a `try`, a binder, a
///   nested conditional (so `elif` declines), a constructor, and a bound- or
///   literal-start stage that still walks steps are all unresolvable here;
/// - the join is truncated by [`total_static_run_len`], which stops both at the
///   first step the codec cannot resolve and at the first OPTIONAL one: a `?`
///   step can emit ZERO, and a hoisted prefix that emits zero turns
///   `.a?.x // 1` over `5` from `1` into no output at all.
fn hoistable_operand_prefix(
    out: &[ProgramNode],
    operands: &[ProgramNodeId],
    witness: Option<ProgramNodeId>,
) -> Option<usize> {
    if let Some(witness) = witness {
        leading_current_stage(out, witness)?;
    }
    let mut common: Option<&[StageStep]> = None;
    for &operand in operands {
        // A steps-free literal ignores the input, so it neither walks the prefix
        // nor constrains it, and it can neither raise nor emit zero. Anything
        // else document-independent but FALLIBLE (`error("x")`, `1/0`) is not
        // exempt: inside a `Binary` the operand evaluation order would decide
        // which error surfaces.
        if ignores_input(out, operand) {
            continue;
        }
        let stage = leading_current_stage(out, operand)?;
        let ProgramNode::Stage { steps, .. } = &out[stage.index()] else {
            return None;
        };
        common = Some(match common {
            None => steps.as_slice(),
            Some(seen) => &seen[..shared_step_len(seen, steps)],
        });
    }
    let prefix_len = total_static_run_len(common?).min(MAX_HOISTED_PREFIX);
    (prefix_len > 0).then_some(prefix_len)
}

/// The `Current`-start [`ProgramNode::Stage`] a graph evaluates FIRST against
/// its own dot, or `None` when it has no such leading producer.
///
/// A `FlatMap` body runs over its upstream's OUTPUTS, so the upstream is the
/// only child that sees the graph's dot — which makes descending it the same
/// move [`analyze_pushdown`] makes on the spine, and is what lets
/// `.users | map(.age) | add` contribute `.users` to a join. A `Label` is NOT
/// descended: moving a path out of a live `break` target is a separate soundness
/// argument with no payoff.
fn leading_current_stage(out: &[ProgramNode], id: ProgramNodeId) -> Option<ProgramNodeId> {
    let mut cursor = id;
    loop {
        match &out[cursor.index()] {
            ProgramNode::Stage {
                start: StageStart::Current,
                ..
            } => return Some(cursor),
            ProgramNode::FlatMap { upstream, .. } => cursor = *upstream,
            _ => return None,
        }
    }
}

/// Whether a graph provably ignores its input: a steps-free literal producer,
/// the one shape that reads nothing, emits exactly one value, and cannot raise.
pub(crate) fn ignores_input(out: &[ProgramNode], id: ProgramNodeId) -> bool {
    matches!(
        &out[id.index()],
        ProgramNode::Stage {
            start: StageStart::Literal(_),
            steps,
        } if steps.is_empty()
    )
}

/// Moves the leading `prefix_len` steps out of an already-placed stage, donating
/// them to `prefix` the first time a stage supplies them.
///
/// The in-place counterpart of [`strip_hoisted_prefix`], which reads its steps
/// out of a loose [`Fused::Stage`]. A spine operand is materialized before its
/// node exists (it may sit under a `FlatMap`), so its steps are moved through
/// the arena instead.
fn strip_leading_steps(
    out: &mut [ProgramNode],
    stage: ProgramNodeId,
    prefix_len: usize,
    prefix: &mut Vec<StageStep>,
    taken: &mut bool,
) -> Result<(), ResourceError> {
    let ProgramNode::Stage { steps, .. } = &mut out[stage.index()] else {
        return Ok(());
    };
    let mut suffix = Vec::new();
    suffix
        .try_reserve_exact(steps.len().saturating_sub(prefix_len))
        .map_err(|_| ResourceError::AllocationFailed)?;
    suffix.extend(steps.drain(prefix_len..));
    let head = core::mem::replace(steps, suffix);
    if !*taken {
        *prefix = head;
        *taken = true;
    }
    Ok(())
}

/// How many leading steps the codec can resolve statically.
///
/// A `.[]`, `.[$x]`, `..`, or `.[a:b]` is not a step the codec resolves, for
/// exactly the reason [`analyze_pushdown`] truncates a stage's prefix there.
fn static_run_len(steps: &[StageStep]) -> usize {
    steps
        .iter()
        .position(|step| !matches!(step.access(), StepAccess::Key(_) | StepAccess::Index(_)))
        .unwrap_or(steps.len())
}

/// [`static_run_len`], additionally stopping at the first OPTIONAL step — the
/// leading run that provably emits exactly one value or raises, never zero.
///
/// This is the run [`fuse_collect_array`] may hoist: a suppressed `?` would turn
/// the collect's `[]` into no output at all once the producer moved upstream.
fn total_static_run_len(steps: &[StageStep]) -> usize {
    let optional = steps.iter().position(StageStep::is_optional).unwrap_or(steps.len());
    static_run_len(steps).min(optional)
}

/// How many leading steps two producers agree on, access AND optional flag alike.
///
/// Only `.k`/`.[i]` steps can agree: every other access is one the hoist would
/// have truncated anyway, so reporting disagreement is the same answer reached
/// one step earlier.
fn shared_step_len(left: &[StageStep], right: &[StageStep]) -> usize {
    left.iter()
        .zip(right)
        .take_while(|(left, right)| {
            left.is_optional() == right.is_optional()
                && match (left.access(), right.access()) {
                    (StepAccess::Key(left), StepAccess::Key(right)) => left == right,
                    (StepAccess::Index(left), StepAccess::Index(right)) => left == right,
                    _ => false,
                }
        })
        .count()
}

/// Removes the hoisted `prefix_len` leading steps from one member producer,
/// donating them to `prefix` the first time a producer supplies them.
///
/// A literal producer walks no steps and is returned untouched; a current-start
/// producer keeps its post-prefix suffix, which is what the rebuilt constructor
/// runs against the hoisted stage's located value.
fn strip_hoisted_prefix(
    producer: Fused,
    prefix_len: usize,
    prefix: &mut Vec<StageStep>,
    taken: &mut bool,
) -> Result<Fused, ResourceError> {
    if prefix_len == 0 {
        return Ok(producer);
    }
    match producer {
        Fused::Stage {
            start: StageStart::Current,
            mut steps,
        } => {
            let mut suffix = Vec::new();
            suffix
                .try_reserve_exact(steps.len().saturating_sub(prefix_len))
                .map_err(|_| ResourceError::AllocationFailed)?;
            suffix.extend(steps.drain(prefix_len..));
            if *taken {
                drop(steps);
            } else {
                *prefix = steps;
                *taken = true;
            }
            Ok(Fused::Stage {
                start: StageStart::Current,
                steps: suffix,
            })
        }
        producer => Ok(producer),
    }
}

/// Fuses one `FlatMap(upstream, body)`: `Stage∘Stage` collapses to one `Stage`
/// (upstream steps before body steps, the upstream start preserved), and any
/// other shape (a `Choice`/constructor on either side, or a literal-start body
/// that ignores the upstream value) keeps the `FlatMap` with its children fused.
fn fuse_flatmap(
    src: &mut [ProgramNode],
    upstream_id: ProgramNodeId,
    body_id: ProgramNodeId,
    out: &mut Vec<ProgramNode>,
    shared: &mut alloc::collections::BTreeMap<usize, ProgramNodeId>,
    protected: &[bool],
) -> Result<Fused, ResourceError> {
    let upstream = fuse_node(src, upstream_id, out, shared, protected)?;
    let body = fuse_node(src, body_id, out, shared, protected)?;
    match (upstream, body) {
        // Stage∘Stage fuses to one stage ONLY when the body starts from the current
        // input (`FlatMap(Stage{s, p}, Stage{Current, q})` → `Stage{s, p ++ q}`);
        // the fused stage keeps the UPSTREAM start, so a literal-start upstream
        // survives into the fused stage. A literal-start or VARIABLE-start BODY
        // (both ignore the upstream value, changing cardinality) blocks fusion
        // and falls through — the two non-`Current` starts behave symmetrically.
        (
            Fused::Stage {
                start,
                steps: mut prefix,
            },
            Fused::Stage {
                start: StageStart::Current,
                steps: suffix,
            },
        ) => {
            prefix
                .try_reserve(suffix.len())
                .map_err(|_| ResourceError::AllocationFailed)?;
            prefix.extend(suffix);
            Ok(Fused::Stage { start, steps: prefix })
        }
        // `PATH | map(f)` ≡ `PATH | [.[] | f]`: hoist a static Key/Index prefix
        // into the collected fan-out so the arena is `[PATH[] | f]` in
        // path-normal form (the element/count tables then see one collect).
        // Not `PATH | (map(f) | g)`: pipe is right-associative, and hoisting
        // through `g` would hide `PATH` from a spine join (`(PATH | map | add)
        // / (PATH | length)`). Those spellings are rows on the count and
        // collect-add tables instead.
        (
            Fused::Stage {
                start: StageStart::Current,
                steps: prefix,
            },
            Fused::Node(body_id),
        ) if hoistable_collect_prefix(&prefix) && collect_inner(out, body_id).is_some() => {
            hoist_prefix_into_collect(prefix, body_id, out)
        }
        // A Choice/constructor on either side, or a literal-start body, blocks
        // fusion: keep the FlatMap.
        (upstream, body) => {
            let upstream = materialize(upstream, out)?;
            let body = materialize(body, out)?;
            Ok(Fused::Node(push(out, ProgramNode::FlatMap { upstream, body })?))
        }
    }
}

/// When `body` is a `CollectArray`, prepend `prefix` onto its body's leading
/// Current-start stage (or onto that stage when the body is a `FlatMap` of one)
/// and return the rebuilt collect. `None` if `body` is not that shape.
fn hoist_prefix_into_collect(
    prefix: Vec<StageStep>,
    body_id: ProgramNodeId,
    out: &mut Vec<ProgramNode>,
) -> Result<Fused, ResourceError> {
    let inner = collect_inner(out, body_id).expect("guard proved a collect");
    if prepend_in_place(out, inner, &prefix)? {
        // Reuse the collect node: copying Each into a fresh stage would leave
        // the old Each in the arena and `count_each_steps` (whole-arena) would
        // see two boundaries.
        return Ok(Fused::Node(body_id));
    }
    let stage = push(
        out,
        ProgramNode::Stage {
            start: StageStart::Current,
            steps: prefix,
        },
    )?;
    let mapped = push(
        out,
        ProgramNode::FlatMap {
            upstream: stage,
            body: inner,
        },
    )?;
    Ok(Fused::Node(push(
        out,
        ProgramNode::CollectArray { body: Some(mapped) },
    )?))
}

fn collect_inner(out: &[ProgramNode], body_id: ProgramNodeId) -> Option<ProgramNodeId> {
    match out.get(body_id.index()) {
        Some(ProgramNode::CollectArray { body: Some(inner) }) => Some(*inner),
        _ => None,
    }
}

/// Fuses each argument graph of a `Call` and rebuilds the node with the stable
/// overload identity preserved (a call survives fusion, like a `Choice`).
fn fuse_call(
    src: &mut [ProgramNode],
    overload: jqf_builtins::registry::BuiltinOverloadId,
    revision: jqf_builtins::registry::SemanticRevision,
    args: Vec<ProgramNodeId>,
    out: &mut Vec<ProgramNode>,
    shared: &mut alloc::collections::BTreeMap<usize, ProgramNodeId>,
    protected: &[bool],
) -> Result<Fused, ResourceError> {
    let mut fused = Vec::new();
    fused
        .try_reserve(args.len())
        .map_err(|_| ResourceError::AllocationFailed)?;
    for arg in args {
        fused.push(fuse_child(src, arg, out, shared, protected)?);
    }
    Ok(Fused::Node(push(out, ProgramNode::call(overload, revision, fused))?))
}

/// Fuses one child graph and materializes it into the arena, returning its id.
///
/// The fuse-then-materialize pair every graph node applies to each of its
/// children (a `Choice`/`Binary`/`Conditional`/… operand always keeps its fused
/// child as a placed node, never a loose stage).
fn fuse_child(
    src: &mut [ProgramNode],
    id: ProgramNodeId,
    out: &mut Vec<ProgramNode>,
    shared: &mut alloc::collections::BTreeMap<usize, ProgramNodeId>,
    protected: &[bool],
) -> Result<ProgramNodeId, ResourceError> {
    let fused = fuse_node(src, id, out, shared, protected)?;
    materialize(fused, out)
}

/// Materializes a [`Fused`] node into the arena, returning its id.
fn materialize(fused: Fused, out: &mut Vec<ProgramNode>) -> Result<ProgramNodeId, ResourceError> {
    match fused {
        Fused::Stage { start, steps } => push(out, ProgramNode::Stage { start, steps }),
        Fused::Node(id) => Ok(id),
    }
}

/// Appends a node to the fused arena and returns its dense id.
fn push(out: &mut Vec<ProgramNode>, node: ProgramNode) -> Result<ProgramNodeId, ResourceError> {
    let id = ProgramNodeId::from_index(out.len()).ok_or(ResourceError::AllocationFailed)?;
    out.try_reserve(1).map_err(|_| ResourceError::AllocationFailed)?;
    out.push(node);
    Ok(id)
}

/// Derives the codec-pushdown split from a path-normal arena.
///
/// The entry stage is reached by descending the root through `FlatMap` upstream
/// edges (never into a `Choice`): a `Stage` on the spine contributes its leading
/// static steps up to (but excluding) the first `.[]` ([`StepAccess::Each`]) as
/// the pushed-down prefix, while a `Choice` on the spine pushes nothing down (a
/// whole-document comma). A program with no `Each` and no `Choice` pushes its
/// whole path down and leaves an identity residual (the pipe/optional grammar).
pub fn analyze(nodes: &[ProgramNode], root: ProgramNodeId) -> PushdownSplit {
    let element = name_element_boundary(nodes, root);
    let mut split = analyze_pushdown(nodes, root);
    split.element = element;
    split
}

/// Names the program's element boundary — the single `.[]` a projected route
/// would stream — in (node, offset) coordinates, or `None` when the shape has
/// none it can name.
///
/// The walk descends the ELEMENT SPINE, which is wider than the pushdown spine:
/// besides a `FlatMap` upstream it enters a `CollectArray` body (`map`'s lowered
/// `[.[] | f]`), a `Reduce`/`Foreach` SOURCE, and a `Bind` SOURCE — exactly the
/// three places the first classifier recorded a boundary the split could not
/// name. It declines a `Choice` (two members, no single
/// boundary), a `Call` (its arguments read the call's input, not the spine), a
/// literal- or variable-start stage (its producer ignores the document, so its
/// `.[]` is not a document element stream — deviation 9), and any stage whose
/// pre-`.[]` steps are not all static.
fn name_element_boundary(nodes: &[ProgramNode], root: ProgramNodeId) -> Option<ElementBoundary> {
    let sole = super::path_steps::count_each_steps(nodes) == 1;
    let mut boundary = descend_element_spine(nodes, root, None, BoundaryConsumer::Residual)?;
    boundary.sole = sole;
    Some(boundary)
}

/// One step of the element-spine descent (see [`name_element_boundary`]).
///
/// `outer` carries an already-collected upstream static prefix — the `.catalog`
/// of `.catalog | map(.name)` — as (stage, step count).
#[allow(
    clippy::match_same_arms,
    reason = "the two declining arms decline for different laws (a non-document producer versus a \
              node with no single element spine); merging them would erase the distinction"
)]
#[allow(clippy::too_many_lines)]
fn descend_element_spine(
    nodes: &[ProgramNode],
    id: ProgramNodeId,
    outer: Option<(ProgramNodeId, usize)>,
    consumer: BoundaryConsumer,
) -> Option<ElementBoundary> {
    match &nodes[id.index()] {
        // The boundary lands here when this stage's first non-static step is a
        // `.[]`, or a `.[a:b]` immediately followed by one (the container-path
        // range). A `..`/`.[$x]` first is P2 vocabulary the codec cannot
        // resolve, so no boundary is nameable at all; no `.[]` at all means
        // this stage is a pure prefix, which only its CALLER can use.
        ProgramNode::Stage {
            start: StageStart::Current,
            steps,
        } => {
            let stopper = steps.iter().position(|step| {
                matches!(
                    step.access(),
                    StepAccess::Each
                        | StepAccess::DynVar(_)
                        | StepAccess::Descend
                        | StepAccess::Slice(_)
                        | StepAccess::NodeAccessor(_)
                        | StepAccess::Attribute(_)
                        | StepAccess::DynNodeAccessor(_)
                        | StepAccess::DynAttribute(_)
                )
            })?;
            // The walk admits exactly ONE trailing `.[a:b]` on the CONTAINER
            // path: `.catalog[1000:20000][]` names its boundary at the `.[]`
            // and hands the codec `.catalog` plus the range. Everything else
            // about the walk is unchanged, so the change is strictly additive —
            // a shape that named a boundary before names the same one, and a
            // shape that named none may now name one.
            //
            // A second slice on the container path (`.a[1:3][2:4][]`) declines:
            // composing two ranges statically is only sound for non-negative
            // closed bounds, and the composition is not admitted.
            let stopper = match steps[stopper].access() {
                StepAccess::Each => stopper,
                StepAccess::Slice(bounds) => {
                    bounds.try_normalize()?;
                    let after = stopper.checked_add(1)?;
                    if !matches!(
                        steps.get(after).map(StageStep::access),
                        Some(StepAccess::Each)
                    ) {
                        return None;
                    }
                    after
                }
                StepAccess::DynVar(_)
                | StepAccess::Descend
                | StepAccess::Key(_)
                | StepAccess::Index(_)
                | StepAccess::NodeAccessor(_)
                | StepAccess::Attribute(_)
                | StepAccess::DynNodeAccessor(_)
                | StepAccess::DynAttribute(_) => return None,
            };
            Some(ElementBoundary {
                stage: id,
                stage_prefix_len: stopper,
                consumer,
                // Overwritten by `name_element_boundary`, which owns the count.
                sole: false,
            })
        }
        // A literal- or variable-start stage produces values the document never
        // supplied, so an `.[]` inside it is not a document element stream.
        ProgramNode::Stage { .. } => None,
        // A shared `?//` chain body has no single element spine a caller can
        // name from the outside: the chain's arms each read the whole input.
        ProgramNode::ChainBody { .. } => None,
        // The upstream owns the boundary when it has one; otherwise a purely
        // STATIC upstream stage becomes the outer container prefix for the body
        // (`.catalog | map(.name)`). A non-static upstream with no boundary of its
        // own leaves nothing nameable from the document root.
        ProgramNode::FlatMap { upstream, body } => {
            let (upstream, body) = (*upstream, *body);
            if let Some(found) = descend_element_spine(nodes, upstream, outer, consumer) {
                return Some(found);
            }
            if outer.is_some() {
                return None;
            }
            let ProgramNode::Stage {
                start: StageStart::Current,
                steps,
            } = &nodes[upstream.index()]
            else {
                return None;
            };
            if !steps
                .iter()
                .all(|step| matches!(step.access(), StepAccess::Key(_) | StepAccess::Index(_)))
            {
                return None;
            }
            descend_element_spine(nodes, body, Some((upstream, steps.len())), consumer)
        }
        // A construction barrier: `map(f)` lowers to `[.[] | f]`, so this is the
        // arm that names the boundary "through map's lowered body".
        ProgramNode::CollectArray { body } => {
            descend_element_spine(nodes, (*body)?, outer, BoundaryConsumer::Collect)
        }
        // A `CountCollect` names no element boundary: its output is the count,
        // not an element, so the spine ends here like any other barrier.
        ProgramNode::CountCollect { .. } => None,
        // The boundary INSIDE a loop source — the projected fold's leading shape.
        ProgramNode::Reduce { source, .. } | ProgramNode::Foreach { source, .. } => {
            descend_element_spine(nodes, *source, outer, BoundaryConsumer::Fold)
        }
        ProgramNode::Bind { source, .. } => {
            descend_element_spine(nodes, *source, outer, BoundaryConsumer::Binding)
        }
        // A comma has two members and therefore no single boundary; a call's
        // arguments read the call's input rather than the spine; the remaining
        // families read the whole input authority through several child graphs.
        // A `ConstructObject` stays here after the member-prefix hoist
        // (`fuse_construct_object`): the hoist extracts only STATIC steps, so it
        // can never move a `.[]` out of a member, and the constructor's own
        // members remain several graphs with no single element spine.
        ProgramNode::Choice { .. }
        | ProgramNode::ConstructObject { .. }
        | ProgramNode::Call { .. }
        | ProgramNode::CallDef { .. }
        | ProgramNode::CallFilter { .. }
        | ProgramNode::Binary { .. }
        | ProgramNode::Concat { .. }
        | ProgramNode::Conditional { .. }
        | ProgramNode::Alternative { .. }
        | ProgramNode::Logical { .. }
        | ProgramNode::Try { .. }
        // A `Label`/`Break` names no element boundary: the barrier reads the
        // whole input authority through its body, and a break reads nothing.
        | ProgramNode::Label { .. }
        | ProgramNode::Break { .. }
        // An assignment names no element boundary either: its output CONTAINS
        // its whole input, so there is nothing to narrow and no per-element
        // spine to walk.
        | ProgramNode::Modify { .. }
        | ProgramNode::FactAssign { .. }
        // A counted stream names none: its source reads the whole input
        // authority, exactly as a loop's does.
        | ProgramNode::Counted { .. }
        | ProgramNode::Empty
        // An engine binding's body reads the whole input authority through one
        // graph, but the cursor is a runtime side effect — no element boundary
        // can be named through it, and a pull reads no document at all.
        | ProgramNode::EngineBind { .. }
        | ProgramNode::EnginePull { .. }
        | ProgramNode::EngineGenerator { .. }
        | ProgramNode::EngineRng { .. } => None,
    }
}

/// Derives the codec-pushdown prefix — [`analyze`] minus the element-boundary
/// naming, which is layered on top and changes nothing here.
fn analyze_pushdown(nodes: &[ProgramNode], root: ProgramNodeId) -> PushdownSplit {
    let mut cursor = root;
    // Whether the walk has passed a `CollectArray` barrier, which forbids an
    // OPTIONAL step in the prefix — see the `CollectArray` arm below.
    let mut collected = false;
    loop {
        match &nodes[cursor.index()] {
            // A current-start stage pushes its leading static steps (before the
            // first `.[]`) down; a literal-start stage ignores its input, so it
            // pushes NOTHING down (whole-document route — the input is still
            // decoded/validated, but the literal producer never navigates it).
            ProgramNode::Stage {
                start: StageStart::Current,
                steps,
            } => {
                // The prefix ends at the first step the codec cannot resolve
                // statically: a `.[]` iteration (the executor's fan-out), a
                // `.[$x]` dynamic index (resolved only at eval time, against the
                // env), a `..` recursive descent (it needs the whole subtree, so
                // only the prefix UPSTREAM of it pushes down — that is the win),
                // or a `.[a:b]` slice. All four bound the pushdown identically.
                //
                // This boundary is deliberately NOT moved. A trailing range
                // reaches the codec through the NAMED-boundary path instead
                // (`boundary_container_path`) and through the range-count row's
                // own lowering, both of which have a publish-nothing DECLINE arm
                // for a non-array container. This split has none — its
                // container-negative outcome is a located value the residual
                // runs — so widening it would turn `"abc"` at a slice step into
                // a pushed-down mismatch where the residual returns the
                // codepoint-cut string. Keeping it fixed also keeps the
                // prefix-scoping duty structural: no program's split,
                // requirement, or residual moves.
                //
                // BENEATH a `CollectArray` the prefix additionally stops at the
                // first OPTIONAL step, because a suppressed `?` means two
                // different things either side of the barrier: a pushed-down
                // suppression is `EngineRun::Suppressed` — the program publishes
                // nothing — while inside the collect it is a producer that
                // emitted nothing, and `[.a.b?]` over `{"a": 5}` is `[]`. Leaving
                // the flagged step in the residual is what keeps the `[]`.
                let prefix_len = if collected {
                    total_static_run_len(steps)
                } else {
                    static_run_len(steps)
                };
                return PushdownSplit {
                    entry: cursor,
                    prefix_len,
                    element: None,
                };
            }
            // The upstream spine leads to the first value producer; descend it.
            ProgramNode::FlatMap { upstream, .. } => cursor = *upstream,
            // A `label` is TRANSPARENT to its body's input: it evaluates the body
            // over its own input unchanged and re-emits the body's outputs, so the
            // authority the body reads IS the label's. Descending it is the same
            // move as a `FlatMap` upstream, and without it `label $o | .a.b` reads
            // the whole document to answer a two-step path.
            //
            // The barrier still stands at run time — the executor pushes the
            // `Label` frame and then evaluates the body, whose stage is the
            // recorded ENTRY, so the pushed-down prefix is skipped by node
            // identity exactly as it is under a `FlatMap`.
            ProgramNode::Label { body, .. } => cursor = *body,
            // A `CollectArray` is TRANSPARENT to its input for the same reason a
            // `Label` is: `[f]` evaluates `f` against the collect's own input, so
            // the barrier has exactly ONE producer and that producer's dot IS the
            // collect's dot. Descending it is the same move as a `FlatMap`
            // upstream, and without it `[.users[].id]` reads the whole document
            // to answer a demand whose container is one key deep.
            //
            // The barrier still stands at run time: the recorded ENTRY is the
            // body's stage, whose leading steps the executor skips by node
            // identity exactly as it does under a `FlatMap`, and the collect
            // itself is evaluated once — a collect on the SPINE is reached only
            // through `FlatMap` upstream edges, so its input is the document,
            // never a per-output rebinding.
            //
            // `[]` has no producer at all and stays with the zero-prefix arm.
            ProgramNode::CollectArray { body: Some(body) }
            | ProgramNode::CountCollect { body: Some(body) } => {
                collected = true;
                cursor = *body;
            }
            // A literal-start stage, a comma, a constructor, or a CALL on the spine
            // shares one document authority with no prefix extraction:
            // whole-document route (the comma-and-call law). A call reads the
            // whole input, so a call ON the spine contributes zero prefix; a
            // static prefix UPSTREAM of a call
            // (`.items | map(.id)`) still pushes down through the FlatMap arm above.
            // A `ConstructObject` reaches this arm only with NOTHING left to
            // extract: `fuse_construct_object` has already hoisted the static
            // prefix its members share into a `FlatMap` upstream stage, which the
            // arm above descends. What remains inside the node is a set of member
            // graphs that diverge at their first step, and the codec's exact-path
            // vocabulary cannot express their union — so this stays zero.
            // A `Variable`-start stage can never be the pushdown ENTRY: no binder
            // encloses the program root (there are no `--arg`-style outer
            // bindings), so a Variable start is always nested under a binder node
            // that already stopped the spine walk. It is classified here for
            // totality and pushes nothing down, exactly like a literal start.
            ProgramNode::Stage {
                start: StageStart::Literal(_) | StageStart::Variable(_),
                ..
            }
            | ProgramNode::Choice { .. }
            | ProgramNode::CollectArray { body: None }
            | ProgramNode::CountCollect { body: None }
            | ProgramNode::ConstructObject { .. }
            | ProgramNode::Call { .. }
            | ProgramNode::CallDef { .. }
            | ProgramNode::CallFilter { .. }
            // A `Binary` on the spine reads the whole input authority through both
            // operand graphs, so it contributes zero pushdown prefix. The
            // control-flow families (`Conditional`/`Alternative`/`Logical`) are the
            // same class: the condition, arms, and operands all read the whole
            // input authority, so each contributes zero prefix on the spine.
            | ProgramNode::Binary { .. }
            | ProgramNode::Concat { .. }
            | ProgramNode::Conditional { .. }
            | ProgramNode::Alternative { .. }
            | ProgramNode::Logical { .. }
            // A `Try` on the spine reads the whole input authority through its body
            // (and handler), so it contributes zero pushdown prefix.
            | ProgramNode::Try { .. }
            // A `ChainBody` is reached through the `?//` chain's arms, each of
            // which reads the whole input authority: zero pushdown prefix.
            | ProgramNode::ChainBody { .. }
            // A `Break` reads nothing, but it is not a path either, so it
            // contributes zero prefix. (A `Label` DESCENDS — see the arm above.)
            | ProgramNode::Break { .. }
            // An assignment's output contains its whole input, so it pushes
            // NOTHING down: the whole-document floor is the only sound route.
            | ProgramNode::Modify { .. }
            | ProgramNode::FactAssign { .. }
            // `Empty` reads nothing at all.
            | ProgramNode::Empty
            // The engine surface reads the whole input authority through the
            // binding's graphs, and a pull is a stateful side effect — zero
            // pushdown prefix, exactly like a call.
            | ProgramNode::EngineBind { .. }
            | ProgramNode::EnginePull { .. }
            | ProgramNode::EngineGenerator { .. }
            | ProgramNode::EngineRng { .. } => {
                return PushdownSplit {
                    entry: cursor,
                    prefix_len: 0,
                    element: None,
                };
            }
            // The binding family reads the outer dot through its SOURCE and,
            // for a binder, its BODY; a loop additionally reads it through
            // `INIT`. Its `update`/`extract` graphs do not — their dot is the
            // fold state. So when every OTHER outer-dot reader is
            // document-independent, the source alone reads the document and its
            // static path pushes down exactly as a bare stage's would
            // (`.catalog[500].id as $i | [$i, $i*2]` keeps the scoped route).
            // Otherwise the family shares one whole-document authority as
            // before. A static prefix UPSTREAM of one
            // (`.catalog | reduce .[] as $x (0; . + 1)`) still pushes down
            // through the `FlatMap` arm above either way.
            ProgramNode::Bind { source, body, .. } => {
                if reads_outer_dot(nodes, *body) || !resolves_completely(nodes, *source) {
                    return PushdownSplit {
                        entry: cursor,
                        prefix_len: 0,
                        element: None,
                    };
                }
                cursor = *source;
            }
            ProgramNode::Reduce { source, init, .. }
            | ProgramNode::Foreach { source, init, .. } => {
                if reads_outer_dot(nodes, *init) || !resolves_completely(nodes, *source) {
                    return PushdownSplit {
                        entry: cursor,
                        prefix_len: 0,
                        element: None,
                    };
                }
                cursor = *source;
            }
            // A counted stream reads the outer dot ONLY through its source (its
            // count came from an env slot the enclosing binder already wrote), so
            // it is the loop arm with no init to consult.
            ProgramNode::Counted { source, .. } => {
                if !resolves_completely(nodes, *source) {
                    return PushdownSplit {
                        entry: cursor,
                        prefix_len: 0,
                        element: None,
                    };
                }
                cursor = *source;
            }
        }
    }
}

/// Whether the codec can resolve the graph at `id` COMPLETELY: it is one
/// current-start stage of static forward steps and nothing else.
///
/// This is the entry condition for pushing a binder/loop SOURCE down, and it is
/// deliberately narrower than "has a static prefix". A source the codec resolves
/// completely becomes one located value the executor reads directly — the scoped
/// route's sweet spot. A source with a residual `.[]` after its prefix instead
/// FANS OUT over the located container, which for a container that is most of
/// the document trades a single-pass whole parse for validate-then-build and
/// loses. That cost is the standing scoped capability tax; declining the
/// fan-out shape keeps the tax off the scoped path.
///
/// An empty step list (`. as $x | …`) resolves completely and yields a
/// zero-length prefix, which is the whole-document route either way.
///
/// An OPTIONAL step disqualifies the source, and the word "completely" is why:
/// this contract promises the codec hands back ONE located value, and a `?`
/// step promises the opposite — a suppressed type mismatch resolves to NO
/// value at all. The executor sees that as "no document" and never runs the
/// program, which silently deletes every output the graph would have produced
/// from an EMPTY source. A `reduce` is exactly that graph: `reduce .a? as $x
/// (0; . + 1)` over `[1,2,3]` must answer `0` (the init, because the source is
/// empty) and answered nothing while this predicate admitted `?`. A binder
/// source survived the same pushdown only by luck — an empty binder source
/// really does produce no output — so the guard belongs here, on the promise
/// that is false, rather than on the one caller that noticed.
fn resolves_completely(nodes: &[ProgramNode], id: ProgramNodeId) -> bool {
    match &nodes[id.index()] {
        ProgramNode::Stage {
            start: StageStart::Current,
            steps,
        } => steps
            .iter()
            .all(|step| !step.is_optional() && matches!(step.access(), StepAccess::Key(_) | StepAccess::Index(_))),
        // Nothing else in the graph vocabulary is a codec-resolvable static
        // path: in particular, a shared `?//` chain body never is.
        _ => false,
    }
}

/// Whether the graph at `id` reads the value its enclosing scope calls dot.
///
/// The pushdown split needs this to know whether a `Bind`/`Reduce`/`Foreach`
/// SOURCE is the only graph reading the DOCUMENT: if the binder's body (or the
/// loop's init) never reads dot, the codec may resolve the source's static
/// prefix and hand the executor the located value, because nothing else in the
/// family will ask for the whole input.
///
/// It over-approximates deliberately — a `Call`, a `try` handler (whose dot is
/// the caught error value), and a variable-start stage's own postfix are all
/// reported as reading dot even when they cannot. Over-reporting only declines
/// a pushdown; UNDER-reporting would hand the executor the wrong dot, so every
/// arm is written to fail closed.
///
/// The dot-rebinding arms are exact, and they are the point: a `FlatMap`'s BODY
/// runs over its upstream's outputs, and a loop's `update`/`extract` run with
/// dot = the fold state, so none of the three can reach the outer dot.
pub(crate) fn reads_outer_dot(nodes: &[ProgramNode], id: ProgramNodeId) -> bool {
    match &nodes[id.index()] {
        ProgramNode::Stage { start, .. } => match start {
            StageStart::Current => true,
            // A literal or bound-variable producer ignores its input entirely.
            StageStart::Literal(_) | StageStart::Variable(_) => false,
        },
        // Only the UPSTREAM sees the outer dot; the body sees its outputs.
        ProgramNode::FlatMap { upstream, .. } => reads_outer_dot(nodes, *upstream),
        ProgramNode::Choice { left, right }
        | ProgramNode::Binary { left, right, .. }
        | ProgramNode::Alternative { left, right }
        | ProgramNode::Logical { left, right, .. } => {
            reads_outer_dot(nodes, *left) || reads_outer_dot(nodes, *right)
        }
        ProgramNode::Concat { parts } => parts.iter().any(|part| reads_outer_dot(nodes, *part)),
        ProgramNode::CollectArray { body }
        | ProgramNode::CountCollect { body } => {
            body.is_some_and(|body| reads_outer_dot(nodes, body))
        }
        ProgramNode::ConstructObject { members } => members.iter().any(|member| {
            reads_outer_dot(nodes, member.key) || reads_outer_dot(nodes, member.value)
        }),
        // A call evaluates over its own input, which IS the outer dot here; and
        // an assignment ALWAYS reads it, because the document it folds is the
        // dot itself, whatever its two child graphs happen to read.
        ProgramNode::Call { .. }
        | ProgramNode::CallDef { .. }
        | ProgramNode::CallFilter { .. }
        | ProgramNode::Modify { .. }
        | ProgramNode::FactAssign { .. }
        // A shared chain body reads whatever dot its arm handed it — the
        // chain's incoming dot or a raised value — so it reads the outer dot.
        // Deliberately NO recursion: the body is shared by every arm, and
        // recursing would revisit it once per arm (exponential for chains
        // nested in their own body). Over-reporting only declines a pushdown.
        | ProgramNode::ChainBody { .. } => true,
        ProgramNode::Conditional {
            condition,
            consequent,
            alternative,
        } => {
            reads_outer_dot(nodes, *condition)
                || reads_outer_dot(nodes, *consequent)
                || reads_outer_dot(nodes, *alternative)
        }
        // The handler's dot is the caught error value, not the outer dot; it is
        // still walked, which can only decline more.
        ProgramNode::Try { body, handler } => {
            reads_outer_dot(nodes, *body)
                || handler.is_some_and(|handler| reads_outer_dot(nodes, handler))
        }
        // A `label`'s body runs over the label's own dot, which IS the outer dot
        // — the barrier does not rebind it. Neither a `break` nor an `empty`
        // reads a dot at all.
        ProgramNode::Label { body, .. } => reads_outer_dot(nodes, *body),
        ProgramNode::Break { .. }
        | ProgramNode::Empty
        // A pull reads no dot (it ignores its input). The cursor bodies are
        // NOT walked — this arm declines `EngineGenerator`/`EngineRng`
        // without recursing into their init/update/extract graphs. That
        // skips nothing soundness-relevant: a cursor graph can only sit
        // under an engine binding, and every pushdown walk that reaches an
        // engine binding has already stopped — the spine's engine arm returns
        // a zero prefix outright, and a binding-family source must satisfy
        // `resolves_completely`, which no engine-surface node does.
        | ProgramNode::EnginePull { .. }
        | ProgramNode::EngineGenerator { .. }
        | ProgramNode::EngineRng { .. } => false,
        // A binder's source AND body both run over the outer dot; an ENGINE
        // binding's cursor graphs and body run over the binding-site dot the
        // same way.
        ProgramNode::Bind { source, body, .. }
        | ProgramNode::EngineBind { source, body, .. } => {
            reads_outer_dot(nodes, *source) || reads_outer_dot(nodes, *body)
        }
        // A loop's source and init run over the outer dot; `update`/`extract`
        // run with dot = the fold state.
        ProgramNode::Reduce { source, init, .. } | ProgramNode::Foreach { source, init, .. } => {
            reads_outer_dot(nodes, *source) || reads_outer_dot(nodes, *init)
        }
        // A counted stream reads the outer dot through its source alone — the
        // count is read from an env slot, never from dot.
        ProgramNode::Counted { source, .. } => reads_outer_dot(nodes, *source),
    }
}

#[cfg(test)]
mod tests {
    use super::{BoundaryConsumer, analyze, fuse_with_callables};
    use crate::program::{
        ObjectMemberNode, ProgramNode, ProgramNodeId, SliceBound, SliceBounds, StageStart, StageStep, StepAccess,
    };
    use alloc::boxed::Box;
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;
    use jqf_data::Value;

    fn key(name: &str, optional: bool) -> StageStep {
        StageStep::new(StepAccess::Key(String::from(name)), optional)
    }

    fn each(optional: bool) -> StageStep {
        StageStep::new(StepAccess::Each, optional)
    }

    /// Builds a `FlatMap(Stage(upstream), Stage(body))` arena — the shape
    /// lowering emits for one pipe — and returns it with the `FlatMap` root.
    fn flatmap(upstream: Vec<StageStep>, body: Vec<StageStep>) -> (Vec<ProgramNode>, ProgramNodeId) {
        let nodes = vec![
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: upstream,
            },
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: body,
            },
            ProgramNode::FlatMap {
                upstream: ProgramNodeId::from_index(0).expect("id"),
                body: ProgramNodeId::from_index(1).expect("id"),
            },
        ];
        (nodes, ProgramNodeId::from_index(2).expect("root"))
    }

    /// Extracts a fused stage's `(key, optional)` witnesses.
    fn fused_keys(nodes: &[ProgramNode], root: ProgramNodeId) -> Vec<(String, bool)> {
        let ProgramNode::Stage { steps, .. } = &nodes[root.index()] else {
            panic!("fused root must be a Stage");
        };
        steps
            .iter()
            .map(|step| match step.access() {
                StepAccess::Key(name) => (name.clone(), step.is_optional()),
                StepAccess::Index(_)
                | StepAccess::Each
                | StepAccess::Descend
                | StepAccess::Slice(_)
                | StepAccess::DynVar(_)
                | StepAccess::NodeAccessor(_)
                | StepAccess::Attribute(_)
                | StepAccess::DynNodeAccessor(_)
                | StepAccess::DynAttribute(_) => {
                    panic!("expected key")
                }
            })
            .collect()
    }

    #[test]
    fn fusion_concatenates_stages_and_yields_a_single_stage() {
        let (mut nodes, root) = flatmap(vec![key("a", false)], vec![key("b", false)]);
        let (fused, fused_root, _) = fuse_with_callables(&mut nodes, root, &[]).expect("fuses");
        assert_eq!(fused.len(), 1, "fusion must collapse to a single Stage node");
        assert!(matches!(fused[fused_root.index()], ProgramNode::Stage { .. }));
        assert_eq!(
            fused_keys(&fused, fused_root),
            [(String::from("a"), false), (String::from("b"), false)]
        );
    }

    #[test]
    fn fusion_preserves_per_step_flags_and_their_asymmetry() {
        // `.a? | .b` flags the first fused step; `.a | .b?` flags the second.
        let (mut left_nodes, left_root) = flatmap(vec![key("a", true)], vec![key("b", false)]);
        let (left, left_fused, _) = fuse_with_callables(&mut left_nodes, left_root, &[]).expect("fuses");
        assert_eq!(
            fused_keys(&left, left_fused),
            [(String::from("a"), true), (String::from("b"), false)]
        );

        let (mut right_nodes, right_root) = flatmap(vec![key("a", false)], vec![key("b", true)]);
        let (right, right_fused, _) = fuse_with_callables(&mut right_nodes, right_root, &[]).expect("fuses");
        assert_eq!(
            fused_keys(&right, right_fused),
            [(String::from("a"), false), (String::from("b"), true)]
        );
    }

    #[test]
    fn nested_pipe_chain_fuses_in_authored_order() {
        // Arena for `.a | (.b | .c)`: FlatMap(Stage(a), FlatMap(Stage(b), Stage(c))).
        let mut nodes = vec![
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: vec![key("a", false)],
            },
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: vec![key("b", false)],
            },
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: vec![key("c", false)],
            },
            ProgramNode::FlatMap {
                upstream: ProgramNodeId::from_index(1).expect("id"),
                body: ProgramNodeId::from_index(2).expect("id"),
            },
            ProgramNode::FlatMap {
                upstream: ProgramNodeId::from_index(0).expect("id"),
                body: ProgramNodeId::from_index(3).expect("id"),
            },
        ];
        let root = ProgramNodeId::from_index(4).expect("root");
        let (fused, fused_root, _) = fuse_with_callables(&mut nodes, root, &[]).expect("fuses");
        assert_eq!(
            fused_keys(&fused, fused_root),
            [
                (String::from("a"), false),
                (String::from("b"), false),
                (String::from("c"), false)
            ]
        );
    }

    /// Builds a single-stage arena directly (no pipe) from `steps` — the shape
    /// `.a[].b`-style chains lower to before fusion is even needed.
    fn stage(steps: Vec<StageStep>) -> (Vec<ProgramNode>, ProgramNodeId) {
        (
            vec![ProgramNode::Stage {
                start: StageStart::Current,
                steps,
            }],
            ProgramNodeId::ROOT,
        )
    }

    #[test]
    fn fused_empty_stage_is_whole_document() {
        let mut nodes = vec![ProgramNode::Stage {
            start: StageStart::Current,
            steps: Vec::new(),
        }];
        let (fused, root, _) = fuse_with_callables(&mut nodes, ProgramNodeId::ROOT, &[]).expect("fuses");
        let split = analyze(&fused, root);
        assert!(split.is_whole_document());
        assert_eq!(split.prefix_len(), 0);
    }

    #[test]
    fn fused_forward_path_pushes_the_whole_path_down() {
        // A path with no `Each` pushes every step down; residual is identity.
        let (mut nodes, root) = flatmap(vec![key("a", false)], vec![key("b", false)]);
        let (fused, fused_root, _) = fuse_with_callables(&mut nodes, root, &[]).expect("fuses");
        let split = analyze(&fused, fused_root);
        assert!(!split.is_whole_document());
        assert_eq!(split.prefix_len(), 2);
    }

    #[test]
    fn a_label_is_transparent_to_its_body_s_prefix() {
        // `label $o | .a.b`: the label evaluates its body over its OWN input
        // unchanged, so the authority the body reads IS the label's and the
        // two-step path pushes down whole. Treating the label as a zero-prefix
        // terminal instead makes it read the entire document to answer `.a.b`.
        let (mut nodes, root) = stage(vec![key("a", false), key("b", false)]);
        nodes.push(ProgramNode::Label { slot: 0, body: root });
        let labelled = ProgramNodeId::from_index(nodes.len() - 1).expect("id");
        let split = analyze(&nodes, labelled);
        assert!(!split.is_whole_document());
        assert_eq!(split.prefix_len(), 2);
        // The ENTRY is the body's stage, so the executor skips the pushed-down
        // prefix by node identity with the `Label` frame still pushed above it.
        assert_eq!(split.entry(), root);
    }

    #[test]
    fn a_break_pushes_nothing_down() {
        // A `break` reads nothing, but it is not a path either: it contributes
        // no prefix, and it must NOT inherit the transparency of the `label`
        // that encloses it.
        let nodes = vec![ProgramNode::Break { slot: 0 }];
        let split = analyze(&nodes, ProgramNodeId::ROOT);
        assert!(split.is_whole_document());
        assert_eq!(split.prefix_len(), 0);
    }

    #[test]
    fn leading_each_pushes_nothing_down() {
        // `.[].b`: the prefix boundary is 0, so the whole stage is residual and
        // the codec reads the whole document.
        let (nodes, root) = stage(vec![each(false), key("b", false)]);
        let split = analyze(&nodes, root);
        assert!(split.is_whole_document());
        assert_eq!(split.prefix_len(), 0);
    }

    #[test]
    fn prefixed_each_pushes_the_pre_each_prefix_down() {
        // `.a[].b`: the prefix is `.a` (one step), the residual is `[Each, b]`.
        let (nodes, root) = stage(vec![key("a", false), each(false), key("b", false)]);
        let split = analyze(&nodes, root);
        assert!(!split.is_whole_document());
        assert_eq!(split.prefix_len(), 1);
    }

    #[test]
    fn only_the_first_each_bounds_the_prefix() {
        // `.a[][]`: the first `Each` at index 1 bounds the prefix; the second is
        // inside the residual, not a further pushdown.
        let (nodes, root) = stage(vec![key("a", false), each(false), each(false)]);
        assert_eq!(analyze(&nodes, root).prefix_len(), 1);
    }

    /// Whether any `FlatMap` in the arena still has two `Stage` children — the
    /// negation of path-normal form.
    fn has_stage_stage_flatmap(nodes: &[ProgramNode]) -> bool {
        nodes.iter().any(|node| match node {
            ProgramNode::FlatMap { upstream, body } => {
                matches!(nodes[upstream.index()], ProgramNode::Stage { .. })
                    && matches!(nodes[body.index()], ProgramNode::Stage { .. })
            }
            _ => false,
        })
    }

    #[test]
    fn fusion_yields_path_normal_form_stage_stage_fuses_choice_survives() {
        // The sanctioned narrowing: `FlatMap(Stage, Stage)` still collapses to one
        // Stage (the pipe/path guarantee), but a `Choice` on a side blocks fusion.
        // `(.a, .b).c` = FlatMap(Choice(Stage[a], Stage[b]), Stage[c]) survives as
        // a path-normal graph: no Stage∘Stage FlatMap remains, but the FlatMap and
        // Choice do.
        let mut nodes = vec![
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: vec![key("a", false)],
            },
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: vec![key("b", false)],
            },
            ProgramNode::Choice {
                left: ProgramNodeId::from_index(0).expect("id"),
                right: ProgramNodeId::from_index(1).expect("id"),
            },
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: vec![key("c", false)],
            },
            ProgramNode::FlatMap {
                upstream: ProgramNodeId::from_index(2).expect("id"),
                body: ProgramNodeId::from_index(3).expect("id"),
            },
        ];
        let root = ProgramNodeId::from_index(4).expect("root");
        let (fused, fused_root, _) = fuse_with_callables(&mut nodes, root, &[]).expect("fuses");
        assert!(
            !has_stage_stage_flatmap(&fused),
            "path-normal form leaves no Stage∘Stage FlatMap"
        );
        let ProgramNode::FlatMap { upstream, body } = &fused[fused_root.index()] else {
            panic!("the choice-coupled FlatMap must survive fusion");
        };
        assert!(matches!(fused[upstream.index()], ProgramNode::Choice { .. }));
        assert!(matches!(fused[body.index()], ProgramNode::Stage { .. }));
    }

    #[test]
    fn nested_stage_stage_flatmap_under_a_choice_still_fuses() {
        // `(.a | .b), .c` = Choice(FlatMap(Stage[a], Stage[b]), Stage[c]): the
        // Stage∘Stage FlatMap inside the left member fuses to `Stage[a, b]`, while
        // the Choice survives — fusion reaches inside graph nodes.
        let mut nodes = vec![
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: vec![key("a", false)],
            },
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: vec![key("b", false)],
            },
            ProgramNode::FlatMap {
                upstream: ProgramNodeId::from_index(0).expect("id"),
                body: ProgramNodeId::from_index(1).expect("id"),
            },
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: vec![key("c", false)],
            },
            ProgramNode::Choice {
                left: ProgramNodeId::from_index(2).expect("id"),
                right: ProgramNodeId::from_index(3).expect("id"),
            },
        ];
        let root = ProgramNodeId::from_index(4).expect("root");
        let (fused, fused_root, _) = fuse_with_callables(&mut nodes, root, &[]).expect("fuses");
        assert!(!has_stage_stage_flatmap(&fused));
        let ProgramNode::Choice { left, right } = &fused[fused_root.index()] else {
            panic!("the top-level Choice must survive");
        };
        // The left member fused `.a | .b` to `Stage[a, b]`.
        assert_eq!(
            fused_keys(&fused, *left),
            [(String::from("a"), false), (String::from("b"), false)]
        );
        assert!(matches!(fused[right.index()], ProgramNode::Stage { .. }));
    }

    /// Builds a `reduce SOURCE as $x (0; .)` arena and returns it with the
    /// `Reduce` root — the shape whose SOURCE the pushdown split may descend.
    fn reduce_over(source: Vec<StageStep>) -> (Vec<ProgramNode>, ProgramNodeId) {
        let nodes = vec![
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: source,
            },
            // The init and update are document-independent, which is what lets
            // the split consider the source at all.
            ProgramNode::Stage {
                start: StageStart::Literal(Value::Null),
                steps: Vec::new(),
            },
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: Vec::new(),
            },
            ProgramNode::Reduce {
                source: ProgramNodeId::from_index(0).expect("id"),
                slot: 0,
                init: ProgramNodeId::from_index(1).expect("id"),
                update: ProgramNodeId::from_index(2).expect("id"),
                keyed_collect: None,
            },
        ];
        (nodes, ProgramNodeId::from_index(3).expect("root"))
    }

    #[test]
    fn a_static_loop_source_pushes_its_whole_path_down() {
        // The engagement witness for the arm the next test guards: a source of
        // required keys IS resolved completely, so the split descends into it
        // and the codec locates `.a.b`.
        let (nodes, root) = reduce_over(vec![key("a", false), key("b", false)]);
        let split = analyze(&nodes, root);
        assert_eq!(split.prefix_len(), 2);
        assert_eq!(split.entry(), ProgramNodeId::from_index(0).expect("id"));
    }

    #[test]
    fn an_optional_loop_source_declines_the_pushdown() {
        // `reduce .a? as $x (…)`: the `?` can suppress a mismatch and locate
        // NOTHING, which the executor reads as "no document" — deleting the
        // init an empty source must still emit. The split must stop at the
        // `Reduce` with a zero-length prefix (the whole-document route).
        for source in [
            vec![key("a", true)],
            vec![key("a", false), key("b", true)],
            vec![key("a", true), key("b", false)],
        ] {
            let (nodes, root) = reduce_over(source);
            let split = analyze(&nodes, root);
            assert_eq!(split.prefix_len(), 0, "an optional step must not push down");
            assert_eq!(split.entry(), root, "the entry stays the whole loop");
        }
    }

    #[test]
    fn a_named_boundary_leaves_the_pushdown_prefix_untouched() {
        // The boundary naming is layered ON the split, never into it: `.a[].b` keeps the
        // identical entry and prefix it had, and additionally names its boundary.
        let (nodes, root) = stage(vec![key("a", false), each(false), key("b", false)]);
        let split = analyze(&nodes, root);
        assert_eq!(split.prefix_len(), 1);
        assert_eq!(split.entry(), root);
        let boundary = split.element_boundary().expect("a `.[]` names a boundary");
        assert_eq!(boundary.stage(), root);
        assert_eq!(boundary.stage_prefix_len(), 1);
        assert_eq!(boundary.consumer(), BoundaryConsumer::Residual);
        assert!(boundary.is_sole());
    }

    #[test]
    fn nested_iteration_names_the_first_boundary_and_marks_it_not_sole() {
        // `.a[][]`: the boundary is the FIRST `.[]` (offset 1), and the second one
        // makes it non-sole — the flag the classifier reads to keep its join.
        let (nodes, root) = stage(vec![key("a", false), each(false), each(false)]);
        let boundary = analyze(&nodes, root).element_boundary().expect("boundary");
        assert_eq!(boundary.stage_prefix_len(), 1);
        assert!(!boundary.is_sole());
    }

    #[test]
    fn a_born_p2_step_before_the_each_names_no_boundary() {
        // A `..` before the `.[]`: recursive descent is P2 vocabulary the codec
        // cannot resolve as a container path, so there is no boundary to name —
        // declined, never approximated. (A NORMALIZING slice before the `.[]`
        // is the OPPOSITE: [`a_normalizing_slice_before_the_each_names_the_boundary`]
        // pins the admitted shape.)
        let (nodes, root) = stage(vec![StageStep::new(StepAccess::Descend, false), each(false)]);
        assert!(analyze(&nodes, root).element_boundary().is_none());
    }

    #[test]
    fn a_normalizing_slice_before_the_each_names_the_boundary() {
        // `.catalog[1000:20000][]`: the walk admits exactly ONE trailing
        // container-path slice whose bounds normalize, naming the boundary at
        // the `.[]` — the codec resolves `.catalog` plus the range.
        let (nodes, root) = stage(vec![
            key("catalog", false),
            StageStep::new(
                StepAccess::Slice(Box::new(SliceBounds {
                    start: SliceBound::Literal(Value::Number(
                        jqf_data::Number::try_json_literal("1000").expect("start"),
                    )),
                    end: SliceBound::Literal(Value::Number(jqf_data::Number::try_json_literal("20000").expect("end"))),
                })),
                false,
            ),
            each(false),
        ]);
        let boundary = analyze(&nodes, root).element_boundary().expect("boundary");
        assert_eq!(boundary.stage(), root);
        assert_eq!(
            boundary.stage_prefix_len(),
            2,
            "the container path is catalog plus the range; the `.[]` sits at offset 2"
        );
    }

    /// Builds a `{k0: PATH, k1: PATH, …}` arena: one steps-free literal key per
    /// member (what a static key lowers to) paired with a current-start path.
    fn construct_object(members: Vec<Vec<StageStep>>) -> (Vec<ProgramNode>, ProgramNodeId) {
        let mut nodes = Vec::new();
        let mut placed = Vec::new();
        for steps in members {
            nodes.push(ProgramNode::Stage {
                start: StageStart::Literal(Value::Null),
                steps: Vec::new(),
            });
            let key = ProgramNodeId::from_index(nodes.len() - 1).expect("id");
            nodes.push(ProgramNode::Stage {
                start: StageStart::Current,
                steps,
            });
            let value = ProgramNodeId::from_index(nodes.len() - 1).expect("id");
            placed.push(ObjectMemberNode {
                key,
                value,
                static_key: None,
            });
        }
        nodes.push(ProgramNode::ConstructObject { members: placed });
        let root = ProgramNodeId::from_index(nodes.len() - 1).expect("root");
        (nodes, root)
    }

    #[test]
    fn a_sole_constructor_member_hoists_its_whole_static_path() {
        // `{a: .p.x}`: the join over one member is that member's own path, so the
        // codec locates `.p.x` and the residual constructor reads identity —
        // instead of the whole-input authority a constructor used to take.
        let (mut nodes, root) = construct_object(vec![vec![key("p", false), key("x", false)]]);
        let (fused, fused_root, _) = fuse_with_callables(&mut nodes, root, &[]).expect("fuses");
        let split = analyze(&fused, fused_root);
        assert!(!split.is_whole_document());
        assert_eq!(split.prefix_len(), 2);
    }

    #[test]
    fn constructor_members_hoist_the_prefix_they_share() {
        // `{a: .p.x, b: .p.y}`: the members diverge at their second step, so the
        // join is `.p` — the exact path the codec CAN express for both.
        let (mut nodes, root) = construct_object(vec![
            vec![key("p", false), key("x", false)],
            vec![key("p", false), key("y", false)],
        ]);
        let (fused, fused_root, _) = fuse_with_callables(&mut nodes, root, &[]).expect("fuses");
        let split = analyze(&fused, fused_root);
        assert_eq!(split.prefix_len(), 1);
    }

    #[test]
    fn constructor_members_diverging_at_the_first_step_hoist_nothing() {
        // `{a: .p, b: .q}`: an empty join is the whole-input authority, which is
        // the constructor's standing classification.
        let (mut nodes, root) = construct_object(vec![vec![key("p", false)], vec![key("q", false)]]);
        let (fused, fused_root, _) = fuse_with_callables(&mut nodes, root, &[]).expect("fuses");
        assert!(analyze(&fused, fused_root).is_whole_document());
    }

    #[test]
    fn a_hoist_stops_at_the_first_step_the_codec_cannot_resolve() {
        // `{a: .p[].x}`: the join is the member's whole step list, but only its
        // leading STATIC run is a path — the same truncation a bare stage's
        // pushdown takes, and the reason the hoist can never move a `.[]` out of
        // a member.
        let (mut nodes, root) = construct_object(vec![vec![key("p", false), each(false), key("x", false)]]);
        let (fused, fused_root, _) = fuse_with_callables(&mut nodes, root, &[]).expect("fuses");
        let split = analyze(&fused, fused_root);
        assert_eq!(split.prefix_len(), 1);
    }

    #[test]
    fn a_member_producer_the_hoist_cannot_prove_total_declines_it() {
        // `{a: (.p.x, .p.y)}`: a comma member can emit zero or many outputs, so
        // hoisting `.p` past it could move a raise ACROSS a member that publishes
        // nothing. The whole constructor keeps its whole-input authority.
        let (mut nodes, _) = construct_object(vec![vec![key("p", false), key("x", false)]]);
        nodes.push(ProgramNode::Stage {
            start: StageStart::Current,
            steps: vec![key("p", false), key("y", false)],
        });
        let right = ProgramNodeId::from_index(nodes.len() - 1).expect("id");
        nodes.push(ProgramNode::Choice {
            left: ProgramNodeId::from_index(1).expect("id"),
            right,
        });
        let choice = ProgramNodeId::from_index(nodes.len() - 1).expect("id");
        nodes.push(ProgramNode::ConstructObject {
            members: vec![ObjectMemberNode {
                key: ProgramNodeId::from_index(0).expect("id"),
                value: choice,
                static_key: None,
            }],
        });
        let root = ProgramNodeId::from_index(nodes.len() - 1).expect("root");
        let (fused, fused_root, _) = fuse_with_callables(&mut nodes, root, &[]).expect("fuses");
        assert!(analyze(&fused, fused_root).is_whole_document());
    }

    #[test]
    fn a_hoist_joins_only_steps_whose_optional_flags_agree() {
        // `{a: .p?.x, b: .p.y}`: the flag decides whether a mismatch SUPPRESSES
        // the constructor or aborts it, so two spellings of `.p` do not join.
        let (mut nodes, root) = construct_object(vec![
            vec![key("p", true), key("x", false)],
            vec![key("p", false), key("y", false)],
        ]);
        let (fused, fused_root, _) = fuse_with_callables(&mut nodes, root, &[]).expect("fuses");
        assert!(analyze(&fused, fused_root).is_whole_document());
    }

    #[test]
    fn top_level_choice_is_whole_document() {
        // `.a, .b`: the spine reaches a Choice, so nothing is pushed down.
        let nodes = vec![
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: vec![key("a", false)],
            },
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: vec![key("b", false)],
            },
            ProgramNode::Choice {
                left: ProgramNodeId::from_index(0).expect("id"),
                right: ProgramNodeId::from_index(1).expect("id"),
            },
        ];
        let root = ProgramNodeId::from_index(2).expect("root");
        let split = analyze(&nodes, root);
        assert!(split.is_whole_document());
        assert_eq!(split.prefix_len(), 0);
        assert_eq!(split.entry(), root);
    }

    #[test]
    fn prefix_inside_a_flatmap_upstream_is_pushed_down() {
        // `.a[] | (.b, .c)` = FlatMap(Stage[a, Each], Choice): the spine descends
        // the FlatMap upstream to the entry stage, pushing `.a` (one step, before
        // the `Each`) down. The entry is the upstream stage, not the root.
        let entry = ProgramNodeId::from_index(0).expect("id");
        let nodes = vec![
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: vec![key("a", false), each(false)],
            },
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: vec![key("b", false)],
            },
            ProgramNode::Stage {
                start: StageStart::Current,
                steps: vec![key("c", false)],
            },
            ProgramNode::Choice {
                left: ProgramNodeId::from_index(1).expect("id"),
                right: ProgramNodeId::from_index(2).expect("id"),
            },
            ProgramNode::FlatMap {
                upstream: entry,
                body: ProgramNodeId::from_index(3).expect("id"),
            },
        ];
        let root = ProgramNodeId::from_index(4).expect("root");
        let split = analyze(&nodes, root);
        assert!(!split.is_whole_document());
        assert_eq!(split.prefix_len(), 1);
        assert_eq!(split.entry(), entry);
    }
}
