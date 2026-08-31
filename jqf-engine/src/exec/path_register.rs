//! The path-provenance register carried by the one evaluator.
//!
//! One job: own the single register that answers "WHERE did this value come
//! from" while a path-family body evaluates on the graph machine. The register
//! is `steps` (the accumulated path) plus `at` (the value that path currently
//! addresses). The machine calls into this module; the register's five laws
//! are written here as prose, because a test that pins a behavior teaches a
//! fact, and a maintainer reading a red test needs the law.
//!
//! # Law 1 — Provenance is dynamic, not static
//!
//! One register — a path plus the value that path currently addresses. At
//! every navigation and every emission the engine answers exactly one
//! question: *is the value in hand the value the register addresses?*
//! [`PathRegister::tracks`] is that question. Path-capability is never
//! decided statically: `. as $x | $x.a` is a path because `$x` is still, by
//! identity, the value at the empty path, while `1 + 1` is never one even
//! when it equals the tracked value.
//!
//! # Law 2 — Navigation
//!
//! Navigation over a tracked value extends the register; over anything else
//! it raises `Invalid path expression near attempt to access element …`.
//! The check runs BEFORE the access and survives the optional form: `?`
//! suppresses the type mismatch below, never this rejection.
//!
//! # Law 3 — Emission
//!
//! Emission of a tracked value publishes the register; of anything else it
//! raises `Invalid path expression with result …`. The emission is checked at
//! `path`'s own boundary after the argument completes, so an inner `try`/`?`
//! is out of scope when the check runs.
//!
//! # Law 4 — `foreach` extract emits the item's PATH
//!
//! `foreach .a[] as $i (0; .+1; $i)` yields `["a",0]`, not an error — the
//! bound item still IS the value the register addresses. The extract runs
//! with the register sitting on the bound item.
//!
//! # Law 5 — `reduce` update runs with the register on the SOURCE item
//!
//! `reduce .a.b as $x (.; .a)` raises, even though `.a` is the most
//! path-capable expression there is, because the source loop does not restore
//! the register before the update.
//!
//! # Subexpression positions freeze the register
//!
//! A binder's source, a conditional's condition, a binary operand, a
//! constructed array's or object's body, a predicate, and a builtin's
//! argument all evaluate with the register FROZEN ([`PathRegister::is_frozen`]):
//! their outputs are values, never locations. A frozen navigation walks the
//! value and records nothing — it neither extends the register nor raises the
//! Law-2 rejection; a type mismatch inside it still raises its own class. The
//! binary and object-construction RESULT emissions temporarily UNFREEZE so
//! the Law-3 check runs (`path(1 == 1)` over `true` is `[]`).
//!
//! Logical operands (`and`/`or`) are deliberately NOT frozen: the left's
//! navigations track, then the right's, and the boolean result is checked at
//! the emission — `path(.e and .)` over `{"e":false}` is `["e"]` while
//! `path(.e or .)` over the same input errors with result `true`.

use alloc::vec::Vec;

use jqf_data::{ScalarView, Value, ValueKind};
use jqf_resource::ResourceContext;

use crate::exec::stage::Container;
use crate::exec::{EngineRunError, FactDelta, GraphMachine, PartialSort, RunPoll, StepScratch};
use crate::program::{ProgramNode, ProgramNodeId, SliceBound, StageStep, StepAccess};
use jqf_builtins::codec_result::EngineResult;
use jqf_builtins::semantics::path::{KeyIntern, PathStep, charged_key, path_value, step_value};
use jqf_data::NodeHandle;

/// Which path-family builtin one register drive serves.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PathFamily {
    /// `path(f)` — publish the LOCATION of every output of `f`.
    Path,
    /// `paths` — every non-root location of the input, native.
    Paths,
    /// `getpath(P)` — publish the value at every path `P` names.
    GetPath,
    /// `setpath(P; V)` — publish the input rewritten at every `(P, V)` pair.
    SetPath,
    /// `delpaths(P)` — publish the input with every path set `P` names removed.
    DelPaths,
}

/// The pre-extension state of a register captured by one navigation or one
/// frozen position, restored when the navigation's continuation completes or
/// the frozen position's drive ends (by completion or by the raise walk).
#[derive(Debug)]
pub(crate) struct RegisterSnapshot {
    steps_len: usize,
    at: Value,
    at_node: Option<NodeHandle>,
    frozen: u32,
}

impl Clone for PathRegister {
    fn clone(&self) -> Self {
        Self {
            steps: self.steps.clone(),
            at: self.at.clone(),
            at_node: self.at_node,
            frozen: self.frozen,
            // A nested machine interns its OWN key texts; a fresh table is the
            // same charge-once discipline, scoped to the nested drive. The
            // frozen stack is empty whenever a register is cloned (frozen
            // positions never span a nested drive boundary).
            saved: Vec::new(),
            keys: KeyIntern::default(),
        }
    }
}

/// The path-provenance register.
pub(crate) struct PathRegister {
    /// The tracked path, as a stack of navigation steps.
    steps: Vec<PathStep>,
    /// The value the tracked path currently addresses.
    at: Value,
    /// The located node that materializes to [`Self::at`], when the register
    /// is sitting on a document node rather than a constructed value. A later
    /// located result with this handle IS the register's address (Law 1), so
    /// a path body can stay located — markup member steps need the document's
    /// name facts — without rematerializing a twin that would fail the
    /// allocation-identity test.
    at_node: Option<NodeHandle>,
    /// Subexpression nesting: non-zero freezes the register.
    frozen: u32,
    /// The snapshot stack of OPEN frozen positions.
    saved: Vec<RegisterSnapshot>,
    /// The shared key texts this register's steps have already paid for.
    keys: KeyIntern,
}

impl PathRegister {
    /// Seeds a register over `root`: the empty path addresses the root.
    ///
    /// `at_node` is the located input node that materialized to `root`, or
    /// `None` when the body starts on an owned value.
    pub(crate) fn new(root: Value, at_node: Option<NodeHandle>) -> Self {
        Self {
            steps: Vec::new(),
            at: root,
            at_node,
            frozen: 0,
            saved: Vec::new(),
            keys: KeyIntern::default(),
        }
    }

    /// Whether `value` IS the value the register addresses.
    ///
    /// Containers answer by allocation identity: a value reached by navigating
    /// and then bound, passed through a filter, or carried as a fold state is
    /// still the same allocation, and an independently built twin is not.
    /// Numbers, strings, and byte strings answer the same way; only
    /// `null`/`true`/`false` (and inline machine numbers and floats) answer by
    /// value, which is also the test: they are inline and bit-identical to
    /// themselves.
    pub(crate) fn tracks(&self, value: &Value) -> bool {
        self.frozen == 0 && identical(value, &self.at)
    }

    /// Law 1 over an in-flight result: a located value whose node is the
    /// register's current address tracks without rematerializing; an owned
    /// value uses [`Self::tracks`]. A rematerialization of that same node
    /// reuses [`Self::at`] so the owned twin still shares the register
    /// allocation — see [`GraphMachine::materialize_uncharged`].
    ///
    /// A miss (`null as [$v]`, `.a` on null) yields owned null and clears
    /// `at_node`. The original located null is still that value, so a later
    /// navigation or emission of the unchanged dot still tracks — the same
    /// by-value rule owned nulls already use.
    pub(crate) fn tracks_result(&self, value: &EngineResult<'_>) -> bool {
        if self.frozen != 0 {
            return false;
        }
        match value {
            EngineResult::Located(located) => {
                self.at_node == Some(located.node()) || inline_located_tracks(&self.at, located)
            }
            EngineResult::Owned(owned) => self.tracks(owned),
        }
    }

    /// The owned address already held for `node`, when `node` is the current
    /// located address. A rematerialization of that node must share this
    /// allocation (Law 1): a fresh twin fails [`Self::tracks`] even though it
    /// is the register address.
    pub(crate) fn owned_for_node(&self, node: NodeHandle) -> Option<Value> {
        (self.at_node == Some(node)).then(|| self.at.clone())
    }

    /// Whether the register is inside a frozen subexpression position.
    pub(crate) const fn is_frozen(&self) -> bool {
        self.frozen > 0
    }

    /// Enters one frozen subexpression position: navigation inside it neither
    /// extends the register nor raises the Law-2 rejection.
    pub(crate) fn enter_frozen(&mut self) {
        self.saved.push(self.snapshot());
        self.frozen = self.frozen.saturating_add(1);
    }

    /// Leaves one frozen position at its consumer (restoring the snapshot).
    pub(crate) fn exit_frozen(&mut self) {
        if let Some(snapshot) = self.saved.pop() {
            self.restore(snapshot);
        }
    }

    /// Abandons one frozen position popped mid-drive by the raise walk.
    pub(crate) fn abandon_frozen(&mut self) {
        self.saved.pop();
        self.frozen = self.frozen.saturating_sub(1);
    }

    /// Captures the register for a frozen position or a navigation scope.
    pub(crate) fn snapshot(&self) -> RegisterSnapshot {
        RegisterSnapshot {
            steps_len: self.steps.len(),
            at: self.at.clone(),
            at_node: self.at_node,
            frozen: self.frozen,
        }
    }

    /// Restores a snapshot taken by [`Self::snapshot`].
    pub(crate) fn restore(&mut self, snapshot: RegisterSnapshot) {
        self.steps.truncate(snapshot.steps_len);
        self.at = snapshot.at;
        self.at_node = snapshot.at_node;
        self.frozen = snapshot.frozen;
    }

    /// The pre-extension state for ONE navigation: the steps length and the
    /// addressed value before the navigation. The caller restores it through
    /// [`Self::restore_navigation`] when the navigation's continuation
    /// completes.
    pub(crate) fn pre_navigation(&self) -> (usize, Value, Option<NodeHandle>) {
        (self.steps.len(), self.at.clone(), self.at_node)
    }

    /// Restores the register to the state [`Self::pre_navigation`] captured.
    pub(crate) fn restore_navigation(&mut self, steps_len: usize, at: Value, at_node: Option<NodeHandle>) {
        self.steps.truncate(steps_len);
        self.at = at;
        self.at_node = at_node;
    }

    /// Extends the register through one resolved navigation.
    pub(crate) fn push_step(
        &mut self,
        step: PathStep,
        at: Value,
        at_node: Option<NodeHandle>,
    ) -> Result<(), EngineRunError> {
        self.steps
            .try_reserve(1)
            .map_err(|_| EngineRunError::allocation_failure())?;
        self.steps.push(step);
        self.at = at;
        self.at_node = at_node;
        Ok(())
    }

    /// Renders the accumulated steps as the path array a publication emits.
    pub(crate) fn path_value(&self, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
        path_value(&self.steps, resources)
    }

    /// The path components a stage's steps at `steps[at..]` contribute, in
    /// order, extending this register's key intern table.
    ///
    /// `Key`/`Index` steps contribute their component directly; a `DynVar`
    /// step contributes the bound value's component (key/index/slice-object/
    /// subsequence); a `Slice` step contributes the authored-bounds component.
    /// `Each` and `Descend` contribute nothing here — their children are
    /// registered per child by the drive — and an accessor step (`@`/`&`)
    /// records NO component (an attached fact is not a navigable owned path).
    ///
    /// `slots` is the machine's env, read for `DynVar` and `$var` slice
    /// bounds. The machine's own descent already raised the index-class
    /// mismatch for a non-component `DynVar` bound, so reaching this helper
    /// means every dynamic step carried a component bound.
    pub(crate) fn stage_components(
        &mut self,
        steps: &[StageStep],
        at: usize,
        slots: &[Option<Value>],
        resources: &ResourceContext<'_>,
    ) -> Result<Vec<PathStep>, EngineRunError> {
        let mut components = Vec::new();
        for step in steps.iter().skip(at) {
            match step.access() {
                StepAccess::Key(name) => {
                    components.push(PathStep::Key(self.keys.intern(name, resources)?));
                }
                StepAccess::Index(index) => components.push(PathStep::Index(*index)),
                StepAccess::DynVar(slot) => {
                    let bound = slots.get(*slot as usize).and_then(Option::as_ref).ok_or_else(|| {
                        EngineRunError::internal_contract("a variable step read an env slot outside the sized env")
                    })?;
                    let component = match bound.untagged() {
                        // The bound value's text is already charged, so the
                        // step retains that allocation instead of paying for a
                        // copy.
                        Value::String(name) => PathStep::Key(name.clone_shared()),
                        Value::Number(_) => number_step(bound)?,
                        // The slice-object key: the AUTHORED object is emitted
                        // verbatim, exactly as the step's own walk records it.
                        Value::Object(_) => PathStep::SliceObject(bound.clone()),
                        // The subsequence-search key.
                        Value::Array(needle) => PathStep::Subsequence(Value::Array(needle.clone())),
                        _ => {
                            // The machine's own descent already raised the
                            // index-class mismatch for a non-component bound,
                            // so a successfully descended DynVar step can only
                            // carry a key/index/slice-object/subsequence bound.
                            return Err(EngineRunError::internal_contract(
                                "a descended dynamic step carried a non-component bound",
                            ));
                        }
                    };
                    components.push(component);
                }
                StepAccess::Slice(bounds) => {
                    let start = Self::authored_bound_value(&bounds.start, bound_slot(&bounds.start, slots));
                    let end = Self::authored_bound_value(&bounds.end, bound_slot(&bounds.end, slots));
                    components.push(PathStep::Slice { start, end });
                }
                // An `Each`/`Descend` fan-out and an accessor step record NO
                // path component here (the fan-out's children are registered
                // per child; an attached fact is not a navigable path).
                StepAccess::Each
                | StepAccess::Descend
                | StepAccess::NodeAccessor(_)
                | StepAccess::Attribute(_)
                | StepAccess::DynNodeAccessor(_)
                | StepAccess::DynAttribute(_) => {}
            }
        }
        Ok(components)
    }

    /// The owned value one component of a bound `$var` step navigates with, as
    /// the authored step spells it.
    ///
    /// Returns `None` for an `Open` or `null` bound — the slice emits `null`
    /// for an absent bound.
    pub(crate) fn authored_bound_value(bound: &SliceBound, slot: Option<&Value>) -> Option<Value> {
        match bound {
            SliceBound::Open => None,
            SliceBound::Literal(value) => match value.untagged() {
                Value::Null => None,
                _ => Some(value.clone()),
            },
            SliceBound::Var(_) => match slot.map(Value::untagged) {
                Some(Value::Null) | None => None,
                _ => slot.cloned(),
            },
        }
    }
}

/// The env value a `$var` slice bound names, or `None` for a non-var bound.
fn bound_slot<'a>(bound: &SliceBound, slots: &'a [Option<Value>]) -> Option<&'a Value> {
    let SliceBound::Var(slot) = bound else {
        return None;
    };
    slots.get(*slot as usize).and_then(Option::as_ref)
}

/// Boxes a computed MACHINE integer or inline float so the path register's
/// allocation identity can reject it.
///
/// A computed number is allocated, so the allocation-identity test refuses it
/// even when it EQUALS the register's tracked one (`path(1 == 1)` over `1`
/// is `Invalid path expression with result true`, never a published path).
/// Inline scalars (bools, null, machine ints, inline floats) carry no
/// allocation for the test to see, so a literal seed is given a boxed arm —
/// `path(1.5)` over `1.5` rejects, exactly as `path(7)` over `7` does.
pub(crate) fn box_computed(value: Value) -> Value {
    match value {
        Value::Number(number) => match number.as_machine() {
            Some(machine) => Value::Number(jqf_data::Number::integer(jqf_data::Integer::boxed_from_i64(machine))),
            None => match number.as_float() {
                Some(float) => Value::Number(jqf_data::Number::boxed_float(float)),
                None => Value::Number(number),
            },
        },
        other => other,
    }
}

/// The `PathStep::Index` for a number component value (a path ARRAY's
/// component — the `getpath` follow).
pub(crate) fn number_component(value: &Value) -> Result<PathStep, EngineRunError> {
    number_step(value)
}

/// The `PathStep::Index` for a number component value.
fn number_step(value: &Value) -> Result<PathStep, EngineRunError> {
    let Value::Number(number) = value.untagged() else {
        return Err(EngineRunError::internal_contract("a number component lost its number"));
    };
    if jqf_builtins::semantics::order::to_f64(number).is_nan() {
        return Ok(PathStep::IndexNaN);
    }
    Ok(PathStep::Index(number_index(value)?))
}

/// The authored integer index of a number component.
fn number_index(value: &Value) -> Result<i64, EngineRunError> {
    let Value::Number(number) = value.untagged() else {
        return Err(EngineRunError::internal_contract("a number component lost its number"));
    };
    let raw = jqf_builtins::semantics::order::to_f64(number);
    let truncated = if raw < 0.0 {
        jqf_builtins::semantics::arith::ceil_binary64(raw)
    } else {
        jqf_builtins::semantics::arith::floor_binary64(raw)
    };
    #[allow(
        clippy::cast_possible_truncation,
        reason = "an out-of-range position saturates, and the navigation then yields null"
    )]
    Ok(truncated as i64)
}

/// Law 1's by-value half when the register sits on an owned inline scalar
/// and the candidate is a located node of the same scalar. Containers still
/// require node or allocation identity — a constructed equal object does not
/// track.
fn inline_located_tracks(at: &Value, located: &jqf_codec_core::LocatedProduct<'_>) -> bool {
    match at.untagged() {
        Value::Null | Value::Bool(_) => {}
        _ => return false,
    }
    let Ok(view) = located.product().document().payload_view(located.node()) else {
        return false;
    };
    let Ok(Some(scalar)) = view.scalar() else {
        return false;
    };
    match (at.untagged(), scalar) {
        (Value::Null, ScalarView::Null) => true,
        (Value::Bool(left), ScalarView::Bool(right)) => *left == right,
        _ => false,
    }
}

/// The allocation-identity test, over the owned value model.
///
/// Two inline machine integers, two inline floats, two booleans, and two
/// nulls ARE their value; everything else compares by allocation. This is the
/// whole of Law 1's "is the value in hand the value the register addresses?":
/// a navigation-extracted value satisfies it, an independently built equal
/// value does not.
fn identical(left: &Value, right: &Value) -> bool {
    match (left.untagged(), right.untagged()) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(left), Value::Bool(right)) => left == right,
        (Value::Number(l), Value::Number(r)) => match (l.as_machine(), r.as_machine()) {
            (Some(li), Some(ri)) => li == ri,
            _ => match (l.as_float(), r.as_float()) {
                (Some(lf), Some(rf)) if l.is_inline_float() && r.is_inline_float() => {
                    lf.get().to_bits() == rf.get().to_bits()
                }
                _ => left.shares_allocation_with(right),
            },
        },
        _ => left.shares_allocation_with(right),
    }
}

struct WalkFrame<'source> {
    /// The container whose children remain to visit.
    container: crate::exec::stage::Container<'source>,
    /// The next child position to visit.
    next: usize,
    /// The register length at this container's own position.
    depth: usize,
}

/// A RESUMABLE native `..` walk over one engine value.
///
/// [`PathRun::descend_walk`] is an explicit-stack loop, but its caller drove it
/// to completion in one call, so a lazy consumer upstream of `paths`/
/// `path(..)` had nothing to cut: `[limit(2; paths(scalars))]` collected every
/// path of the document before the countdown could fire (the leftover). This
/// struct is that loop lifted out: `next_child`
/// advances until one child is produced and returns it, retaining the walk
/// state — the descend frame stack, the register, and the key interner —
/// between calls. The graph machine's `PathWalkEmit` producer frame holds it,
/// so a countdown cut drops the frame and the walk with it.
///
/// The walk runs over the LOCATED document (or an owned constructed value),
/// never a whole-document materialization: children are retained one at a
/// time, so `limit(2; paths(scalars))` walks two scalars and pays for two
/// children, not for the document. A deferred container span is materialized
/// on touch, exactly as the stage walk's entry does.
pub(crate) struct PathWalk<'source> {
    /// The tracked path of the last produced child.
    register: Vec<PathStep>,
    /// The shared key texts this walk's register steps have already paid for.
    keys: KeyIntern,
    /// The suspended descend frames.
    stack: Vec<WalkFrame<'source>>,
    /// The root, handed out first (pre-order self-first); `None` once emitted.
    root: Option<EngineResult<'source>>,
}

impl<'source> PathWalk<'source> {
    /// Seeds a walk over `root`; the root itself is the first child.
    pub(crate) fn new(root: EngineResult<'source>) -> Self {
        Self {
            register: Vec::new(),
            keys: KeyIntern::default(),
            stack: Vec::new(),
            root: Some(root),
        }
    }

    /// Whether the register names the root (the empty path) — the `paths`
    /// root-exclusion test, which is the register's depth exactly as the
    /// lowered `select(length > 0)` spells it.
    pub(crate) fn at_root(&self) -> bool {
        self.register.is_empty()
    }

    /// The path the register names, as a value.
    pub(crate) fn current_path(&self, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
        path_value(&self.register, resources)
    }

    /// Advances to the next walked child, or returns `None` when the walk is
    /// exhausted.
    pub(crate) fn next_child(
        &mut self,
        scratch: &mut StepScratch<'_, '_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        if let Some(root) = self.root.take() {
            // A deferred container span is materialized on touch (the CBOR
            // whole-document route's source-retention mode), exactly as the
            // stage walk's entry does before its first step.
            let root = match root {
                EngineResult::Located(located) => {
                    let document = located.product().document();
                    match crate::exec::stage::materialize_if_span(document, located.node(), scratch)? {
                        Some(owned) => EngineResult::owned(owned),
                        None => EngineResult::Located(located),
                    }
                }
                EngineResult::Owned(owned) => EngineResult::Owned(owned),
            };
            if crate::exec::stage::engine_container_len(&root)? > 0 {
                let container = match &root {
                    EngineResult::Located(located) => {
                        crate::exec::stage::Container::Located(located.try_clone().map_err(EngineRunError::Codec)?)
                    }
                    EngineResult::Owned(value) => crate::exec::stage::Container::Owned(value.clone()),
                };
                self.stack
                    .try_reserve(1)
                    .map_err(|_| EngineRunError::allocation_failure())?;
                self.stack.push(WalkFrame {
                    container,
                    next: 0,
                    depth: 0,
                });
            }
            return Ok(Some(root));
        }
        while let Some(frame) = self.stack.last_mut() {
            let index = frame.next;
            frame.next += 1;
            let depth = frame.depth;
            let Some((component, child)) = walk_child(&frame.container, index, &mut self.keys, scratch)? else {
                self.stack.pop();
                continue;
            };
            self.register.truncate(depth);
            self.register
                .try_reserve(1)
                .map_err(|_| EngineRunError::allocation_failure())?;
            self.register.push(component);
            if crate::exec::stage::engine_container_len(&child)? > 0 {
                let depth = self.register.len();
                let container = match &child {
                    EngineResult::Located(located) => {
                        crate::exec::stage::Container::Located(located.try_clone().map_err(EngineRunError::Codec)?)
                    }
                    EngineResult::Owned(value) => crate::exec::stage::Container::Owned(value.clone()),
                };
                self.stack
                    .try_reserve(1)
                    .map_err(|_| EngineRunError::allocation_failure())?;
                self.stack.push(WalkFrame {
                    container,
                    next: 0,
                    depth,
                });
            }
            return Ok(Some(child));
        }
        Ok(None)
    }
}

/// The `cursor`-th child of a walk container, with the register step that
/// reaches it.
fn walk_child<'source>(
    container: &crate::exec::stage::Container<'source>,
    cursor: usize,
    keys: &mut KeyIntern,
    scratch: &mut StepScratch<'_, '_>,
) -> Result<Option<(PathStep, EngineResult<'source>)>, EngineRunError> {
    match container {
        crate::exec::stage::Container::Owned(value) => {
            let Some((step, child)) = each_child_at(value, cursor, keys, scratch.resources())? else {
                return Ok(None);
            };
            Ok(Some((step, EngineResult::owned(child))))
        }
        crate::exec::stage::Container::Located(located) => {
            crate::exec::stage::located_walk_child(located, cursor, keys, scratch)
        }
        // A markup member step's filtered fan-out: map the cursor through
        // the matched positions exactly as the graph machine's `next_child`
        // does, so the path component is the document's real index.
        crate::exec::stage::Container::LocatedFiltered(located, positions) => {
            let Some(position) = positions.get(cursor) else {
                return Ok(None);
            };
            crate::exec::stage::located_walk_child(located, *position, keys, scratch)
        }
    }
}

/// The (component, child) pair one container child contributes, for the walk's
/// OWNED containers (the located half uses [`crate::exec::stage::located_walk_child`]).
fn each_child_at(
    value: &Value,
    index: usize,
    keys: &mut KeyIntern,
    resources: &ResourceContext<'_>,
) -> Result<Option<(PathStep, Value)>, EngineRunError> {
    match value.untagged() {
        Value::Array(array) => match array.get(index) {
            Some(child) => {
                let position = i64::try_from(index).map_err(|_| EngineRunError::allocation_failure())?;
                Ok(Some((PathStep::Index(position), child.clone())))
            }
            None => Ok(None),
        },
        Value::Object(object) => match object.get_index(index) {
            Some(entry) => Ok(Some((
                PathStep::Key(keys.intern(entry.key(), resources)?),
                entry.value().clone(),
            ))),
            None => Ok(None),
        },
        _ => Ok(None),
    }
}

/// The component VALUE a step navigates with, rendered for the untracked
/// rejection's accessor text (`"a"`, `0`, the slice object, ...).
///
/// Returns `None` for a `DynVar` step whose bound is not a component — the
/// machine's own descent raises that index-class mismatch, and the
/// untracked rejection must not preempt it. `env` is the machine's slot
/// vector, read for the bound value.
pub(crate) fn step_component_value(
    step: &StageStep,
    env: &[Option<Value>],
    resources: &mut ResourceContext<'_>,
) -> Result<Option<Value>, EngineRunError> {
    let step = match step.access() {
        StepAccess::Key(name) => PathStep::Key(charged_key(name, resources)?),
        StepAccess::Index(index) => PathStep::Index(*index),
        StepAccess::DynVar(slot) => {
            let Some(bound) = env.get(*slot as usize).and_then(Option::as_ref) else {
                return Err(EngineRunError::internal_contract(
                    "a variable step read an env slot outside the sized env",
                ));
            };
            match bound.untagged() {
                Value::String(name) => PathStep::Key(name.clone_shared()),
                Value::Number(_) => PathStep::Index(0),
                Value::Object(_) => PathStep::SliceObject(bound.clone()),
                Value::Array(needle) => PathStep::Subsequence(Value::Array(needle.clone())),
                _ => return Ok(None),
            }
        }
        StepAccess::Slice(bounds) => PathStep::Slice {
            start: PathRegister::authored_bound_value(&bounds.start, None),
            end: PathRegister::authored_bound_value(&bounds.end, None),
        },
        _ => {
            return Err(EngineRunError::internal_contract(
                "a non-access step reached the accessor renderer",
            ));
        }
    };
    Ok(Some(step_value(&step, resources)?))
}

/// The path component one container child contributes (an array's position,
/// an object's member key), or `None` for a container the walk cannot name a
/// child of.
pub(crate) fn child_component(
    container: &Container<'_>,
    cursor: usize,
    resources: &mut ResourceContext<'_>,
) -> Result<Option<PathStep>, EngineRunError> {
    let component = match container {
        Container::Owned(owned) => match owned.untagged() {
            Value::Array(_) => jqf_builtins::semantics::path::PathStep::Index(
                i64::try_from(cursor).map_err(|_| EngineRunError::allocation_failure())?,
            ),
            Value::Object(object) => {
                let Some(entry) = object.get_index(cursor) else {
                    return Ok(None);
                };
                jqf_builtins::semantics::path::PathStep::Key(jqf_builtins::semantics::path::charged_key(
                    entry.key(),
                    resources,
                )?)
            }
            _ => return Ok(None),
        },
        Container::Located(located) => {
            return located_child_component(located, cursor, resources);
        }
        Container::LocatedFiltered(located, positions) => {
            let Some(&position) = positions.get(cursor) else {
                return Ok(None);
            };
            return located_child_component(located, position, resources);
        }
    };
    Ok(Some(component))
}

/// The path component one located container child contributes.
fn located_child_component(
    located: &jqf_codec_core::LocatedProduct<'_>,
    cursor: usize,
    resources: &mut ResourceContext<'_>,
) -> Result<Option<PathStep>, EngineRunError> {
    let document = located.product().document();
    let view = document
        .payload_view(located.node())
        .map_err(|_| EngineRunError::internal_contract("join container view failed over a valid node"))?;
    let kind = view
        .kind()
        .map_err(|_| EngineRunError::internal_contract("join container kind failed over a valid node"))?;
    let component = match kind {
        ValueKind::Array => jqf_builtins::semantics::path::PathStep::Index(
            i64::try_from(cursor).map_err(|_| EngineRunError::allocation_failure())?,
        ),
        ValueKind::Object => {
            let Some(object) = view
                .object()
                .map_err(|_| EngineRunError::internal_contract("join container object failed over a valid node"))?
            else {
                return Ok(None);
            };
            let Some(entry) = object
                .get_index(cursor)
                .map_err(|_| EngineRunError::internal_contract("join container entry failed over a valid node"))?
            else {
                return Ok(None);
            };
            jqf_builtins::semantics::path::PathStep::Key(jqf_builtins::semantics::path::charged_key(
                entry.key(),
                resources,
            )?)
        }
        _ => return Ok(None),
    };
    Ok(Some(component))
}

/// The `PathStep`s one runtime path array names (the `getpath` follow's
/// vocabulary half).
pub(crate) fn follow_steps(path: &Value, resources: &ResourceContext<'_>) -> Result<Vec<PathStep>, EngineRunError> {
    let Value::Array(components) = path.untagged() else {
        return Err(jqf_builtins::semantics::path::raise(
            "Path must be specified as an array",
            resources,
        ));
    };
    let mut steps = Vec::new();
    for component in components {
        let step = match component.untagged() {
            Value::String(name) => PathStep::Key(name.clone_shared()),
            Value::Number(_) => number_component(component)?,
            Value::Object(bounds) => PathStep::Slice {
                start: bounds.get(jqf_builtins::semantics::path::SLICE_START).cloned(),
                end: bounds.get(jqf_builtins::semantics::path::SLICE_END).cloned(),
            },
            Value::Array(needle) => PathStep::Subsequence(Value::Array(needle.clone())),
            _ => {
                return Err(jqf_builtins::semantics::path::index_error(
                    &Value::Null,
                    component,
                    resources,
                ));
            }
        };
        steps.push(step);
    }
    Ok(steps)
}

/// The frozen argument law's drive: a filter evaluated on a NESTED machine
/// with NO register (frozen = outputs are values, never locations), collected
/// eagerly. The nested drive is the one evaluator — the same machine the
/// enclosing evaluation runs on — and it replenishes its cooperative entry on
/// a pause, because the eager two-evaluator drive it replaces never yielded
/// (the argument's work is bounded by the standing recursion ceilings either
/// way).
///
/// The PREFIX-KEEPING form: an error mid-argument returns the outputs
/// collected so far beside the error, so a caller's product loop can publish
/// the combinations already computed before raising — the behavior for
/// `splits("X"; ("g", error("z")))` and its kin.
///
/// `topk` threads the parent program's partial-sort table so a sort-slice
/// shape inside one of these arguments takes the bounded-heap route. The
/// scan/anti-join tables stay EMPTY at the seed below, deliberately: their
/// warm-up is runtime state built across REPEATED scans of one field, which
/// a per-call argument machine never reaches, and pooled argument machines
/// keep their ORIGINAL seed's tables across `reseed`. Do not thread them
/// symmetrically with topk; see `take_arg_machine` for the full argument.
pub(crate) fn evaluate_owned_filter_prefix(
    nodes: &[ProgramNode],
    resources: &mut ResourceContext<'_>,
    slots: Vec<Option<Value>>,
    filter: ProgramNodeId,
    input: Value,
    topk: &[PartialSort],
    fact_deltas: &mut Vec<FactDelta>,
) -> Result<(Vec<Value>, Option<EngineRunError>), EngineRunError> {
    let mut machine = GraphMachine::seed(
        nodes,
        filter,
        filter,
        0,
        u32::try_from(slots.len()).map_err(|_| EngineRunError::internal_contract("argument slot count overflow"))?,
        // Scans/anti intentionally empty — see the doc above.
        &[],
        topk,
        &[],
        EngineResult::owned(input),
    )?;
    machine.fact_deltas.clone_from(fact_deltas);
    for (index, slot) in slots.into_iter().enumerate() {
        if let Some(value) = slot {
            machine.write_slot(
                u32::try_from(index).map_err(|_| EngineRunError::internal_contract("argument slot overflow"))?,
                EngineResult::owned(value),
            )?;
        }
    }
    let result = drain_owned_filter(&mut machine, resources);
    fact_deltas.clone_from(&machine.fact_deltas);
    result
}

/// Drives `machine` to completion, collecting owned outputs. A mid-drive
/// error returns the prefix beside the error (the frozen-argument
/// prefix-keep law).
pub(crate) fn drain_owned_filter(
    machine: &mut GraphMachine<'_, '_>,
    resources: &mut ResourceContext<'_>,
) -> Result<(Vec<Value>, Option<EngineRunError>), EngineRunError> {
    let mut out = Vec::new();
    loop {
        match machine.poll(resources) {
            Ok(RunPoll::Item(EngineResult::Owned(value))) => {
                out.try_reserve(1).map_err(|_| EngineRunError::allocation_failure())?;
                out.push(value);
            }
            Ok(RunPoll::Item(EngineResult::Located(_))) => {
                return Err(EngineRunError::internal_contract(
                    "an argument drive published a located value",
                ));
            }
            Ok(RunPoll::Complete) => return Ok((out, None)),
            Ok(RunPoll::Pending) => {
                resources
                    .try_begin_next_cooperative_entry(4_096)
                    .map_err(|error| EngineRunError::Codec(jqf_codec_core::CodecError::from(error)))?;
            }
            Err(error) => return Ok((out, Some(error))),
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the frozen argument-machine seed: program, resources, slots, filter, input, topk, and fact deltas are each an independent channel the machine needs at once"
)]
pub(crate) fn evaluate_owned_filter(
    nodes: &[ProgramNode],
    resources: &mut ResourceContext<'_>,
    slots: Vec<Option<Value>>,
    filter: ProgramNodeId,
    input: Value,
    topk: &[PartialSort],
    fact_deltas: &mut Vec<FactDelta>,
) -> Result<Vec<Value>, EngineRunError> {
    let (values, pending) = evaluate_owned_filter_prefix(nodes, resources, slots, filter, input, topk, fact_deltas)?;
    if let Some(error) = pending {
        return Err(error);
    }
    Ok(values)
}

/// Whether a frame's whole life IS a frozen subexpression drive, so its
/// normal-completion pop ends the frozen scope (a collect/object/argument
/// frame). `BindBody`, the binary/concat operands, `ConditionalBody`, and
/// `SelectBody` are consumer-exit frames instead: their scope ends when the
/// consumer consumes an output, and their normal pop must NOT exit again.
pub(super) fn frozen_pop_frame(frame: &crate::exec::GraphFrame<'_, '_>) -> bool {
    matches!(
        frame,
        crate::exec::GraphFrame::Collect { .. }
            | crate::exec::GraphFrame::CountCollect { .. }
            | crate::exec::GraphFrame::ObjectDest { .. }
            | crate::exec::GraphFrame::ObjectKey { .. }
            | crate::exec::GraphFrame::ObjectValue { .. }
            | crate::exec::GraphFrame::ArgumentAnswer { .. }
            | crate::exec::GraphFrame::MathArgs { .. }
            | crate::exec::GraphFrame::MathCompute { .. }
            | crate::exec::GraphFrame::PathSetValue { .. }
            | crate::exec::GraphFrame::PathSetPair { .. }
            | crate::exec::GraphFrame::Walk { .. }
            | crate::exec::GraphFrame::MapValues { .. }
            | crate::exec::GraphFrame::KeyedCollect { .. }
            | crate::exec::GraphFrame::KeyedStreamSource { .. }
            | crate::exec::GraphFrame::KeyedStreamKey { .. }
            | crate::exec::GraphFrame::KeyedCollectSource { .. }
            | crate::exec::GraphFrame::KeyedCollectKey { .. }
            | crate::exec::GraphFrame::ErrorArg { .. }
            | crate::exec::GraphFrame::ConditionalBody { .. }
            | crate::exec::GraphFrame::SelectBody { .. }
            | crate::exec::GraphFrame::Generate {
                state: crate::exec::GenerateState::RangeBound { .. }
                    | crate::exec::GenerateState::LoopCond { .. }
                    | crate::exec::GenerateState::LoopUpdate { .. }
                    | crate::exec::GenerateState::CombinationsWidth { .. },
            }
    )
}

/// Whether a frame is a frozen-position frame whose drive the raise walk (or a
/// cut) abandons mid-flight: the pop-exit set plus the consumer-exit frames.
pub(super) fn frozen_abandon_frame(frame: &crate::exec::GraphFrame<'_, '_>) -> bool {
    matches!(
        frame,
        crate::exec::GraphFrame::BindBody { .. }
            | crate::exec::GraphFrame::BinaryLeft { .. }
            | crate::exec::GraphFrame::BinaryRight { .. }
            | crate::exec::GraphFrame::ConcatPart { .. }
    ) || frozen_pop_frame(frame)
}
