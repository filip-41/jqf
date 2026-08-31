//! The backward projection-demand lattice.
//!
//! [`classify`] answers one question about a lowered, fused arena: *how much of
//! each streamed document element does this program actually consume?* The
//! answer is one [`ProjectionClass`] on the lattice `Structure < Fields(S) <
//! Subtree`, propagated BACKWARD **from publication**, never forward from
//! consumption sites.
//!
//! The class does not itself choose a route. Its live consumers are the
//! count-demand derivation (a `Structure` class on the counted boundary lets a
//! count answer from structure alone) and the routing-facts plan, which
//! serializes the class as a receipt the smoke batteries assert on: a wrong
//! answer today is a failing receipt, never wrong bytes.
//!
//! # The lattice
//!
//! - [`ProjectionClass::Structure`] (P0) — boundaries and counts only; no
//!   payload of the element is needed.
//! - [`ProjectionClass::Fields`] (P1) — the named top-level fields' payloads are
//!   needed, nothing else. The set is an over-approximation: a deeper path
//!   `.a.b` names `a` (the whole `a` subtree), and an array index or a dynamic
//!   `.[$x]` cannot be named at all, so both fall to `Subtree`.
//! - [`ProjectionClass::Subtree`] (P2) — today's behavior, and the conservative
//!   default for everything the v1 transfer table does not pin.
//!
//! # Where the class is recorded
//!
//! The class describes ONE STREAMED ELEMENT, so it is recorded at the `.[]`
//! ([`StepAccess::Each`]) steps: whatever demand the walk has computed for the
//! value a `.[]` produces IS the per-element demand.
//!
//! A named boundary: [`PushdownSplit`](super::PushdownSplit) NAMES the single
//! `.[]` that is THE element boundary — including one inside a `reduce`/`foreach`
//! source and one inside `map`'s lowered `[.[] | f]` body. When the program has a
//! SOLE `.[]`, the class is recorded at exactly that boundary and nowhere else.
//! Nested iteration (two or more `.[]`) keeps the conservative join over every
//! boundary, because separating the outer element's demand from the inner one's
//! needs a per-boundary walk the naming does not build. A program with no `.[]`
//! at all has no element stream to project, so it classifies `Subtree`.
//!
//! # The transfer table, and where each half of it lives
//!
//! The `Call` half of the table is REGISTRY LAW: every builtin
//! overload record carries a required
//! [`jqf_builtins::registry::DemandTransfer`] declaration, and
//! `Classifier::call_demand` interprets that closed combinator vocabulary
//! instead of matching overload ids. A registered overload without a declaration
//! is not expressible (the field is required), so the classifier is TOTAL over
//! the call vocabulary by construction.
//!
//! That totality law is CALL-SCOPED, deliberately. The other half of the table
//! lives on program NODES that carry no overload id to hang a record on, and
//! stays guarded by the exhaustive, wildcard-free match in [`Classifier::demand`]
//! — adding a `ProgramNode` variant is a compile error until its arm is written.
//!
//! | construct | transfer | home |
//! | --- | --- | --- |
//! | `length` on a CONSTRUCTED input (`[…]`/`{…}`) | `Structure` | registry (`CountOfConstructedInput`) |
//! | `length` on a document value | `Subtree` (codepoint/magnitude counts need payloads) | registry (same tag, document arm) |
//! | `select(cond)` | condition demand ∪ pass-through demand | registry (`ConditionUnionPassThrough`) |
//! | `not` | the input demand IS the output demand | registry (`InputPassThrough`) |
//! | `error/0`, `error/1` | `Subtree` | registry (`Subtree`) |
//! | `type`, `keys` | `Subtree` — an element's kind and keys ARE its payload here (see [`Classifier::call_demand`]) | registry (`Subtree`) |
//! | `map(f)` | none needed — it expanded to `[.[] \| f]` before the arena | registry (`ViaLowering`) |
//! | `+`/`-`/`…`/comparisons | `Subtree` on both operand graphs (a fold over a projected field reaches the field through its operand) | node arm (`Binary`/`Logical`) |
//! | `if`/`//`/`try`/constructors/`reduce`/`foreach` state | see each arm | node arms |
//! | `.k` step | `Structure` when the field's own demand is `Structure`, else `Fields({k})` | step arm |
//!
//! # The output rule
//!
//! A value reaching publication, an unprojected construction barrier,
//! `error/0`'s input capture, `error/1`'s payload, a `catch` handler's error
//! value, or any builtin declaring the conservative `Subtree` transfer is
//! `Subtree`.

use alloc::vec::Vec;

use crate::program::{ObjectMemberNode, ProgramNode, ProgramNodeId, StageStart, StageStep, StepAccess, VarSlot};
use jqf_builtins::registry::DemandTransfer;
use jqf_data::Value;

/// The most top-level field names one [`ProjectionClass::Fields`] can name.
pub const INLINE_FIELDS: usize = 8;

/// The most top-level field names a classifier field set can name. The set is
/// a FROZEN bound: a demand wider than it joins up to `Subtree` before a
/// requirement is lowered.
pub const MAX_PROJECTED_FIELDS: usize = 64;

/// The classifier sizes its slot table to the program's own binder-occurrence
#[derive(Clone, Debug)]
pub struct ProjectedFields<'program> {
    names: FieldNames<'program>,
}

#[derive(Clone, Debug)]
enum FieldNames<'program> {
    /// The common case: up to [`INLINE_FIELDS`] names, no allocation.
    Inline {
        names: [&'program str; INLINE_FIELDS],
        len: usize,
    },
    /// A demand wider than [`INLINE_FIELDS`]: a heap set, still sorted and
    /// deduplicated, still bounded by [`MAX_PROJECTED_FIELDS`].
    Wide(Vec<&'program str>),
}

impl<'program> ProjectedFields<'program> {
    /// The projected field names, sorted and deduplicated.
    #[must_use]
    pub fn names(&self) -> &[&'program str] {
        match &self.names {
            FieldNames::Inline { names, len } => &names[..*len],
            FieldNames::Wide(names) => names,
        }
    }

    /// The one-field set, the only way a `StepAccess::Key` step seeds a
    /// projection.
    #[must_use]
    pub fn singleton(name: &'program str) -> Self {
        let mut names = [""; INLINE_FIELDS];
        names[0] = name;
        Self {
            names: FieldNames::Inline { names, len: 1 },
        }
    }

    /// Inserts `name` in sorted position, reporting `false` when the set is
    /// already at `MAX_PROJECTED_FIELDS` (the caller then joins up to
    /// `Subtree`).
    pub fn insert(&mut self, name: &'program str) -> bool {
        match &mut self.names {
            FieldNames::Inline { names, len } => {
                // `binary_search`'s `Err(at)` IS the insertion point — the
                // sorted-and-deduplicated contract this set documents.
                match names[..*len].binary_search(&name) {
                    Ok(_) => true,
                    Err(at) => {
                        if *len < INLINE_FIELDS {
                            names.copy_within(at..*len, at + 1);
                            names[at] = name;
                            *len += 1;
                            return true;
                        }
                        // The inline capacity is exhausted: overflow into the
                        // heap set, preserving sorted order. Classification
                        // runs once per compiled program, so this allocation
                        // is not on any hot path.
                        let mut wide = Vec::with_capacity(INLINE_FIELDS + 1);
                        wide.extend_from_slice(&names[..*len]);
                        wide.insert(at, name);
                        self.names = FieldNames::Wide(wide);
                        true
                    }
                }
            }
            FieldNames::Wide(names) => match names.binary_search(&name) {
                Ok(_) => true,
                Err(at) => {
                    if names.len() >= MAX_PROJECTED_FIELDS {
                        return false;
                    }
                    names.insert(at, name);
                    true
                }
            },
        }
    }
}

impl PartialEq for ProjectedFields<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.names() == other.names()
    }
}

impl Eq for ProjectedFields<'_> {}

/// How much of one streamed document element a compiled program consumes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProjectionClass<'program> {
    /// P0 — boundaries and counts only; no element payload is decoded.
    Structure,
    /// P1 — only the named top-level fields' payloads are needed.
    //
    // P1 eligibility rests on the UNCAUGHT message's spelling never rendering
    // a projected element's PAYLOAD. The engine renders operand values in type
    // errors, and the re-gate holds by the whole-copy argument: every
    // non-object projected element is copied WHOLE from validated source
    // spans, so an offending element's payload is always present on the error
    // path and only its type and path step render — the same words the floor
    // would render; an object renders kind-only. Revisit this site when the
    // operand rendering changes.
    Fields(ProjectedFields<'program>),
    /// P2 — the whole element subtree, today's behavior and the conservative
    /// default for every construct the v1 transfer table does not pin.
    Subtree,
}

impl ProjectionClass<'_> {
    /// The lattice join (least upper bound): `Structure` is the bottom, two
    /// field sets union (overflowing the inline capacity to `Subtree`), and
    /// `Subtree` absorbs everything.
    #[must_use]
    pub fn join(self, other: Self) -> Self {
        match (self, other) {
            (Self::Subtree, _) | (_, Self::Subtree) => Self::Subtree,
            (Self::Structure, keep) | (keep, Self::Structure) => keep,
            (Self::Fields(mut left), Self::Fields(right)) => {
                for name in right.names() {
                    if !left.insert(name) {
                        return Self::Subtree;
                    }
                }
                Self::Fields(left)
            }
        }
    }

    /// Whether this demand needs no payload at all — the test every payload-free
    /// transfer (a `.k` step, a `.[i]` step) keys off.
    const fn is_structure_only(&self) -> bool {
        matches!(self, Self::Structure)
    }
}

/// Whether a node's outputs are CONSTRUCTED values (`[…]`/`{…}` products)
fn produces_constructed(nodes: &[ProgramNode], id: ProgramNodeId) -> bool {
    match &nodes[id.index()] {
        ProgramNode::CollectArray { .. }
        | ProgramNode::CountCollect { .. }
        | ProgramNode::ConstructObject { .. } => true,
        // A `CountCollect` is reported constructed although its output is the
        // COUNT, a NUMBER whose magnitude a `length` reads as payload — the
        // Slice clause's twin. It is inert for the same reason: a count-collect
        // body is Each-free by the lowering gate, so no `.[]` exists downstream
        // of the count for the transfer to reach; the demand a `length`-over-
        // count would mis-transfer lands in a body with nothing to classify.
        ProgramNode::FlatMap { body, .. } => produces_constructed(nodes, *body),
        ProgramNode::Choice { left, right } => {
            produces_constructed(nodes, *left) && produces_constructed(nodes, *right)
        }
        // A stage ending in `.[a:b]` is deliberately NOT reported as
        // constructed, although a slice does materialize a new array, so that
        // `CountOfConstructedInput` fires on `PATH[a:b] | length`. The clause
        // is INERT and would be latently unsound:
        //
        // - inert, because the only consumer of this answer is the demand that
        //   travels backward to a `.[]`, and the born-P2 `StepAccess::Slice` arm
        //   in `steps_demand` joins any demand back up to `Subtree` before it
        //   can reach one. No program's class moves;
        // - latently unsound, because a slice of a STRING produces a string,
        //   whose `length` is a codepoint count over PAYLOAD. The clause would
        //   become observable, and wrong, the moment the born-P2 arm was
        //   relaxed.
        //
        // The `PATH[a:b] | length` row carries its own soundness argument
        // instead (`analysis::count::is_range_count`): the codec resolves the
        // range against an ARRAY and every other container declines the rung.
        ProgramNode::Stage { .. }
        | ProgramNode::Call { .. }
        | ProgramNode::CallDef { .. }
        | ProgramNode::CallFilter { .. }
        | ProgramNode::Binary { .. }
        | ProgramNode::Concat { .. }
        | ProgramNode::Conditional { .. }
        | ProgramNode::Alternative { .. }
        | ProgramNode::Logical { .. }
        | ProgramNode::Try { .. }
        | ProgramNode::Label { .. }
        | ProgramNode::Break { .. }
        // A shared `?//` chain body's outputs are not provably constructed
        // without revisiting it per arm; report the conservative answer.
        | ProgramNode::ChainBody { .. }
        // An assignment's output is a rewritten copy of its INPUT, so it carries
        // document payloads: `"ab" | . += "c" | length` is a codepoint count.
        // Reporting it constructed would hand `length` the payload-free transfer
        // and answer that row wrong.
        | ProgramNode::Modify { .. }
        | ProgramNode::FactAssign { .. }
        | ProgramNode::Empty
        | ProgramNode::Bind { .. }
        | ProgramNode::Reduce { .. }
        | ProgramNode::Foreach { .. }
        // A counted stream re-publishes its SOURCE's values unchanged, so it
        // carries whatever payloads they carried. Reporting it constructed would
        // hand `length` the payload-free transfer for `limit(1; .a[]) | length`
        // and answer that row wrong.
        | ProgramNode::Counted { .. }
        // Engine pulls and binds are state-carrying side effects whose outputs
        // are runtime-defined — never provably constructed.
        | ProgramNode::EngineBind { .. }
        | ProgramNode::EnginePull { .. }
        | ProgramNode::EngineGenerator { .. }
        | ProgramNode::EngineRng { .. } => false,
    }
}

/// Classifies a fused arena's per-element projection demand.
pub fn classify(
    nodes: &[ProgramNode],
    root: ProgramNodeId,
    slots: u32,
    boundary: Option<(ProgramNodeId, usize)>,
) -> ProjectionClass<'_> {
    // The slot table is sized to the program's OWN binder-occurrence count:
    // the compiler mints exactly one slot per binder occurrence, so this is a
    // small vector proportional to the program, never a fixed cap that a
    // binder-heavy program overflows into Subtree.
    let mut classifier = Classifier {
        nodes,
        slots: alloc::vec![ProjectionClass::Structure; slots as usize],
        element: None,
        boundary,
    };
    // The output rule's root case: everything the program emits is PUBLISHED,
    // so the root's outputs carry full `Subtree` demand.
    classifier.demand(root, ProjectionClass::Subtree, InputOrigin::Document);
    classifier.element.unwrap_or(ProjectionClass::Subtree)
}

/// What kind of value feeds a node's dot — the one context the `length`
#[derive(Clone, Copy, Eq, PartialEq)]
enum InputOrigin {
    /// The dot is a document value (or derived from one by navigation).
    Document,
    /// The dot is a constructed `[…]`/`{…}` product.
    Constructed,
}

/// The backward walk's state: the arena, the per-binder-slot demands the walk
struct Classifier<'program> {
    nodes: &'program [ProgramNode],
    slots: Vec<ProjectionClass<'program>>,
    element: Option<ProjectionClass<'program>>,
    /// The NAMED element boundary in (stage node, step offset) coordinates,
    /// present only when the boundary walk named a SOLE one. `None` restores
    /// recording at every `.[]` and join — which is what nested iteration
    /// keeps.
    boundary: Option<(ProgramNodeId, usize)>,
}

impl<'program> Classifier<'program> {
    /// Records the demand on one `.[]` step's element values.
    ///
    /// With a named boundary this records ONLY at that boundary — the exact
    /// per-boundary classification a named boundary buys. Without one it joins
    /// every `.[]` in the program — the conservative behavior nested iteration
    /// keeps.
    fn record_element(&mut self, stage: ProgramNodeId, offset: usize, demand: ProjectionClass<'program>) {
        if let Some((boundary_stage, boundary_offset)) = self.boundary
            && (boundary_stage != stage || boundary_offset != offset)
        {
            return;
        }
        self.element = Some(match self.element.take() {
            Some(seen) => seen.join(demand),
            None => demand,
        });
    }

    /// The demand accumulated on one binder occurrence's bound value.
    fn slot(&self, slot: VarSlot) -> ProjectionClass<'program> {
        self.slots
            .get(slot as usize)
            .cloned()
            // Unreachable: the table is sized to the program's own slot count.
            // Conservative rather than panicking.
            .unwrap_or(ProjectionClass::Subtree)
    }

    /// Joins `demand` into one binder occurrence's bound-value demand.
    fn join_slot(&mut self, slot: VarSlot, demand: ProjectionClass<'program>) {
        if let Some(existing) = self.slots.get_mut(slot as usize) {
            *existing = existing.clone().join(demand);
        }
    }

    /// The demand on `id`'s INPUT value, given the demand `out` on its outputs
    /// and what kind of value feeds its dot.
    #[allow(
        clippy::too_many_lines,
        reason = "one arm per arena node family: the transfer table is read as a single table"
    )]
    fn demand(
        &mut self,
        id: ProgramNodeId,
        out: ProjectionClass<'program>,
        input: InputOrigin,
    ) -> ProjectionClass<'program> {
        // The arena outlives the walk (`'program`), so reading the node through a
        // copy of the slice reference — not through `self` — leaves `self`
        // free for the `&mut` recursion inside every arm.
        let nodes: &'program [ProgramNode] = self.nodes;
        match &nodes[id.index()] {
            // A current-start stage walks its steps over the input; a literal- or
            // variable-start stage IGNORES the input (so its input demand is the
            // lattice bottom), but its steps still walk — over the literal, or
            // over the bound value whose demand they contribute to the slot.
            ProgramNode::Stage { start, steps } => {
                let entering = self.steps_demand(id, steps, out);
                match start {
                    StageStart::Current => entering,
                    StageStart::Literal(_) => ProjectionClass::Structure,
                    StageStart::Variable(slot) => {
                        self.join_slot(*slot, entering);
                        ProjectionClass::Structure
                    }
                }
            }
            // Pipe: the body's input demand becomes the upstream's output demand.
            // The upstream also decides whether the body's dot is a constructed
            // value — the one context the `length` transfer reads.
            ProgramNode::FlatMap { upstream, body } => {
                let (upstream, body) = (*upstream, *body);
                let origin = if produces_constructed(self.nodes, upstream) {
                    InputOrigin::Constructed
                } else {
                    InputOrigin::Document
                };
                let middle = self.demand(body, out, origin);
                self.demand(upstream, middle, input)
            }
            // Both comma members read the SAME input under the same output demand.
            ProgramNode::Choice { left, right } => {
                let (left, right) = (*left, *right);
                let left = self.demand(left, out.clone(), input);
                let right = self.demand(right, out, input);
                left.join(right)
            }
            // A construction barrier is transparent to a payload-free demand
            // (`[…] | length` counts what the body produced) and opaque to every
            // other demand: an UNPROJECTED barrier publishes its members, so the
            // body carries full `Subtree` demand (the output rule).
            ProgramNode::CollectArray { body } | ProgramNode::CountCollect { body } => {
                let body = *body;
                let inner = if out.is_structure_only() {
                    ProjectionClass::Structure
                } else {
                    ProjectionClass::Subtree
                };
                match body {
                    Some(body) => self.demand(body, inner, input),
                    None => ProjectionClass::Structure,
                }
            }
            // Object construction is NOT in the v1 transfer table, and it cannot
            // borrow `CollectArray`'s unconditionally: duplicate keys collapse,
            // so the KEY producers decide the member count.
            //
            // The ALL-STATIC-KEY exception: when every key is a steps-free
            // string literal, duplicates are compile-time known and the member
            // count is fixed — the count of a constructed object is exactly the
            // element count (the count table's construct row). The member
            // VALUES may therefore carry the outgoing demand instead of
            // `Subtree`. A dynamic key producer can yield zero or many keys per
            // element, which changes the member count, so any dynamic or mixed
            // key set keeps the `Subtree` fallback (recorded conservatism, not
            // an oversight).
            ProgramNode::ConstructObject { members } => {
                let members: &'program [ObjectMemberNode] = members;
                let static_keys = members
                    .iter()
                    .all(|member| is_static_literal_key(nodes, member.key));
                let mut demand = ProjectionClass::Structure;
                for member in members {
                    let key = self.demand(member.key, ProjectionClass::Subtree, input);
                    let value = if static_keys {
                        self.demand(member.value, out.clone(), input)
                    } else {
                        self.demand(member.value, ProjectionClass::Subtree, input)
                    };
                    demand = demand.join(key).join(value);
                }
                demand
            }
            ProgramNode::Call { overload, args, .. } => {
                let overload = overload.get();
                let args: &'program [ProgramNodeId] = args;
                self.call_demand(overload, args, out, input)
            }
            // A recursive call reads the whole input through its argument
            // graphs (and the body reads its own input), so the demand is
            // `Subtree` — the same class every call contributes.
            ProgramNode::CallDef {
                args, filter_args, ..
            } => {
                let args: &'program [ProgramNodeId] = args;
                let filter_args: &'program [ProgramNodeId] = filter_args;
                for arg in args {
                    let _ = self.demand(*arg, ProjectionClass::Subtree, InputOrigin::Document);
                }
                for arg in filter_args {
                    let _ = self.demand(*arg, ProjectionClass::Subtree, InputOrigin::Document);
                }
                ProjectionClass::Subtree
            }
            // A shared `?//` chain body reads arbitrary per-arm work; the
            // conservative whole-element demand is returned without revisiting
            // the body once per arm.
            ProgramNode::ChainBody { .. }
            // A pull's output is cursor state, never a document projection, so
            // its INPUT demand is the conservative whole element — the most
            // demanding class, not the lattice bottom.
            | ProgramNode::EnginePull { .. }
            | ProgramNode::EngineRng { .. }
            // A filter call materializes its input's payload through the
            // predicate, so its input demand is the conservative whole
            // element too.
            | ProgramNode::CallFilter { .. } => ProjectionClass::Subtree,
            // A condition is consumed for truthiness (not pinned in the v1 table,
            // so `Subtree`); both arms re-read the ORIGINAL input under the
            // conditional's own output demand.
            ProgramNode::Conditional {
                condition,
                consequent,
                alternative,
            } => {
                let (condition, consequent, alternative) = (*condition, *consequent, *alternative);
                let condition = self.demand(condition, ProjectionClass::Subtree, input);
                let consequent = self.demand(consequent, out.clone(), input);
                let alternative = self.demand(alternative, out, input);
                condition.join(consequent).join(alternative)
            }
            // `//` filters the left by truthiness AND re-emits it, so the left
            // carries both demands (`Subtree`); the right passes through
            // unfiltered under the alternative's own output demand.
            ProgramNode::Alternative { left, right } => {
                let (left, right) = (*left, *right);
                let left = self.demand(left, ProjectionClass::Subtree, input);
                let right = self.demand(right, out, input);
                left.join(right)
            }
            // Every arithmetic and comparison operand is materialized at the op
            // barrier, and `and`/`or` consume both operands for truthiness — so
            // both families demand full payloads from both operand graphs. This
            // is the `+`-fold-on-a-projected-field transfer: the fold reaches the
            // field THROUGH its operand graph, which then names it.
            ProgramNode::Binary { left, right, .. } | ProgramNode::Logical { left, right, .. } => {
                let (left, right) = (*left, *right);
                let left = self.demand(left, ProjectionClass::Subtree, input);
                let right = self.demand(right, ProjectionClass::Subtree, input);
                left.join(right)
            }
            // String interpolation materializes every part (holes already ran
            // `tostring`); same payload demand as a `+` fold.
            ProgramNode::Concat { parts } => {
                let mut demand = ProjectionClass::Structure;
                for part in parts {
                    demand = demand.join(self.demand(*part, ProjectionClass::Subtree, input));
                }
                demand
            }
            // A catchless `try` only SWALLOWS errors, so it adds no demand. A
            // `catch` handler is an error-value CAPTURE: raised messages render
            // the offending operand (`5 | try .[] catch .` →
            // `"Cannot iterate over number (5)"`), so a handled `try` falls to
            // `Subtree` under the output rule. The handler graph is still walked
            // (its `.[]` boundaries and `$x` reads are real) with its dot standing
            // for the error value, never the try's input.
            ProgramNode::Try { body, handler } => {
                let (body, handler) = (*body, *handler);
                let body = self.demand(body, out.clone(), input);
                match handler {
                    None => body,
                    Some(handler) => {
                        self.demand(handler, out, InputOrigin::Document);
                        body.join(ProjectionClass::Subtree)
                    }
                }
            }
            // A `label` is demand-TRANSPARENT: it evaluates its body over its own
            // input and re-emits the body's outputs unchanged, so the body's
            // demand IS the label's. The barrier changes cardinality, never which
            // bytes are read.
            ProgramNode::Label { body, .. } => {
                let body = *body;
                self.demand(body, out, input)
            }
            // A `break` reads nothing and emits nothing, and `empty` reads
            // nothing at all.
            ProgramNode::Break { .. } | ProgramNode::Empty => ProjectionClass::Structure,
            // An assignment is `Subtree`, ALWAYS. Its output CONTAINS its whole
            // input — every byte the fold did not rewrite is republished — so
            // there is no demand to narrow, whatever the two child graphs read.
            // Both are still walked so their slot demand is recorded, and the
            // result is joined up to `Subtree` regardless.
            // A fact assignment has no value demand at all: its output IS its
            // input (the edit lane's value-identity class), but the path and
            // update graphs walk exactly as a value assignment's do.
            ProgramNode::Modify { paths, update, .. }
            | ProgramNode::FactAssign { paths, update, .. } => {
                let (paths, update) = (*paths, *update);
                self.demand(paths, ProjectionClass::Subtree, input);
                self.demand(update, ProjectionClass::Subtree, InputOrigin::Document);
                ProjectionClass::Subtree
            }
            // A binder's body runs over the binder's ORIGINAL input, so both the
            // body and the source read it. The body is walked FIRST: it is what
            // accumulates the slot's demand, which then becomes the SOURCE's
            // output demand. A handle consumed only by projected steps
            // (`$x.id`) keeps `Fields`; one that escapes to publication, a
            // barrier, or an error capture is already `Subtree` when it lands
            // in the slot.
            ProgramNode::Bind {
                source,
                slot,
                body,
                frame: _,
            } => {
                let (source, slot, body) = (*source, *slot, *body);
                let body = self.demand(body, out, input);
                let source = self.demand(source, self.slot(slot), input);
                body.join(source)
            }
            // `reduce`/`foreach` fold over a state whose demand the v1 table does
            // not pin, so the update (and a `foreach` extract's state input) runs
            // under `Subtree` — the conservative fixpoint, not an iterated one.
            // The SOURCE's output demand is the slot's, which is the whole point:
            // `reduce .catalog[] as $x (0; . + 1)` never reads `$x`.
            ProgramNode::Reduce {
                source,
                slot,
                init,
                update,
                keyed_collect: _,
            } => {
                let (source, slot, init, update) = (*source, *slot, *init, *update);
                self.demand(update, ProjectionClass::Subtree, InputOrigin::Document);
                let init = self.demand(init, ProjectionClass::Subtree, input);
                let source = self.demand(source, self.slot(slot), input);
                init.join(source)
            }
            ProgramNode::Foreach {
                source,
                slot,
                init,
                update,
                extract,
            } => {
                let (source, slot, init, update, extract) =
                    (*source, *slot, *init, *update, *extract);
                self.demand(update, ProjectionClass::Subtree, InputOrigin::Document);
                if let Some(extract) = extract {
                    self.demand(extract, out, InputOrigin::Document);
                }
                let init = self.demand(init, ProjectionClass::Subtree, input);
                let source = self.demand(source, self.slot(slot), input);
                init.join(source)
            }
            // A counted stream is a pass-through filter on its source: the
            // values it publishes ARE source values, so the source inherits this
            // node's own output demand unchanged.
            ProgramNode::Counted { source, .. } => {
                let source = *source;
                self.demand(source, out, input)
            }
            // The engine surface is STATEFUL: a pull's output depends on cursor
            // state rather than on the document, so its demand is the lattice
            // bottom for the element it is applied to, and everything it touches
            // reads whole. The binding walks both its graphs against whole-document
            // demand — a program that pulls a cursor is never a projected row.
            ProgramNode::EngineBind { source, body, .. } => {
                let (source, body) = (*source, *body);
                let body = self.demand(body, out, input);
                let source = self.demand(source, ProjectionClass::Subtree, input);
                body.join(source)
            }
            ProgramNode::EngineGenerator {
                init,
                update,
                extract,
            } => {
                let (init, update, extract) = (*init, *update, *extract);
                self.demand(update, ProjectionClass::Subtree, InputOrigin::Document);
                self.demand(extract, ProjectionClass::Subtree, InputOrigin::Document);
                self.demand(init, ProjectionClass::Subtree, input)
            }
        }
    }

    /// The builtin transfer table, read from the REGISTRY: the overload's record
    /// declares which combinator applies and this interprets it.
    ///
    /// Totality is by construction and Call-scoped: `demand_transfer` is a
    /// required record field, so a registered overload always declares one, and
    /// the match below is exhaustive over the closed
    /// [`DemandTransfer`] vocabulary. An id this registry does not know cannot
    /// reach here (the compiler mints call ids only through `resolve_builtin`),
    /// and if one ever did it takes the conservative arm rather than a panic.
    fn call_demand(
        &mut self,
        overload: u16,
        args: &[ProgramNodeId],
        out: ProjectionClass<'program>,
        input: InputOrigin,
    ) -> ProjectionClass<'program> {
        match jqf_builtins::registry::demand_transfer(overload) {
            // `length` on a CONSTRUCTED container is a count over boundaries the
            // constructor already knows; on a document value it is a codepoint
            // count or a numeric magnitude, both of which need the payload, so
            // it falls through to the conservative arm.
            Some(DemandTransfer::CountOfConstructedInput) if input == InputOrigin::Constructed => {
                self.walk_args(args, input);
                ProjectionClass::Structure
            }
            // `select(cond)` passes its input THROUGH unchanged when the
            // condition holds: its demand is the condition's ∪ the pass-through's.
            Some(DemandTransfer::ConditionUnionPassThrough) => {
                let mut demand = out;
                for arg in args {
                    demand = demand.join(self.demand(*arg, ProjectionClass::Subtree, input));
                }
                demand
            }
            // `not`: the demand passes straight through. The overload is TOTAL
            // (it raises on nothing, so no error path observes an unprojected
            // payload) and its output is a fresh boolean holding no part of the
            // input — so a payload-free demand on the output makes the input's
            // truthiness observationally irrelevant, and a payload demand on the
            // output hands the input the same payload demand.
            Some(DemandTransfer::InputPassThrough) => {
                self.walk_args(args, input);
                out
            }
            // The conservative arm: the declared `Subtree` transfer (`error/0`,
            // `error/1`), `length` on a document value, the `ViaLowering`
            // declaration (unreachable — a lowered call left no `Call` node
            // behind), and an id outside this registry.
            //
            // `type`, `keys`, `tag` and `has` land here too, and that is a
            // RULING rather than an omission: this lattice measures ONE STREAMED
            // ELEMENT, whose keys and kind ARE its payload as far as a P0/P1
            // route is concerned — `Structure` promises no element payload is
            // decoded at all, and `Fields(S)` promises the named members'
            // payloads, neither of which is "the element's own identities". The
            // demand they answer is the value at a PATH, served by the
            // whole-document lazy binding, not this lattice.
            Some(DemandTransfer::Subtree | DemandTransfer::CountOfConstructedInput | DemandTransfer::ViaLowering)
            | None => {
                self.walk_args(args, input);
                ProjectionClass::Subtree
            }
        }
    }

    /// Walks a call's argument graphs under full payload demand, discarding the
    /// result: their `.[]` boundaries and `$x` reads are real and must be
    /// recorded even when the call's own transfer ignores them.
    fn walk_args(&mut self, args: &[ProgramNodeId], input: InputOrigin) {
        for arg in args {
            self.demand(*arg, ProjectionClass::Subtree, input);
        }
    }

    /// The demand on the value ENTERING a stage's step list, walked backward
    /// from `out` (the demand on the value the last step produces).
    fn steps_demand(
        &mut self,
        stage: ProgramNodeId,
        steps: &'program [StageStep],
        out: ProjectionClass<'program>,
    ) -> ProjectionClass<'program> {
        let mut demand = out;
        // Sticky once an accessor has been walked past: a `.@`/`.&` accessor
        // reads a fact OF THE ELEMENT NODE, which no field set can name, so
        // every field step UPSTREAM of one must keep the whole-element demand
        // instead of narrowing to `Fields`. Narrowing there would promise the
        // field's payload while the fact need goes silently unserved.
        let mut fact_boundary = false;
        for (offset, step) in steps.iter().enumerate().rev() {
            demand = match step.access() {
                // A field access needs the field's payload only when the field's
                // own demand needs one: `map(.name) | length` never renders a
                // name, so `.name` under `Structure` stays `Structure`.
                StepAccess::Key(key) => {
                    if demand.is_structure_only() {
                        ProjectionClass::Structure
                    } else if fact_boundary {
                        ProjectionClass::Subtree
                    } else {
                        ProjectionClass::Fields(ProjectedFields::singleton(key.as_str()))
                    }
                }
                // An array position cannot be named in a field set.
                StepAccess::Index(_) => {
                    if demand.is_structure_only() {
                        ProjectionClass::Structure
                    } else {
                        ProjectionClass::Subtree
                    }
                }
                // The element boundary: the demand computed for the value this
                // `.[]` produces IS the per-element class.
                StepAccess::Each => {
                    self.record_element(stage, offset, demand.clone());
                    if demand.is_structure_only() {
                        ProjectionClass::Structure
                    } else {
                        ProjectionClass::Subtree
                    }
                }
                // `..` and `.[a:b]` are P2: recursive descent needs the whole
                // subtree by construction, and a slice materializes a new array
                // out of its subrange. A `.@`/`.&` accessor reads a fact OF THE
                // ELEMENT NODE itself, not a member of it, so no field set can
                // name it: the whole element (with its facts) must be delivered,
                // and the sticky boundary above keeps an upstream field step
                // from narrowing that promise away.
                StepAccess::Descend | StepAccess::Slice(_) => ProjectionClass::Subtree,
                StepAccess::NodeAccessor(_) | StepAccess::Attribute(_) => {
                    fact_boundary = true;
                    ProjectionClass::Subtree
                }
                // A dynamic index or accessor reads the bound value, so the
                // SLOT needs its payload and the container cannot be named. A
                // dynamic ACCESSOR is a fact read like its static twins, so it
                // sets the same boundary.
                StepAccess::DynVar(slot) => {
                    self.join_slot(*slot, ProjectionClass::Subtree);
                    ProjectionClass::Subtree
                }
                StepAccess::DynNodeAccessor(slot) | StepAccess::DynAttribute(slot) => {
                    self.join_slot(*slot, ProjectionClass::Subtree);
                    fact_boundary = true;
                    ProjectionClass::Subtree
                }
            };
        }
        demand
    }
}

/// Whether `id` is a stage that is a bare string-literal producer — a
pub fn is_static_literal_key(nodes: &[ProgramNode], id: ProgramNodeId) -> bool {
    matches!(
        &nodes[id.index()],
        ProgramNode::Stage {
            start: StageStart::Literal(Value::String(_)),
            steps,
        } if steps.is_empty()
    )
}

#[cfg(test)]
mod tests {
    use super::{INLINE_FIELDS, MAX_PROJECTED_FIELDS, ProjectedFields, ProjectionClass};
    use crate::CodecRequirementPolicy;
    use crate::compile::{CompileOptions, try_compile_program};
    use alloc::format;
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

    /// Compiles `source` through the REAL pipeline and renders its class as a
    /// stable string (`Structure`, `Fields[a,b]`, `Subtree`) — the same spelling
    /// the sdk-smoke receipts print.
    fn class_of(source: &str) -> String {
        let resources = resources();
        let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
        let program =
            try_compile_program(source, policy, CompileOptions::new(), &resources).expect("probe program compiles");
        match program.projection_class() {
            ProjectionClass::Structure => String::from("Structure"),
            ProjectionClass::Subtree => String::from("Subtree"),
            ProjectionClass::Fields(fields) => {
                let mut rendered = String::from("Fields[");
                for (position, name) in fields.names().iter().enumerate() {
                    if position > 0 {
                        rendered.push(',');
                    }
                    rendered.push_str(name);
                }
                rendered.push(']');
                rendered
            }
        }
    }

    fn fields(names: &[&'static str]) -> ProjectionClass<'static> {
        let mut set = ProjectedFields::singleton(names[0]);
        for name in &names[1..] {
            assert!(set.insert(name), "probe field set must fit");
        }
        ProjectionClass::Fields(set)
    }

    #[test]
    fn join_orders_the_three_lattice_points() {
        // Structure is the bottom: it never raises anything.
        assert_eq!(
            ProjectionClass::Structure.join(ProjectionClass::Structure),
            ProjectionClass::Structure
        );
        assert_eq!(ProjectionClass::Structure.join(fields(&["id"])), fields(&["id"]));
        assert_eq!(fields(&["id"]).join(ProjectionClass::Structure), fields(&["id"]));
        assert_eq!(
            ProjectionClass::Structure.join(ProjectionClass::Subtree),
            ProjectionClass::Subtree
        );
        // Subtree is the top: it absorbs everything, from both sides.
        assert_eq!(
            ProjectionClass::Subtree.join(ProjectionClass::Structure),
            ProjectionClass::Subtree
        );
        assert_eq!(ProjectionClass::Subtree.join(fields(&["id"])), ProjectionClass::Subtree);
        assert_eq!(fields(&["id"]).join(ProjectionClass::Subtree), ProjectionClass::Subtree);
        assert_eq!(
            ProjectionClass::Subtree.join(ProjectionClass::Subtree),
            ProjectionClass::Subtree
        );
    }

    #[test]
    fn join_unions_field_sets_sorted_and_deduplicated() {
        assert_eq!(fields(&["id"]).join(fields(&["id"])), fields(&["id"]));
        // Sorted, so the join is commutative in its RENDERED form too.
        assert_eq!(fields(&["name"]).join(fields(&["id"])), fields(&["id", "name"]));
        assert_eq!(fields(&["id"]).join(fields(&["name"])), fields(&["id", "name"]));
        assert_eq!(
            fields(&["id", "sku"]).join(fields(&["name", "sku"])),
            fields(&["id", "name", "sku"])
        );
    }

    #[test]
    fn a_field_set_wider_than_the_inline_capacity_joins_up_to_subtree() {
        // The inline array holds the first eight names, the ninth and beyond
        // overflow to a heap set, and only the codec's frozen bound (64) still
        // joins up to Subtree. Never a silently truncated field set, which
        // would be an UNSOUND projection.
        const NAMES: [&str; INLINE_FIELDS] = ["a", "b", "c", "d", "e", "f", "g", "h"];
        // The heap set still honors the codec's frozen bound: one more than it
        // reports false, and a join then lands on Subtree.
        const EXTRA: [&str; MAX_PROJECTED_FIELDS - INLINE_FIELDS] = [
            "i", "j", "k", "l", "m", "n", "o", "p", "q", "r", "s", "t", "u", "v", "w", "x", "y", "z", "aa", "ab", "ac",
            "ad", "ae", "af", "ag", "ah", "ai", "aj", "ak", "al", "am", "an", "ao", "ap", "aq", "ar", "as", "at", "au",
            "av", "aw", "ax", "ay", "az", "ba", "bb", "bc", "bd", "be", "bf", "bg", "bh", "bi", "bj", "bk", "bl",
        ];
        let mut full = ProjectedFields::singleton(NAMES[0]);
        for name in &NAMES[1..] {
            assert!(full.insert(name));
        }
        assert_eq!(full.names().len(), INLINE_FIELDS);
        // The ninth distinct field overflows into the heap set, still sorted.
        assert!(full.insert("i"));
        assert_eq!(full.names(), &["a", "b", "c", "d", "e", "f", "g", "h", "i"][..]);
        for name in EXTRA {
            assert!(full.insert(name));
        }
        assert_eq!(full.names().len(), MAX_PROJECTED_FIELDS);
        assert!(!full.insert("overflow"), "the 65th distinct field must not fit");
        assert_eq!(
            ProjectionClass::Fields(full).join(fields(&["overflow"])),
            ProjectionClass::Subtree
        );
    }

    #[test]
    fn transfer_table_length_on_a_constructed_container_is_structure() {
        // The P0 rows: `length` over a CONSTRUCTED array counts boundaries the
        // constructor already produced, so no element payload is ever needed —
        // and the demand propagates THROUGH the collect into the element stream.
        assert_eq!(class_of("[.catalog[]] | length"), "Structure");
        assert_eq!(class_of(".catalog | map(.name) | length"), "Structure");
        assert_eq!(class_of("[.catalog[] | .name] | length"), "Structure");
        assert_eq!(class_of("map(.name) | length"), "Structure");
        // The all-static-key construct joins the same rows: the count of a
        // constructed object is the element count, so the member VALUES carry
        // the outgoing Structure demand instead of Subtree (the count table's
        // construct row). A dynamic key can change the member count per
        // element, so it keeps the Subtree fallback and names its fields.
        assert_eq!(class_of("[.[] | {x: .id}] | length"), "Structure");
        assert_eq!(class_of("[.[] | {x: .id, y: .name}] | length"), "Structure");
        assert_eq!(class_of("[.[] | {(.k): .v}] | length"), "Fields[k,v]");
    }

    #[test]
    fn transfer_table_length_on_a_document_value_is_subtree() {
        // The review's adversarial pair: `length` on DOCUMENT values is a
        // codepoint count / numeric magnitude, so it needs the payloads.
        assert_eq!(class_of("[.catalog[] | length]"), "Subtree");
        assert_eq!(class_of("[.catalog[] | length] | length"), "Subtree");
    }

    #[test]
    fn transfer_table_select_joins_condition_and_pass_through_demand() {
        // Counted: only the condition's field escapes.
        assert_eq!(class_of("[.catalog[] | select(.id > 35990)] | length"), "Fields[id]");
        // Published: the pass-through is the whole element (the output rule).
        assert_eq!(class_of("[.catalog[] | select(.id > 35990)]"), "Subtree");
        // Pass-through PROJECTED: the union of both sides, and nothing more.
        assert_eq!(
            class_of("[.catalog[] | select(.id > 35990) | .name]"),
            "Fields[id,name]"
        );
    }

    #[test]
    fn transfer_table_fold_over_a_projected_field_names_only_that_field() {
        // `+` materializes both operands, so the fold reaches `.id` THROUGH the
        // operand graph — which names it and nothing else.
        assert_eq!(class_of("reduce .catalog[].id as $i (0; . + $i)"), "Fields[id]");
        // The slot is never read, so the source carries only the bottom demand.
        assert_eq!(class_of("reduce .catalog[] as $x (0; . + 1)"), "Structure");
        // The commutative mirror is the same fold row.
        assert_eq!(class_of("reduce .catalog[] as $x (0; 1 + .)"), "Structure");
        assert_eq!(class_of("foreach .catalog[] as $x (0; . + 1)"), "Structure");
    }

    #[test]
    fn a_key_step_is_free_when_its_own_demand_is_payload_free() {
        // `map(.name) | length` never RENDERS a name — the field is navigated but
        // not decoded, so the element demand stays at the lattice bottom.
        assert_eq!(class_of("[.catalog[] | .name] | length"), "Structure");
        // Published, the same step names the field.
        assert_eq!(class_of(".catalog[].name"), "Fields[name]");
        // A deeper path over-approximates to its FIRST component's subtree.
        assert_eq!(class_of("[.catalog[] | .attrs.color]"), "Fields[attrs]");
        // An array position cannot be named, so it takes the field's subtree.
        assert_eq!(class_of("[.catalog[] | .tags[0]]"), "Fields[tags]");
        // Static-key object construction is the P1 field-set row: each
        // member value is a static path, so the join is those keys.
        assert_eq!(class_of(".catalog[] | {id,name,sku}"), "Fields[id,name,sku]");
        assert_eq!(class_of(".catalog[] | {id: .id, name: .name}"), "Fields[id,name]");
        // A dynamic key can change the member count, so the construct stays
        // Subtree (the table's declining default).
        assert_eq!(class_of(".catalog[] | {(.k): .v}"), "Fields[k,v]");
    }

    #[test]
    fn an_accessor_step_demands_the_whole_element_through_upstream_fields() {
        // A `.@`/`.&` accessor reads a fact OF the element node itself, which
        // no field set can name: the element class stays Subtree.
        assert_eq!(class_of("[.catalog[] | .@tag]"), "Subtree");
        assert_eq!(class_of(".catalog[].a.&href"), "Subtree");
        // The law the field arm must not violate: an accessor DOWNSTREAM of a
        // field keeps the upstream demand whole instead of narrowing to that
        // field's payload — a Fields[price] answer here would promise the
        // price payload while the fact read goes silently unserved.
        assert_eq!(class_of("[.catalog[] | .price.@tag]"), "Subtree");
        // The dynamic-selector twin sets the same boundary.
        assert_eq!(class_of("[.catalog[] | \"tag\" as $r | .@($r)]"), "Subtree");
        // And a field chain with NO accessor still narrows: the boundary is
        // the accessor's, not every deep path's.
        assert_eq!(class_of("[.catalog[] | .price.name]"), "Fields[price]");
    }

    #[test]
    fn the_output_rule_sends_published_and_barrier_bound_elements_to_subtree() {
        assert_eq!(class_of(".catalog[]"), "Subtree");
        assert_eq!(class_of("[.catalog[]]"), "Subtree");
        assert_eq!(class_of("{items: [.catalog[]]}"), "Subtree");
        // `error/0` captures its input; `error/1` captures its argument's payload.
        assert_eq!(class_of("[.catalog[] | error] | length"), "Subtree");
        assert_eq!(class_of("[.catalog[] | error(.name)] | length"), "Subtree");
        // A `catch` handler is an error-value capture: the offending operand
        // renders into the message, so a handled `try` needs the payload. A
        // CATCHLESS try only swallows, and keeps the projection.
        assert_eq!(class_of("[.catalog[] | try .id catch .] | length"), "Subtree");
        assert_eq!(class_of("[.catalog[] | (.id)?]"), "Fields[id]");
    }

    #[test]
    fn a_bound_handle_keeps_fields_unless_it_escapes() {
        // ESCAPES to publication through the barrier: Subtree.
        assert_eq!(class_of("[.catalog[] as $x | $x]"), "Subtree");
        // Consumed only by a projected step: the handle keeps Fields.
        assert_eq!(class_of("[.catalog[] as $x | $x.id]"), "Fields[id]");
        // Consumed only by a payload-free count: the handle keeps the bottom.
        assert_eq!(class_of("[.catalog[] as $x | $x.id] | length"), "Structure");
    }

    #[test]
    fn descend_and_slice_are_born_subtree() {
        // Both steps are born-P2 vocabulary. `reduce ..`
        // never reads its slot, yet the SHAPE has no element stream to project
        // (there is no `.[]` boundary at all), so it classifies Subtree.
        assert_eq!(class_of("reduce .. as $x (0; . + 1)"), "Subtree");
        // Applied to the ELEMENT, either step takes the whole element.
        assert_eq!(class_of("[.catalog[] | ..] | length"), "Subtree");
        assert_eq!(class_of("[.catalog[] | .[1:3]] | length"), "Subtree");
        // Applied BELOW a field, born-P2 stops at that field: the slice needs
        // `tags` whole, and still nothing else on the element.
        assert_eq!(class_of("[.catalog[] | .tags[1:3]] | length"), "Fields[tags]");
    }

    #[test]
    fn a_conservatively_declared_builtin_classifies_subtree() {
        // The conservative declaration, pinned through `keys` — a REGISTERED
        // builtin whose record declares `DemandTransfer::Subtree` because its
        // key strings ARE payload (a `Fields`-of-keys vocabulary is future
        // work). A newly registered builtin declaring the same tag inherits this
        // arm untouched.
        assert_eq!(class_of("[.catalog[] | keys] | length"), "Subtree");
        // The conservative arm is `Subtree` on the builtin's OWN input, not on
        // the element: reached through a projected path, it still bounds the
        // element demand to that path.
        assert_eq!(class_of("[.catalog[] | .id | keys] | length"), "Fields[id]");
    }

    #[test]
    fn not_passes_its_output_demand_through_to_its_input() {
        // The `InputPassThrough` declaration — a classification change from
        // the side table, where `not` sat in the unpinned default and both of
        // the first two rows read `Subtree`/`Fields[id]`.
        //
        // Soundness: `not` is TOTAL on every value (it raises on nothing, so no
        // error path can observe an unprojected payload) and its output is a
        // fresh boolean carrying no part of the input. When that output is
        // demanded payload-free — as under a count — the input's truthiness is
        // never observable, so nothing of the input is needed.
        assert_eq!(class_of("[.catalog[] | not] | length"), "Structure");
        assert_eq!(class_of("[.catalog[] | .id | not] | length"), "Structure");
        // When the boolean IS rendered, the pass-through hands the input the
        // same payload demand, and it arrives through the already-projected
        // path — so the field set is exactly the path's, never wider.
        assert_eq!(class_of("[.catalog[] | .id | not]"), "Fields[id]");
        assert_eq!(class_of("[.catalog[] | not]"), "Subtree");
    }

    #[test]
    fn a_program_with_no_element_boundary_has_nothing_to_project() {
        // A scoped LOCATE, not an element stream.
        assert_eq!(class_of(".catalog[0].id"), "Subtree");
        assert_eq!(class_of("."), "Subtree");
        assert_eq!(class_of("length"), "Subtree");
    }

    #[test]
    fn a_sole_named_boundary_records_at_that_boundary_and_nested_keeps_the_join() {
        // The class is recorded at the NAMED boundary when the program has a
        // sole `.[]`. With one boundary the exact record and the join coincide
        // by construction, so the answers do not move — only the machinery did.
        assert_eq!(class_of("[.catalog[] | .name] | length"), "Structure");
        assert_eq!(class_of("reduce .catalog[] as $x (0; . + 1)"), "Structure");
        // Nested iteration keeps the conservative join: the OUTER element's own
        // demand here is `Fields[tags]`, but separating it from the inner
        // `.[]`'s needs a soundness argument the naming does not make, so the
        // join stands.
        assert_eq!(class_of("[.catalog[] | .tags[]]"), "Subtree");
        assert_eq!(class_of("[.catalog[] | .tags[]] | length"), "Structure");
    }

    #[test]
    fn a_wide_field_set_still_classifies_fields() {
        // A PUBLISHED construct projecting between 9 and 64 top-level members
        // keeps the Fields class (and therefore the projected route) instead of
        // joining up to Subtree on the ninth.
        assert_eq!(
            class_of("[.catalog[] | {a:.a, b:.b, c:.c, d:.d, e:.e, f:.f, g:.g, h:.h, i:.i}]"),
            "Fields[a,b,c,d,e,f,g,h,i]"
        );
        // Under `| length` the SAME construct's values carry the outgoing
        // Structure demand (the all-static-key exception): the count of a
        // constructed object is the element count, so no member payload is
        // needed. The multi-path count row declines below that; this single-
        // construct shape is answered whole by the count demand.
        assert_eq!(
            class_of("[.catalog[] | {a:.a, b:.b, c:.c, d:.d, e:.e, f:.f, g:.g, h:.h, i:.i}] | length"),
            "Structure"
        );
        // The engine's bound is the classifier's own frozen bound: a PUBLISHED
        // 65-member projection joins up to Subtree rather than naming a field
        // set wider than the bound.
        let members = (0..65)
            .map(|index| format!("f{index}: .f{index}"))
            .collect::<alloc::vec::Vec<_>>()
            .join(", ");
        let program = format!("[.catalog[] | {{{members}}}] | length");
        assert_eq!(class_of(&program), "Structure");
        let published = format!("[.catalog[] | {{{members}}}]");
        assert_eq!(class_of(&published), "Subtree");
    }

    #[test]
    fn the_class_renders_the_receipt_table_rows() {
        // Every receipt-table row, in table order — the in-crate twin.
        let table = [
            ("reduce .catalog[] as $x (0; . + 1)", "Structure"),
            ("[.catalog[]] | length", "Structure"),
            (".catalog | map(.name) | length", "Structure"),
            ("reduce .catalog[].id as $i (0; . + $i)", "Fields[id]"),
            ("[.catalog[] | select(.id > 35990)] | length", "Fields[id]"),
            ("[.catalog[] | select(.id > 35990)]", "Subtree"),
            ("[.catalog[] | length]", "Subtree"),
        ];
        for (source, expected) in table {
            assert_eq!(class_of(source), expected, "row {source:?} must classify {expected}");
        }
    }
}
