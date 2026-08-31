//! The prune-tree fact: which parts of the DOCUMENT a whole-document program
//! can actually read.
//!
//! A FORWARD walk over the fused arena tracks, per node, which document
//! subtrees the node's OUTPUT values may alias (a small set of root-anchored
//! paths over `Key`/`Element` segments). Wherever a document value's PAYLOAD
//! is observed — published, folded into arithmetic, captured by a builtin,
//! rendered into an error — the walk CONTRIBUTES its path to the tree, and the
//! codec keeps that subtree whole. A member of a non-contributed position is
//! never observed by the program, so a decode may omit it; delivering MORE
//! than the tree names is always sound (monotonicity), which is why the tree
//! travels as an optional requirement HINT a codec is free to ignore.
//!
//! The call table is the REGISTRY LAW: every builtin overload's
//! [`DemandTransfer`] declaration drives the transfer here exactly as it
//! drives `projection.rs`'s backward classifier, so a conservatively declared
//! builtin is conservative in both lattices by construction.
//!
//! Deliberate v1 collapses, each sound in the over-keeping direction:
//!
//! - No structure-only ("shell") tier: `length`/`keys`/`type` on a document
//!   value keep its subtree whole instead of a skeleton.
//! - A static `.[i]` index, a dynamic `.[$k]` member, and `..` keep their
//!   container whole. Arrays NEVER omit elements (omission would shift
//!   indices and counts); pruning inside an array happens per element
//!   through the shared element node.
//! - Scalars at kept positions are always delivered verbatim, so type errors
//!   and truthiness reads on them are identical to the floor's.
//! - `input`/`inputs` (and every `~cursor` engine binding) decline the ROOT
//!   tree: a pulled document's demand is not the root's. The walk still
//!   records a SECOND tree for the pulled-record demand (the `$r` slot in
//!   `reduce inputs as $r (0; . + $r.v)`), which the record `NullFirst`
//!   drive may attach as a per-record prune hint. Engine-binding `~cursor`
//!   forms still decline both trees.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use crate::program::{ProgramNode, ProgramNodeId, SliceBound, StageStart, StepAccess, VarSlot};
use jqf_builtins::registry::{self, DemandTransfer};

/// Deeper contributions widen to the whole subtree at the cap instead of
/// growing an unbounded tree.
const MAX_PRUNE_DEPTH: usize = 32;
/// Total tree-node budget; exhaustion widens the current position, never
/// declines the walk.
const MAX_PRUNE_NODES: usize = 512;

/// Total node visits one walk may spend before declining. Real programs walk
/// each arena node a handful of times; only pathological shared-subgraph
/// shapes (nested `?//` bodies) approach this, and they decline in
/// microseconds instead of recursing exponentially.
const MAX_PRUNE_WALK_STEPS: usize = 65_536;
/// An outcome aliasing more document positions than this is consumed whole.
const MAX_OUTCOME_PATHS: usize = 8;

/// One node of the kept-subtree selection over the document root.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PruneNode {
    /// The whole subtree at this position is kept (a contribution leaf).
    all: bool,
    /// Demand shared by EVERY child value (`.[]` over an array's elements or
    /// an object's member values).
    element: Option<Box<PruneNode>>,
    /// Demand on exactly-named object members.
    keys: BTreeMap<String, PruneNode>,
}

impl PruneNode {
    /// Whether the whole subtree at this position must be delivered.
    pub(crate) const fn is_all(&self) -> bool {
        self.all
    }

    /// The shared every-child demand, when one exists.
    pub(crate) fn element(&self) -> Option<&PruneNode> {
        self.element.as_deref()
    }

    /// The exactly-named member demands.
    pub(crate) fn keys(&self) -> &BTreeMap<String, PruneNode> {
        &self.keys
    }

    /// Names one kept member (the count prune builder).
    pub(crate) fn with_key(mut self, key: String, child: PruneNode) -> Self {
        self.keys.insert(key, child);
        self
    }

    /// Names the shared every-child demand (the count prune's element spine).
    pub(crate) fn with_element(mut self, element: PruneNode) -> Self {
        self.element = Some(Box::new(element));
        self
    }

    /// The whole-subtree-at-this-position leaf (the count probe's tail).
    pub(crate) fn all() -> Self {
        Self {
            all: true,
            ..Self::default()
        }
    }
}

/// One step of a root-anchored document path.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Seg {
    Key(String),
    Element,
    /// The pulled-record document root (`input`/`inputs`). Only legal as a
    /// path's first segment; later contribution strips it and writes the
    /// remainder into [`Walker::pull_tree`].
    Pull,
}

/// A document position the value under consideration may alias.
type Path = Vec<Seg>;

/// The document positions a node's OUTPUT values may alias. Empty means the
/// output carries no un-contributed document payload (a constructed value, a
/// scalar product, or data already kept whole).
type Outcome = Vec<Path>;

/// Derives the kept-subtree trees for a whole-document program and for a pulled
/// record, sharing one walk.
pub(crate) fn prune_trees(
    nodes: &[ProgramNode],
    root: ProgramNodeId,
    slots: u32,
) -> (Option<PruneNode>, Option<PruneNode>) {
    let walker = walk_program(nodes, root, slots);
    let root_tree = if walker.declined || walker.saw_input_pull || walker.tree.all {
        None
    } else {
        let mut tree = walker.tree;
        let mut merge_budget = MAX_PRUNE_NODES * 8;
        absorb_element_into_keys(&mut tree, &mut merge_budget);
        Some(tree)
    };
    let pulled = if walker.declined || !walker.saw_input_pull || walker.pull_tree.all {
        None
    } else {
        let mut tree = walker.pull_tree;
        let mut merge_budget = MAX_PRUNE_NODES * 8;
        absorb_element_into_keys(&mut tree, &mut merge_budget);
        Some(tree)
    };
    (root_tree, pulled)
}

fn walk_program(nodes: &[ProgramNode], root: ProgramNodeId, slots: u32) -> Walker<'_> {
    let input_pull = [
        registry::resolve_builtin("input", 0).map(|record| record.id.get()),
        registry::resolve_builtin("inputs", 0).map(|record| record.id.get()),
    ];
    let mut walker = Walker {
        nodes,
        tree: PruneNode::default(),
        pull_tree: PruneNode::default(),
        slots: alloc::vec![Vec::new(); slots as usize],
        visiting: Vec::new(),
        node_budget: MAX_PRUNE_NODES,
        step_budget: MAX_PRUNE_WALK_STEPS,
        declined: false,
        saw_input_pull: false,
        input_pull,
    };
    let published = walker.walk(root, &alloc::vec![Vec::new()]);
    walker.consume(&published);
    walker
}

/// Folds each node's shared `element` demand INTO its named-key children, so a
/// decode-time member lookup is hit-or-element with no join: `.users.name,
/// .users[].id` must deliver `name` under BOTH its named demand and the
/// every-child demand. Runs bottom-up once, after the walk; a merge blowing
/// the budget widens the node to `all` (sound, over-keeping).
fn absorb_element_into_keys(node: &mut PruneNode, budget: &mut usize) {
    if node.all {
        return;
    }
    if let Some(element) = node.element.as_deref()
        && !node.keys.is_empty()
    {
        let element = element.clone();
        for child in node.keys.values_mut() {
            merge_into(child, &element, budget);
        }
    }
    if *budget == 0 {
        node.all = true;
        node.element = None;
        node.keys.clear();
        return;
    }
    if let Some(element) = node.element.as_deref_mut() {
        absorb_element_into_keys(element, budget);
    }
    for child in node.keys.values_mut() {
        absorb_element_into_keys(child, budget);
    }
}

/// Tree join: after it, `into` keeps everything either side kept.
fn merge_into(into: &mut PruneNode, from: &PruneNode, budget: &mut usize) {
    if into.all {
        return;
    }
    if from.all || *budget == 0 {
        into.all = true;
        into.element = None;
        into.keys.clear();
        return;
    }
    if let Some(from_element) = from.element.as_deref() {
        let into_element = into.element.get_or_insert_with(|| {
            *budget = budget.saturating_sub(1);
            Box::default()
        });
        merge_into(into_element, from_element, budget);
    }
    for (name, from_child) in &from.keys {
        if !into.keys.contains_key(name) {
            *budget = budget.saturating_sub(1);
            into.keys.insert(name.clone(), PruneNode::default());
        }
        let into_child = into.keys.get_mut(name).expect("inserted above when absent");
        merge_into(into_child, from_child, budget);
    }
}

/// Walks `path` through the tree, inserting nodes under the budget law, and
/// answers the leaf the path names — or `None` when the walk ended early
/// (the position was already kept whole, a budget exhaustion collapsed the
/// subtree, or a nested Pull kept it whole). Both [`contribute`] and
/// [`contribute_spine`] are this one walk; they differ only in what happens
/// to the leaf.
fn walk_prune_path<'a>(tree: &'a mut PruneNode, budget: &mut usize, path: &[Seg]) -> Option<&'a mut PruneNode> {
    let mut node = tree;
    for seg in path {
        if node.all {
            return None;
        }
        node = match seg {
            Seg::Key(name) => {
                if !node.keys.contains_key(name) {
                    if *budget == 0 {
                        node.all = true;
                        node.element = None;
                        node.keys.clear();
                        return None;
                    }
                    *budget -= 1;
                    node.keys.insert(name.clone(), PruneNode::default());
                }
                node.keys.get_mut(name).expect("inserted above when absent")
            }
            Seg::Element => {
                if node.element.is_none() {
                    if *budget == 0 {
                        node.all = true;
                        node.keys.clear();
                        return None;
                    }
                    *budget -= 1;
                    node.element = Some(Box::default());
                }
                node.element.as_mut().expect("inserted above when absent")
            }
            // Origin-only. A nested Pull is a walk bug; keep whole.
            Seg::Pull => {
                node.all = true;
                node.element = None;
                node.keys.clear();
                return None;
            }
        };
    }
    Some(node)
}

/// Inserts one kept path into the tree, widening to `all` at the leaf. A
/// budget exhaustion widens AT the current position — sound (over-keeping),
/// never a decline.
fn contribute(tree: &mut PruneNode, budget: &mut usize, path: &[Seg]) {
    let Some(node) = walk_prune_path(tree, budget, path) else {
        return;
    };
    node.all = true;
    // Children below an `all` node are redundant.
    node.element = None;
    node.keys.clear();
}

/// Inserts the SPINE of a navigated path without keeping any leaf payload:
/// every position a program steps THROUGH must exist in the pruned document
/// (an omitted spine would turn `.orders[]` over an input-ignoring body into
/// iteration over `null`), but its unread members may still prune away.
fn contribute_spine(tree: &mut PruneNode, budget: &mut usize, path: &[Seg]) {
    let _ = walk_prune_path(tree, budget, path);
}

/// The forward walk's state: the arena, the accumulating tree, and the
/// per-binder-slot outcome table.
struct Walker<'program> {
    nodes: &'program [ProgramNode],
    tree: PruneNode,
    /// Kept-subtree selection over a pulled `input`/`inputs` document.
    pull_tree: PruneNode,
    slots: Vec<Outcome>,
    /// `CallDef` BODY ids currently being walked — the recursion guard.
    visiting: Vec<ProgramNodeId>,
    node_budget: usize,
    /// Total walk steps remaining. The arena is a DAG (`?//` shares each
    /// chain body across its pattern arms behind `ChainBody`), so a naive
    /// descent revisits shared subtrees once per incoming edge — exponential
    /// in nesting depth. Exhaustion declines the whole tree.
    step_budget: usize,
    declined: bool,
    /// Whether the walk observed an `input`/`inputs` pull.
    saw_input_pull: bool,
    /// The `input`/`inputs` overload ids, resolved once.
    input_pull: [Option<u16>; 2],
}

impl<'program> Walker<'program> {
    /// Marks every position in `outcome` as read whole.
    fn consume(&mut self, outcome: &Outcome) {
        for path in outcome {
            match path.first() {
                Some(Seg::Pull) => {
                    contribute(&mut self.pull_tree, &mut self.node_budget, &path[1..]);
                }
                _ => contribute(&mut self.tree, &mut self.node_budget, path),
            }
        }
    }

    fn slot(&self, slot: VarSlot) -> Outcome {
        self.slots.get(slot as usize).cloned().unwrap_or_default()
    }

    fn set_slot(&mut self, slot: VarSlot, outcome: Outcome) {
        if let Some(entry) = self.slots.get_mut(slot as usize) {
            *entry = outcome;
        }
    }

    /// Unions two outcomes; a fan-out beyond [`MAX_OUTCOME_PATHS`] is consumed
    /// whole instead of tracked.
    fn union(&mut self, mut left: Outcome, mut right: Outcome) -> Outcome {
        left.append(&mut right);
        if left.len() > MAX_OUTCOME_PATHS {
            self.consume(&left);
            return Vec::new();
        }
        left
    }

    /// The outcome of `id`'s outputs given the outcome of its dot.
    #[allow(
        clippy::too_many_lines,
        reason = "one arm per arena node family: the transfer table is read as a single table"
    )]
    fn walk(&mut self, id: ProgramNodeId, input: &Outcome) -> Outcome {
        if self.declined {
            return Vec::new();
        }
        let Some(remaining) = self.step_budget.checked_sub(1) else {
            self.declined = true;
            return Vec::new();
        };
        self.step_budget = remaining;
        let nodes: &'program [ProgramNode] = self.nodes;
        match &nodes[id.index()] {
            ProgramNode::Stage { start, steps } => {
                let mut paths: Outcome = match start {
                    StageStart::Current => input.clone(),
                    StageStart::Literal(_) => Vec::new(),
                    StageStart::Variable(slot) => self.slot(*slot),
                };
                // Side-effect reads (dynamic keys, slice bounds) fire even when
                // the dot is opaque — a state-object write like `.[$e.bucket]`
                // still reads the ELEMENT's member through the slot.
                for step in steps {
                    match step.access() {
                        StepAccess::Key(name) => {
                            for path in &mut paths {
                                path.push(Seg::Key(name.clone()));
                            }
                        }
                        StepAccess::Each => {
                            for path in &mut paths {
                                path.push(Seg::Element);
                            }
                        }
                        // A slice keeps its container's elements reachable and
                        // resolves its bounds against the element count, which
                        // element nodes preserve (arrays never omit elements).
                        // The slice VALUE is a new array, but its elements ARE
                        // the source container's elements: pushing an Element
                        // here and again for a later `.[]` over the bound slice
                        // would name a document position one level too deep
                        // (measured: `.users[0:36] as $x | $x[] | .metrics`
                        // pruned the metrics member to an empty object). One
                        // element level, contributed by the iteration that
                        // reads the slice, is the document position.
                        StepAccess::Slice(bounds) => {
                            for bound in [&bounds.start, &bounds.end] {
                                if let SliceBound::Var(slot) = bound {
                                    let bound_source = self.slot(*slot);
                                    self.consume(&bound_source);
                                }
                            }
                        }
                        // A dynamic member read: the key VALUE is read, and the
                        // member it names cannot be named statically — keep the
                        // container whole (v1 widening). The output is a member
                        // of a kept-whole value: covered, so opaque from here.
                        StepAccess::DynVar(slot)
                        | StepAccess::DynNodeAccessor(slot)
                        | StepAccess::DynAttribute(slot) => {
                            let key_source = self.slot(*slot);
                            self.consume(&key_source);
                            self.consume(&paths);
                            paths = Vec::new();
                        }
                        // Static indices shift under omission, and descent and
                        // the non-JSON accessors read below any nameable
                        // boundary: keep the container whole (v1 widening).
                        StepAccess::Index(_)
                        | StepAccess::Descend
                        | StepAccess::NodeAccessor(_)
                        | StepAccess::Attribute(_) => {
                            self.consume(&paths);
                            paths = Vec::new();
                        }
                    }
                    if paths.iter().any(|path| path.len() > MAX_PRUNE_DEPTH) {
                        self.consume(&paths);
                        paths = Vec::new();
                    }
                }
                // The navigated spine must EXIST in the pruned document even
                // when nothing downstream consumes the leaf payloads.
                for path in &paths {
                    match path.first() {
                        Some(Seg::Pull) => {
                            contribute_spine(&mut self.pull_tree, &mut self.node_budget, &path[1..]);
                        }
                        _ => contribute_spine(&mut self.tree, &mut self.node_budget, path),
                    }
                }
                paths
            }
            ProgramNode::FlatMap { upstream, body } => {
                let (upstream, body) = (*upstream, *body);
                let upstream = self.walk(upstream, input);
                self.walk(body, &upstream)
            }
            ProgramNode::Choice { left, right } => {
                let (left, right) = (*left, *right);
                let left = self.walk(left, input);
                let right = self.walk(right, input);
                self.union(left, right)
            }
            // A construction barrier PUBLISHES what its body produced: any
            // document payload the body's outputs alias is observable through
            // the product, so it is consumed here. (`[…] | length` could keep
            // a skeleton instead; v1 deliberately has no shell tier.)
            ProgramNode::CollectArray { body } | ProgramNode::CountCollect { body } => {
                if let Some(body) = *body {
                    let published = self.walk(body, input);
                    self.consume(&published);
                }
                Vec::new()
            }
            ProgramNode::ConstructObject { members } => {
                let members: &'program [crate::program::ObjectMemberNode] = members;
                for member in members {
                    let key = self.walk(member.key, input);
                    self.consume(&key);
                    let value = self.walk(member.value, input);
                    self.consume(&value);
                }
                Vec::new()
            }
            ProgramNode::Call { overload, args, .. } => {
                let overload = overload.get();
                if self.input_pull.contains(&Some(overload)) {
                    // The pulled document is not the root. Record a Pull
                    // origin so `$r.v` after `inputs as $r` lands on the
                    // pulled-record tree; the root tree still declines.
                    self.saw_input_pull = true;
                    return alloc::vec![alloc::vec![Seg::Pull]];
                }
                match registry::demand_transfer(overload) {
                    // `select(cond)` passes its input through unchanged.
                    Some(DemandTransfer::ConditionUnionPassThrough) => {
                        for arg in args {
                            let observed = self.walk(*arg, input);
                            self.consume(&observed);
                        }
                        input.clone()
                    }
                    // Everything else observes its input's payload under this
                    // lattice: `not`'s truthiness distinguishes a present
                    // member from an omitted one, `length`/`keys`/`type` on a
                    // document value need at least its skeleton and v1 has no
                    // skeleton tier, and the conservative declarations capture
                    // their input outright.
                    Some(
                        DemandTransfer::InputPassThrough
                        | DemandTransfer::CountOfConstructedInput
                        | DemandTransfer::Subtree
                        | DemandTransfer::ViaLowering,
                    )
                    | None => {
                        self.consume(input);
                        for arg in args {
                            let observed = self.walk(*arg, input);
                            self.consume(&observed);
                        }
                        Vec::new()
                    }
                }
            }
            // Only RECURSIVE defs survive lowering as `CallDef`. The walk
            // cannot bound which paths a recursive body touches per level, so
            // the call input and every argument are kept whole; the body walks
            // ONCE (per recursion chain) for its own document reads through
            // enclosing binds.
            ProgramNode::CallDef {
                body,
                param_slots,
                args,
                filter_args,
                ..
            } => {
                let body = *body;
                self.consume(input);
                for (position, arg) in args.iter().enumerate() {
                    let observed = self.walk(*arg, input);
                    self.consume(&observed);
                    if let Some(slot) = param_slots.get(position) {
                        self.set_slot(*slot, Vec::new());
                    }
                }
                for arg in filter_args {
                    let observed = self.walk(*arg, input);
                    self.consume(&observed);
                }
                if !self.visiting.contains(&body) {
                    self.visiting.push(body);
                    let published = self.walk(body, &Vec::new());
                    self.consume(&published);
                    self.visiting.pop();
                }
                Vec::new()
            }
            ProgramNode::CallFilter { .. } => {
                self.consume(input);
                Vec::new()
            }
            // Binary arithmetic reads both operand payloads; Logical
            // truthiness distinguishes a present member from an omitted one.
            // Same transfer either way: both outcomes are read whole.
            ProgramNode::Binary { left, right, .. } | ProgramNode::Logical { left, right, .. } => {
                let (left, right) = (*left, *right);
                let left = self.walk(left, input);
                self.consume(&left);
                let right = self.walk(right, input);
                self.consume(&right);
                Vec::new()
            }
            ProgramNode::Concat { parts } => {
                for part in parts {
                    let walked = self.walk(*part, input);
                    self.consume(&walked);
                }
                Vec::new()
            }
            ProgramNode::Conditional {
                condition,
                consequent,
                alternative,
            } => {
                let (condition, consequent, alternative) = (*condition, *consequent, *alternative);
                let condition = self.walk(condition, input);
                self.consume(&condition);
                let consequent = self.walk(consequent, input);
                let alternative = self.walk(alternative, input);
                self.union(consequent, alternative)
            }
            // `l // r` observes l's truthiness (kept whole) and publishes
            // either side.
            ProgramNode::Alternative { left, right } => {
                let (left, right) = (*left, *right);
                let left = self.walk(left, input);
                self.consume(&left);
                let right = self.walk(right, input);
                self.union(left, right)
            }
            // A HANDLED try is an error-value capture: the rendered message
            // can expose the offending operand (`projection.rs` precedent), so
            // the input and the body's outputs are kept whole. A catchless try
            // only swallows, and keeps the walk's precision.
            ProgramNode::Try { body, handler } => {
                let (body, handler) = (*body, *handler);
                let published = self.walk(body, input);
                match handler {
                    None => published,
                    Some(handler) => {
                        self.consume(input);
                        self.consume(&published);
                        self.walk(handler, &Vec::new())
                    }
                }
            }
            ProgramNode::ChainBody { body } | ProgramNode::Label { body, .. } => {
                let body = *body;
                self.walk(body, input)
            }
            ProgramNode::Empty | ProgramNode::Break { .. } => Vec::new(),
            // `E as $x | F` runs F with the SAME dot.
            ProgramNode::Bind { source, slot, body, .. } => {
                let (source, slot, body) = (*source, *slot, *body);
                let bound = self.walk(source, input);
                self.set_slot(slot, bound);
                self.walk(body, input)
            }
            // The `~cursor` engine-binding vocabulary pulls from the input
            // stream; see the `input`/`inputs` decline above.
            ProgramNode::EngineBind { .. }
            | ProgramNode::EnginePull { .. }
            | ProgramNode::EngineGenerator { .. }
            | ProgramNode::EngineRng { .. } => {
                self.declined = true;
                Vec::new()
            }
            ProgramNode::Reduce {
                source,
                slot,
                init,
                update,
                keyed_collect: _,
            } => {
                let (source, slot, init, update) = (*source, *slot, *init, *update);
                let bound = self.walk(source, input);
                self.set_slot(slot, bound);
                let init = self.walk(init, input);
                self.consume(&init);
                // The state is a constructed value; anything document-aliasing
                // stored INTO it was consumed when the storing expression's
                // outcome was, below.
                let update = self.walk(update, &Vec::new());
                self.consume(&update);
                Vec::new()
            }
            ProgramNode::Foreach {
                source,
                slot,
                init,
                update,
                extract,
            } => {
                let (source, slot, init, update, extract) = (*source, *slot, *init, *update, *extract);
                let bound = self.walk(source, input);
                self.set_slot(slot, bound);
                let init = self.walk(init, input);
                self.consume(&init);
                let update = self.walk(update, &Vec::new());
                self.consume(&update);
                if let Some(extract) = extract {
                    let extract = self.walk(extract, &Vec::new());
                    self.consume(&extract);
                }
                Vec::new()
            }
            // `limit`/`skip`/`nth` pass a subset of the source's values
            // through unchanged.
            ProgramNode::Counted { source, .. } => {
                let source = *source;
                self.walk(source, input)
            }
            // An assignment over a DOCUMENT value publishes the whole
            // rewritten document; over a constructed state it reads only what
            // its path and update expressions read.
            // An assignment walks the same two graphs over the same dot (the
            // edit lane never prunes a fact assignment, but the walk must stay
            // total).
            ProgramNode::Modify { paths, update, .. } | ProgramNode::FactAssign { paths, update, .. } => {
                let (paths, update) = (*paths, *update);
                if !input.is_empty() {
                    self.consume(input);
                }
                let addressed = self.walk(paths, input);
                self.consume(&addressed);
                let update = self.walk(update, &Vec::new());
                self.consume(&update);
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PruneNode;
    use crate::codec_requirement::CodecRequirementPolicy;
    use crate::compile::{CompileOptions, try_compile_program};
    use alloc::string::String;
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

    /// Renders a tree as a compact canonical string for assertions:
    /// `*` = whole subtree, `{k:…}` named members, `[]…` the element node.
    fn render(node: &PruneNode) -> String {
        if node.is_all() {
            return String::from("*");
        }
        let mut out = String::from("{");
        let mut first = true;
        if let Some(element) = node.element() {
            out.push_str("[]:");
            out.push_str(&render(element));
            first = false;
        }
        for (name, child) in node.keys() {
            if !first {
                out.push(',');
            }
            first = false;
            out.push_str(name);
            out.push(':');
            out.push_str(&render(child));
        }
        out.push('}');
        out
    }

    fn tree_of(source: &str) -> Option<String> {
        let resources = resources();
        let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
        let program =
            try_compile_program(source, policy, CompileOptions::new(), &resources).expect("probe program compiles");
        program.prune_tree().map(render)
    }

    #[test]
    fn identity_declines() {
        assert_eq!(tree_of("."), None);
    }

    #[test]
    fn union_of_traversals_names_the_fields() {
        // Taxonomy group A (transpose_user_columns): three separate collects
        // union into one per-element field set.
        assert_eq!(
            tree_of("[.users[].id], [.users[].name], [.users[].age]").as_deref(),
            Some("{users:{[]:{age:*,id:*,name:*}}}")
        );
    }

    #[test]
    fn slice_bound_iteration_keeps_the_element_payload_whole() {
        // `.users[0:36] as $x | $x[] | .metrics`: the slice's elements ARE
        // the source array's elements, so the tree names ONE element level
        // under `users`. A second level (from treating the slice as a fresh
        // container) pruned the metrics member to `{}` in the decoder — the
        // empty-object defect this test pins.
        assert_eq!(
            tree_of(".users[0:36] as $x | $x[] | .metrics").as_deref(),
            Some("{users:{[]:{metrics:*}}}")
        );
    }

    #[test]
    fn bound_collected_array_keeps_element_precision() {
        // Taxonomy group C (stats_variance_scores): demand on the OWNED
        // collected array must not widen the document element.
        assert_eq!(
            tree_of("[.users[].score] as $a | ($a | add)").as_deref(),
            Some("{users:{[]:{score:*}}}")
        );
    }

    #[test]
    fn fold_state_reads_stay_on_the_slot_paths() {
        // Taxonomy group B (hc_reduce_histogram shape): the dynamic member
        // WRITE is on the constructed state; the dynamic KEY read is the
        // element's member.
        assert_eq!(
            tree_of("reduce .events[] as $e ({}; .[$e.bucket] += 1)").as_deref(),
            Some("{events:{[]:{bucket:*}}}")
        );
    }

    #[test]
    fn foreach_extract_contributes_like_publication() {
        assert_eq!(
            tree_of("[foreach .users[] as $u (0; . + 1; {n: ., id: $u.id})] | length").as_deref(),
            Some("{users:{[]:{id:*}}}")
        );
    }

    #[test]
    fn select_passthrough_publishes_the_element() {
        // The collect publishes the SELECTED elements whole; only the sibling
        // top-level members prune away (v1: no length back-pressure). The
        // second traversal defeats the static-prefix pushdown, which would
        // otherwise take the forward route where no prune applies.
        assert_eq!(
            tree_of("([.orders[] | select(.status == \"completed\")] | length), (.users[].id)").as_deref(),
            Some("{orders:{[]:*},users:{[]:{id:*}}}")
        );
    }

    #[test]
    fn published_root_and_document_assignment_decline() {
        assert_eq!(tree_of(".users, ."), None);
        assert_eq!(tree_of("(.users[].name) |= ascii_upcase"), None);
    }

    #[test]
    fn conservative_builtins_keep_their_input_whole() {
        assert_eq!(
            tree_of("(.users | tojson | length), (.events[].type)").as_deref(),
            Some("{events:{[]:{type:*}},users:*}")
        );
    }

    #[test]
    fn a_pushdown_prefix_program_derives_the_tree() {
        // A static-prefix program still has a root-anchored tree; the
        // forward lowering re-anchors it onto the located node (a keep-all
        // remainder is a no-op and is not attached).
        assert_eq!(tree_of(".users | tojson | length").as_deref(), Some("{users:*}"));
    }

    #[test]
    fn static_index_widens_its_container() {
        // v1: omitting array elements would shift indices, so `products` stays
        // whole; the iterated-but-unread `orders` elements keep only their
        // SPINE (count preserved, members pruned).
        assert_eq!(
            tree_of(". as $d | [.orders[] | $d.products[3].id] | add").as_deref(),
            Some("{orders:{[]:{}},products:*}")
        );
    }

    #[test]
    fn iterated_but_unread_elements_keep_their_spine() {
        // The element COUNT is observable (one output per element), so the
        // spine survives even though no payload does.
        assert_eq!(
            tree_of("([.orders[] | 1] | length), (.users[].id)").as_deref(),
            Some("{orders:{[]:{}},users:{[]:{id:*}}}")
        );
    }

    #[test]
    fn shared_element_demand_absorbs_into_named_members() {
        // `name` is reachable BOTH by name and through `.[]`: the finalize
        // pass folds the element demand into the named child so decode-time
        // lookup is hit-or-element with no join.
        assert_eq!(
            tree_of("(.users.name), (.users[].id)").as_deref(),
            Some("{users:{[]:{id:*},name:*}}")
        );
        assert_eq!(
            tree_of("(.users.name.first), (.users[].id)").as_deref(),
            Some("{users:{[]:{id:*},name:{first:*,id:*}}}")
        );
    }

    #[test]
    fn input_pulls_decline() {
        assert_eq!(tree_of(".users, input"), None);
        assert_eq!(tree_of("[inputs] | length"), None);
    }

    fn pulled_of(source: &str) -> Option<String> {
        let resources = resources();
        let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
        let program = try_compile_program(source, policy, CompileOptions::new(), &resources).expect("compiles");
        program.pulled_record_prune().map(render)
    }

    #[test]
    fn pulled_record_tree_names_slot_fields() {
        assert_eq!(pulled_of("reduce inputs as $r (0; . + $r.v)").as_deref(), Some("{v:*}"));
        assert_eq!(pulled_of("input.v").as_deref(), Some("{v:*}"));
        // The construction publishes the pulled document whole.
        assert_eq!(pulled_of("[inputs] | length"), None);
        // No pull: no pulled-record tree.
        assert_eq!(pulled_of(".v"), None);
    }
}
